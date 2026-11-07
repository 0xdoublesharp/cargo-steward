//! Reads cargo build directories, in both layouts.
//!
//! Before Cargo 1.100 ("legacy"), a profile directory holds every unit's
//! files side by side, told apart by a 16-hex-digit hash in their names:
//!
//! ```text
//! debug/.fingerprint/serde-0123456789abcdef/
//! debug/deps/libserde-0123456789abcdef.rlib
//! debug/deps/app-fedcba9876543210.<cgu>.<invocation>.rcgu.o
//! debug/build/serde-0123456789abcdef/          build script
//! debug/incremental/app-<id>/
//! ```
//!
//! From Cargo 1.100 ("v2"), each unit has its own directory:
//!
//! ```text
//! debug/build/serde/0123456789abcdef/fingerprint/
//! debug/build/serde/0123456789abcdef/out/libserde.rlib
//! debug/incremental/app-<id>/
//! ```
//!
//! A build directory upgraded in place holds both until the old files are
//! removed. The root of a build directory is found by the `CACHEDIR.TAG` cargo
//! writes there.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::fsutil::{entries, freed_by, subdirs, touched};

/// Suffixes of the codegen-unit object files that unpacked debug info keeps:
/// `.o` on macOS, `.dwo` on ELF platforms.
pub const OBJECT_SUFFIXES: [&str; 2] = [".rcgu.o", ".rcgu.dwo"];

pub fn is_object(name: &str) -> bool {
    OBJECT_SUFFIXES.iter().any(|s| name.ends_with(s))
}

fn strip_object_suffix(name: &str) -> Option<&str> {
    OBJECT_SUFFIXES.iter().find_map(|s| name.strip_suffix(s))
}

/// Whether `dir` is the root of a cargo build or target directory.
///
/// Cargo caches `rustc -vV` in `.rustc_info.json` (`.rustc-info.json` in the
/// new layout) at the root and nowhere else, whoever created the directory.
/// `CACHEDIR.TAG` is only written when cargo creates a directory itself, so a
/// target dir a script made beforehand has none, while per-target
/// subdirectories (`target/wasm32-unknown-unknown/`) get their own. Cargo
/// also tags its download caches (`$CARGO_HOME/registry`), so a tag alone
/// only counts when the directory holds build profiles.
pub fn is_cargo_root(dir: &Path) -> bool {
    has_rustc_info(dir) || (has_cargo_tag(dir) && !profiles(dir).is_empty())
}

fn has_rustc_info(dir: &Path) -> bool {
    [".rustc_info.json", ".rustc-info.json"]
        .iter()
        .any(|name| dir.join(name).is_file())
}

fn has_cargo_tag(dir: &Path) -> bool {
    fs::read_to_string(dir.join("CACHEDIR.TAG")).is_ok_and(|t| t.contains("created by cargo"))
}

/// The build directory containing `path`: the nearest ancestor holding
/// cargo's rustc cache, else the outermost one tagged by cargo.
pub fn find_root(path: &Path) -> Option<PathBuf> {
    let nearby: Vec<&Path> = path.ancestors().take(8).collect();
    if let Some(root) = nearby.iter().find(|d| has_rustc_info(d)) {
        return Some(root.to_path_buf());
    }
    nearby
        .iter()
        .rev()
        .find(|d| has_cargo_tag(d))
        .map(|d| d.to_path_buf())
}

/// Drops roots nested inside another root in the list.
pub fn outermost(mut roots: Vec<PathBuf>) -> Vec<PathBuf> {
    roots.sort();
    roots.dedup();
    let mut kept: Vec<PathBuf> = Vec::new();
    for root in roots {
        if !kept.iter().any(|outer| root.starts_with(outer)) {
            kept.push(root);
        }
    }
    kept
}

/// Directory names never searched for build directories.
const SKIP_DIRS: [&str; 8] = [
    ".git",
    "node_modules",
    ".Trashes",
    ".Spotlight-V100",
    ".fseventsd",
    ".DocumentRevisions-V100",
    ".TemporaryItems",
    ".pnpm-store",
];

/// Build directories under each of `roots`, at most `max_depth` levels down.
/// Build directories are not searched inside.
pub fn discover(roots: &[PathBuf], max_depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = roots.iter().map(|r| (r.clone(), 0)).collect();
    while let Some((dir, depth)) = stack.pop() {
        if is_cargo_root(&dir) {
            found.push(dir);
            continue;
        }
        // Other directories cargo tags are its caches, such as a registry's
        // thousands of unpacked crates: nothing to find in there.
        if depth >= max_depth || has_cargo_tag(&dir) {
            continue;
        }
        for (name, path) in entries(&dir) {
            if SKIP_DIRS.contains(&name.as_str()) || name.starts_with(crate::fsutil::TRASH_PREFIX) {
                continue;
            }
            if fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
                stack.push((path, depth + 1));
            }
        }
    }
    outermost(found)
}

/// A profile directory, such as `target/debug` or
/// `target/aarch64-apple-darwin/release`.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub path: PathBuf,
    /// Holds old-layout units.
    pub legacy: bool,
    /// Holds new-layout units.
    pub v2: bool,
}

/// Profile directories in a build directory: its children, or with
/// `--target`, its grandchildren.
pub fn profiles(root: &Path) -> Vec<Profile> {
    let mut found = Vec::new();
    for child in subdirs(root).filter(|c| !is_trash(c)) {
        if let Some(profile) = as_profile(&child) {
            found.push(profile);
            continue;
        }
        found.extend(
            subdirs(&child)
                .filter(|g| !is_trash(g))
                .filter_map(|g| as_profile(&g)),
        );
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

/// Renamed aside for deletion ([`crate::fsutil::remove`]).
fn is_trash(dir: &Path) -> bool {
    dir.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(crate::fsutil::TRASH_PREFIX))
}

fn as_profile(dir: &Path) -> Option<Profile> {
    let legacy = dir.join(".fingerprint").is_dir();
    let v2 = has_v2_units(dir);
    let marked = [".cargo-lock", ".cargo-build-lock", "incremental"]
        .iter()
        .any(|m| dir.join(m).exists());
    (legacy || v2 || marked).then(|| Profile {
        path: dir.to_path_buf(),
        legacy,
        v2,
    })
}

fn has_v2_units(profile: &Path) -> bool {
    subdirs(&profile.join("build"))
        .take(64)
        .any(|pkg| subdirs(&pkg).take(8).any(|unit| is_v2_unit(&unit)))
}

fn is_v2_unit(dir: &Path) -> bool {
    dir.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(is_hash)
        && dir.join("fingerprint").is_dir()
}

/// Cargo's unit hash: 16 lowercase hex digits.
pub fn is_hash(s: &str) -> bool {
    s.len() == 16
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A directory removed as a whole, and when a build last used it.
#[derive(Debug, Clone)]
pub struct Dated {
    pub path: PathBuf,
    pub last_used: SystemTime,
}

/// New-layout units: `build/<package>/<hash>/`, last used when cargo last
/// read or wrote their fingerprint.
pub fn v2_units(profile: &Path) -> Vec<Dated> {
    let mut units = Vec::new();
    for pkg in subdirs(&profile.join("build")) {
        for unit in subdirs(&pkg) {
            if !is_v2_unit(&unit) {
                continue;
            }
            let dir_time = fs::metadata(&unit).map_or(SystemTime::UNIX_EPOCH, |m| touched(&m));
            let last_used =
                newest_in(&unit.join("fingerprint")).map_or(dir_time, |t| t.max(dir_time));
            units.push(Dated {
                path: unit,
                last_used,
            });
        }
    }
    units
}

/// A new-layout unit and what its fingerprints say about it: the hashes
/// cargo records for it, and those of the units it depends on.
#[derive(Debug, Clone)]
pub struct UnitNode {
    pub unit: Dated,
    /// Its own fingerprint hashes, one per target kind built in it.
    pub hashes: Vec<u64>,
    /// The fingerprint hashes of the units it depends on.
    pub deps: Vec<u64>,
    /// Every fingerprint record in it parsed. When not, its dependencies are
    /// unknown.
    pub readable: bool,
}

/// [`v2_units`] with their fingerprints read. Cargo keeps a unit's hash in
/// `fingerprint/<kind>-<target>` as the little-endian hex of a `u64`, and
/// its dependencies' hashes as the fourth field of each `deps` entry in
/// `fingerprint/<kind>-<target>.json`; together they link each unit to the
/// units it was built from, build scripts and build-dependencies included.
pub fn v2_unit_nodes(profile: &Path) -> Vec<UnitNode> {
    v2_units(profile)
        .into_iter()
        .map(|unit| {
            let mut node = UnitNode {
                hashes: Vec::new(),
                deps: Vec::new(),
                readable: true,
                unit,
            };
            for (name, path) in entries(&node.unit.path.join("fingerprint")) {
                if name.ends_with(".json") {
                    match fingerprint_deps(&path) {
                        Some(deps) => node.deps.extend(deps),
                        None => node.readable = false,
                    }
                } else if !name.starts_with("dep-")
                    && !name.starts_with("output-")
                    && name != "invoked.timestamp"
                    && let Some(hash) = fs::read_to_string(&path)
                        .ok()
                        .and_then(|text| fingerprint_hash(text.trim()))
                {
                    node.hashes.push(hash);
                }
            }
            node
        })
        .collect()
}

/// A fingerprint hash file's contents: 16 hex digits of a little-endian u64.
fn fingerprint_hash(text: &str) -> Option<u64> {
    if text.len() != 16 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0u8; 8];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(u64::from_le_bytes(bytes))
}

/// The dependency hashes in a fingerprint record; `None` if it doesn't parse.
fn fingerprint_deps(path: &Path) -> Option<Vec<u64>> {
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let deps = value.get("deps")?.as_array()?;
    deps.iter()
        .map(|dep| dep.as_array()?.get(3)?.as_u64())
        .collect()
}

/// Incremental caches: `incremental/<crate>-<id>/`, each holding session
/// directories that rustc replaces on every compile. A cache is removed whole;
/// removing part of a session can crash rustc.
pub fn incremental_dirs(profile: &Path) -> Vec<Dated> {
    subdirs(&profile.join("incremental"))
        .map(|dir| {
            let mut last_used = fs::metadata(&dir).map_or(SystemTime::UNIX_EPOCH, |m| touched(&m));
            for session in subdirs(&dir) {
                if let Ok(meta) = fs::metadata(&session) {
                    last_used = last_used.max(touched(&meta));
                }
                if let Some(t) = newest_in(&session) {
                    last_used = last_used.max(t);
                }
            }
            Dated {
                path: dir,
                last_used,
            }
        })
        .collect()
}

/// Finalized incremental sessions that a newer finalized session of the same
/// cache has replaced, with their `.lock` files.
///
/// rustc only ever loads a cache's newest finalized session
/// (`s-<time>-<random>-<hash>/`), and deletes the older ones itself the next
/// time that crate compiles; until then they are dead weight, often as big
/// as the live session. Each is removed whole, as rustc would. In-progress
/// sessions (`...-working`) are never included.
pub fn superseded_sessions(cache: &Path) -> Vec<PathBuf> {
    let mut finalized: Vec<(SystemTime, String, PathBuf)> = entries(cache)
        .filter(|(name, path)| {
            name.starts_with("s-") && !name.ends_with("-working") && path.is_dir()
        })
        .filter_map(|(name, path)| {
            let modified = fs::metadata(&path).and_then(|m| m.modified()).ok()?;
            Some((modified, name, path))
        })
        .collect();
    if finalized.len() < 2 {
        return Vec::new();
    }
    finalized.sort();
    let newest = finalized.last().map(|(t, _, _)| *t);
    let mut out = Vec::new();
    for (modified, name, path) in finalized {
        if Some(modified) == newest {
            continue;
        }
        // `s-<time>-<random>-<hash>` locks as `s-<time>-<random>.lock`.
        let parts: Vec<&str> = name.splitn(4, '-').collect();
        if let [s, time, random, _] = parts.as_slice() {
            let lock = cache.join(format!("{s}-{time}-{random}.lock"));
            if lock.is_file() {
                out.push(lock);
            }
        }
        out.push(path);
    }
    out
}

/// When an old-layout build last used this profile.
pub fn legacy_last_used(profile: &Path) -> Option<SystemTime> {
    let mut newest = None;
    for unit in subdirs(&profile.join(".fingerprint")) {
        let dir_time = fs::metadata(&unit).ok().map(|m| touched(&m));
        for t in [dir_time, newest_in(&unit)].into_iter().flatten() {
            newest = Some(newest.map_or(t, |n: SystemTime| n.max(t)));
        }
    }
    newest
}

/// The old-layout files of a profile: `.fingerprint/`, `deps/`, and the
/// `build/<package>-<hash>/` build-script directories. Final outputs copied
/// up into the profile and `examples/` are shared with the new layout and
/// stay.
pub fn legacy_paths(profile: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = [".fingerprint", "deps"]
        .iter()
        .map(|d| profile.join(d))
        .filter(|p| p.exists())
        .collect();
    paths.extend(subdirs(&profile.join("build")).filter(|dir| is_legacy_unit_dir(dir)));
    paths
}

fn is_legacy_unit_dir(dir: &Path) -> bool {
    dir.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.rsplit_once('-'))
        .is_some_and(|(_, hash)| is_hash(hash))
}

/// Newest [`touched`] time of the entries directly in `dir`.
fn newest_in(dir: &Path) -> Option<SystemTime> {
    entries(dir)
        .filter_map(|(_, p)| fs::symlink_metadata(&p).ok())
        .map(|m| touched(&m))
        .max()
}

/// Object files left behind by earlier compiles of one unit.
#[derive(Debug, Clone)]
pub struct StaleObjects {
    /// The unit, e.g. `app-fedcba9876543210` (legacy) or `app` (v2).
    pub unit: String,
    pub files: Vec<PathBuf>,
    /// Space removing them gives back.
    pub bytes: u64,
}

/// Directories of a profile that hold codegen-unit object files.
pub fn object_dirs(profile: &Profile) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if profile.legacy {
        dirs.push(profile.path.join("deps"));
        dirs.push(profile.path.join("examples"));
        dirs.extend(subdirs(&profile.path.join("build")).filter(|d| is_legacy_unit_dir(d)));
    }
    if profile.v2 {
        for pkg in subdirs(&profile.path.join("build")) {
            dirs.extend(
                subdirs(&pkg)
                    .filter(|u| is_v2_unit(u))
                    .map(|u| u.join("out")),
            );
        }
    }
    dirs
}

/// Stale object files in one directory.
///
/// Since Rust 1.88, every incremental compile names its codegen-unit objects
/// `<unit>.<cgu>.<invocation>.rcgu.o` with a fresh `<invocation>`, renaming
/// the objects it reuses too, and nothing removes the previous set
/// (rust-lang/rust#161824). For each unit with more than one invocation's
/// objects, this keeps the invocations its outputs still name (a binary's
/// debug map lists its objects; an rlib holds them as members) and returns
/// the rest. When no output names any, the newest invocation is kept.
///
/// An invocation whose newest object was modified at or after `keep_after`
/// is always kept: a binary linked from it may still be running, and needs
/// it for symbolized backtraces (`object-grace`).
///
/// Non-incremental objects (`...-cgu.<n>.rcgu.o`) are overwritten in place by
/// each compile and are left alone.
pub fn stale_objects_in(dir: &Path, keep_after: SystemTime) -> Vec<StaleObjects> {
    struct Object {
        name: String,
        path: PathBuf,
        /// Read only for units with more than one invocation.
        modified: SystemTime,
    }
    let mut units: BTreeMap<String, BTreeMap<String, Vec<Object>>> = BTreeMap::new();
    let mut others: Vec<(String, PathBuf)> = Vec::new();
    for (name, path) in entries(dir) {
        let Some(stem) = strip_object_suffix(&name) else {
            others.push((name, path));
            continue;
        };
        let Some((head, invocation)) = stem.rsplit_once('.') else {
            continue;
        };
        if head.ends_with("-cgu") {
            continue;
        }
        let unit = head
            .split_once('.')
            .map_or(head, |(unit, _)| unit)
            .to_owned();
        units
            .entry(unit)
            .or_default()
            .entry(invocation.to_owned())
            .or_default()
            .push(Object {
                name,
                path,
                modified: SystemTime::UNIX_EPOCH,
            });
    }

    let newest = |objects: &Vec<Object>| objects.iter().map(|o| o.modified).max();
    let mut stale = Vec::new();
    for (unit, mut invocations) in units {
        if invocations.len() < 2 {
            continue;
        }
        // Only now, for the few units with leftovers: a big `deps/` holds
        // hundreds of thousands of objects.
        for objects in invocations.values_mut() {
            objects.retain_mut(|o| match fs::symlink_metadata(&o.path) {
                Ok(meta) => {
                    o.modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    true
                }
                Err(_) => false,
            });
        }
        invocations.retain(|_, objects| !objects.is_empty());
        if invocations.len() < 2 {
            continue;
        }
        let recent: HashSet<&str> = invocations
            .iter()
            .filter(|(_, objects)| newest(objects).is_some_and(|t| t >= keep_after))
            .map(|(invocation, _)| invocation.as_str())
            .collect();
        if recent.len() == invocations.len() {
            // Nothing old enough to remove: no need to read the outputs.
            continue;
        }
        // Compared without the suffix, so a binary or rlib naming
        // `x.rcgu.o` keeps the same compile's `x.rcgu.dwo` too.
        let named: HashSet<String> = others
            .iter()
            .filter(|(name, path)| {
                name.contains(&unit)
                    && !name.ends_with(".d")
                    && !name.ends_with(".rmeta")
                    && path.is_file()
            })
            .flat_map(|(_, path)| named_objects(path))
            .filter_map(|name| strip_object_suffix(&name).map(str::to_owned))
            .collect();
        let mut keep: HashSet<&str> = invocations
            .iter()
            .filter(|(_, objects)| {
                objects
                    .iter()
                    .any(|o| strip_object_suffix(&o.name).is_some_and(|s| named.contains(s)))
            })
            .map(|(invocation, _)| invocation.as_str())
            .collect();
        if keep.is_empty() {
            let latest = invocations.values().filter_map(newest).max();
            keep = invocations
                .iter()
                .filter(|(_, objects)| newest(objects) == latest)
                .map(|(invocation, _)| invocation.as_str())
                .collect();
        }
        keep.extend(recent);
        let files: Vec<PathBuf> = invocations
            .iter()
            .filter(|(invocation, _)| !keep.contains(invocation.as_str()))
            .flat_map(|(_, objects)| objects.iter().map(|o| o.path.clone()))
            .collect();
        if !files.is_empty() {
            let bytes = freed_by(&files);
            stale.push(StaleObjects { unit, files, bytes });
        }
    }
    stale
}

/// Names of the codegen-unit object files that `output` mentions, such as
/// `/…/deps/app-0123456789abcdef.0a1b.1x2y.rcgu.o` in a debug map or an
/// rlib member name.
///
/// Outputs are big (rlibs and binaries run to hundreds of megabytes), so
/// where the format is known only the part that can name objects is read:
/// an archive's member headers, or a 64-bit Mach-O file's string table,
/// which holds its debug map. Anything else, or anything that doesn't parse,
/// is searched in full.
pub fn named_objects(output: &Path) -> HashSet<String> {
    let mut names = HashSet::new();
    let Ok(file) = File::open(output) else {
        return names;
    };
    let len = file.metadata().map_or(u64::MAX, |m| m.len());
    let mut file = BufReader::with_capacity(HEADER_BUFFER, file);
    let mut magic = [0u8; 8];
    let head = read_up_to(&mut file, &mut magic);
    let parsed = if head == magic.len() && magic == *ARCHIVE_MAGIC {
        if len <= WHOLE_ARCHIVE {
            // One sequential read costs less than a seek per member.
            let mut contents = Vec::with_capacity(len as usize);
            file.read_to_end(&mut contents).is_ok()
                && archive_names(&mut io::Cursor::new(contents), &mut names)
        } else {
            archive_names(&mut file, &mut names)
        }
    } else if head >= 4 && magic[..4] == MH_MAGIC_64.to_le_bytes() {
        macho_names(&mut file, &mut names)
    } else {
        false
    };
    if !parsed {
        names.clear();
        if file.seek(SeekFrom::Start(0)).is_ok() {
            search_names(&mut file, &mut names);
        }
    }
    names
}

/// Read buffer for headers. Small: a big archive's headers are read one by
/// one with seeks in between, so a bigger buffer would read most of each
/// member anyway. Bulk reads bypass it.
const HEADER_BUFFER: usize = 4096;
/// Archives up to this size are read whole; bigger ones header by header.
const WHOLE_ARCHIVE: u64 = 4 << 20;
const ARCHIVE_MAGIC: &[u8; 8] = b"!<arch>\n";
const MH_MAGIC_64: u32 = 0xfeed_facf;
const LC_SYMTAB: u32 = 0x2;

/// Collects the object names among an archive's member names (BSD `#1/<n>`
/// long names, GNU `//` name tables, and plain 16-byte names), skipping
/// over the members' contents. `false` if it isn't a well-formed archive.
fn archive_names(file: &mut (impl Read + Seek), names: &mut HashSet<String>) -> bool {
    let mut text = Vec::new();
    loop {
        let mut header = [0u8; 60];
        match read_up_to(file, &mut header) {
            0 => break,
            60 => {}
            _ => return false,
        }
        if &header[58..] != b"`\n" {
            return false;
        }
        let Some(size) = decimal(&header[48..58]) else {
            return false;
        };
        let name = &header[..16];
        let mut consumed = 0;
        if let Some(len) = name.strip_prefix(b"#1/") {
            let Some(len) = decimal(len).filter(|&n| n <= size && n <= 4096) else {
                return false;
            };
            let start = text.len();
            text.resize(start + len as usize, 0);
            if file.read_exact(&mut text[start..]).is_err() {
                return false;
            }
            consumed = len;
        } else if name.starts_with(b"// ") {
            let Some(len) = usize::try_from(size).ok().filter(|&n| n <= 64 << 20) else {
                return false;
            };
            let start = text.len();
            text.resize(start + len, 0);
            if file.read_exact(&mut text[start..]).is_err() {
                return false;
            }
            consumed = size;
        } else {
            text.extend_from_slice(name);
        }
        text.push(0);
        // Members start at even offsets.
        let skip = size - consumed + (size & 1);
        let Ok(skip) = i64::try_from(skip) else {
            return false;
        };
        if file.seek(SeekFrom::Current(skip)).is_err() {
            return false;
        }
    }
    find_names(&text, false, names);
    true
}

/// Collects the object names in a little-endian 64-bit Mach-O file's string
/// table, where its debug map (`N_OSO` entries) keeps the paths of the
/// objects it was linked from. `false` if the file doesn't parse as one.
fn macho_names(file: &mut BufReader<File>, names: &mut HashSet<String>) -> bool {
    let u32_at = |buf: &[u8], at: usize| -> Option<u32> {
        let bytes = buf.get(at..at + 4)?;
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };
    // The 32-byte header; its first 8 bytes are already read.
    let mut header = [0u8; 24];
    if file.read_exact(&mut header).is_err() {
        return false;
    }
    let (Some(count), Some(size)) = (u32_at(&header, 8), u32_at(&header, 12)) else {
        return false;
    };
    if size > 16 << 20 {
        return false;
    }
    let mut commands = vec![0u8; size as usize];
    if file.read_exact(&mut commands).is_err() {
        return false;
    }
    let mut at = 0usize;
    let mut table = None;
    for _ in 0..count {
        let (Some(cmd), Some(cmd_size)) = (u32_at(&commands, at), u32_at(&commands, at + 4)) else {
            return false;
        };
        if cmd == LC_SYMTAB {
            table = u32_at(&commands, at + 16).zip(u32_at(&commands, at + 20));
            break;
        }
        if cmd_size < 8 {
            return false;
        }
        at += cmd_size as usize;
    }
    let Some((offset, size)) = table else {
        return false;
    };
    if file.seek(SeekFrom::Start(offset.into())).is_err() {
        return false;
    }
    search_names(&mut Read::by_ref(file).take(size.into()), names);
    true
}

/// Searches everything `reader` yields for object names.
fn search_names(reader: &mut impl Read, names: &mut HashSet<String>) {
    const CHUNK: usize = 1 << 20;
    // Carried between reads so a name split across two is still found.
    // File names are at most 255 bytes.
    const OVERLAP: usize = 256;
    let mut buf: Vec<u8> = Vec::with_capacity(OVERLAP + CHUNK);
    loop {
        let kept = buf.len();
        buf.resize(kept + CHUNK, 0);
        let read = match reader.read(&mut buf[kept..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.truncate(kept + read);
        find_names(&buf, kept > 0, names);
        buf.drain(..buf.len().saturating_sub(OVERLAP));
    }
}

fn is_name_byte(b: &u8) -> bool {
    b.is_ascii_alphanumeric() || b"._-".contains(b)
}

/// Adds every object file name in `buf`: a run of name bytes ending in one
/// of [`OBJECT_SUFFIXES`] and not followed by more (`x.rcgu.o.tmp` isn't
/// one).
///
/// `carried`: `buf` starts with the end of the previous read, so a name
/// reaching back to its first byte may have begun before it. Such a name
/// was already found whole in that read (names are shorter than the bytes
/// carried over), and is skipped rather than recorded cut short.
fn find_names(buf: &[u8], carried: bool, names: &mut HashSet<String>) {
    const MARK: &[u8] = b".rcgu.";
    let mut at = 0;
    while let Some(dot) = buf[at..].iter().position(|&b| b == b'.') {
        let start_of_mark = at + dot;
        at = start_of_mark + 1;
        let Some(rest) = buf[start_of_mark..].strip_prefix(MARK) else {
            continue;
        };
        let tail = if rest.starts_with(b"dwo") {
            3
        } else if rest.starts_with(b"o") {
            1
        } else {
            continue;
        };
        let end = start_of_mark + MARK.len() + tail;
        if buf.get(end).is_some_and(is_name_byte) {
            continue;
        }
        let start = buf[..start_of_mark]
            .iter()
            .rposition(|b| !is_name_byte(b))
            .map_or(0, |i| i + 1);
        if start == 0 && carried {
            at = end;
            continue;
        }
        names.insert(String::from_utf8_lossy(&buf[start..end]).into_owned());
        at = end;
    }
}

/// Reads until `buf` is full or the input ends; how much was read.
fn read_up_to(reader: &mut impl Read, buf: &mut [u8]) -> usize {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    filled
}

/// An archive header's space-padded decimal field.
fn decimal(field: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(field)
        .ok()?
        .trim_end_matches([' ', '\0']);
    text.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::FileTimes;
    use std::time::Duration;

    fn write(path: &Path, contents: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn obj(unit: &str, cgu: &str, invocation: &str) -> String {
        format!("{unit}.{cgu}.{invocation}.rcgu.o")
    }

    /// A `keep_after` no object reaches: no grace.
    fn no_grace() -> SystemTime {
        SystemTime::now() + Duration::from_secs(3600)
    }

    fn set_modified(path: &Path, time: SystemTime) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(time))
            .unwrap();
    }

    /// An archive in the BSD format macOS rlibs use: `#1/<n>` names stored
    /// before each member's contents.
    fn bsd_archive(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = b"!<arch>\n".to_vec();
        for (name, data) in members {
            let size = name.len() + data.len();
            out.extend(
                format!(
                    "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                    format!("#1/{}", name.len()),
                    0,
                    0,
                    0,
                    644,
                    size
                )
                .as_bytes(),
            );
            out.extend(name.as_bytes());
            out.extend(*data);
            if size % 2 == 1 {
                out.push(b'\n');
            }
        }
        out
    }

    /// An archive in the GNU format Linux rlibs use: short names end in
    /// `/`, long ones are offsets into a `//` table.
    fn gnu_archive(table: &str, members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = b"!<arch>\n".to_vec();
        let mut add = |name: &str, data: &[u8]| {
            out.extend(
                format!(
                    "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                    0,
                    0,
                    0,
                    644,
                    data.len()
                )
                .as_bytes(),
            );
            out.extend(data);
            if data.len() % 2 == 1 {
                out.push(b'\n');
            }
        };
        add("//", table.as_bytes());
        for (name, data) in members {
            add(name, data);
        }
        out
    }

    /// A minimal 64-bit Mach-O file: a header, an `LC_SYMTAB` command, code
    /// that happens to contain an object's name, and a string table.
    fn macho(code: &[u8], strings: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let header_and_commands = 32 + 8 + 24;
        let stroff = (header_and_commands + code.len()) as u32;
        for word in [MH_MAGIC_64, 0x0100_000c, 0, 2, 2, 8 + 24, 0, 0] {
            out.extend(word.to_le_bytes());
        }
        // An unrelated command, then the symbol table's.
        out.extend(0x19u32.to_le_bytes());
        out.extend(8u32.to_le_bytes());
        for word in [LC_SYMTAB, 24, 0, 0, stroff, strings.len() as u32] {
            out.extend(word.to_le_bytes());
        }
        out.extend(code);
        out.extend(strings);
        out
    }

    fn names(stale: &[StaleObjects]) -> Vec<(String, Vec<String>)> {
        let mut out: Vec<(String, Vec<String>)> = stale
            .iter()
            .map(|s| {
                let mut files: Vec<String> = s
                    .files
                    .iter()
                    .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                    .collect();
                files.sort();
                (s.unit.clone(), files)
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn keeps_objects_outputs_name_and_returns_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let deps = tmp.path().join("deps");
        // A library compiled twice; its rlib holds the second compile's objects.
        let lib = "dep-0123456789abcdef";
        for invocation in ["0aaaaaa", "0bbbbbb"] {
            for cgu in ["c1", "c2"] {
                write(&deps.join(obj(lib, cgu, invocation)), b"o");
            }
        }
        let mut rlib = b"!<arch>\n#1/40           ".to_vec();
        rlib.extend(obj(lib, "c1", "0bbbbbb").as_bytes());
        rlib.extend(b"\0\0\n");
        rlib.extend(obj(lib, "c2", "0bbbbbb").as_bytes());
        write(&deps.join(format!("lib{lib}.rlib")), &rlib);
        // `.d` files mention old objects too and must not count.
        write(
            &deps.join(format!("{lib}.d")),
            obj(lib, "c1", "0aaaaaa").as_bytes(),
        );

        // A binary whose debug map names its objects by full path.
        let bin = "app-fedcba9876543210";
        for invocation in ["0cccccc", "0dddddd"] {
            write(&deps.join(obj(bin, "main", invocation)), b"o");
        }
        let current = deps.join(obj(bin, "main", "0dddddd"));
        write(
            &deps.join(bin),
            format!("\0{}\0", current.display()).as_bytes(),
        );

        // Non-incremental objects are rewritten in place: never stale.
        for n in ["0", "1"] {
            write(
                &deps.join(obj("num-00112233445566ff", "num.0123-cgu", n)),
                b"o",
            );
        }
        // Compiled once.
        write(&deps.join(obj("once-ffeeddccbbaa0099", "x", "0ggg")), b"o");

        assert_eq!(
            names(&stale_objects_in(&deps, no_grace())),
            [
                (bin.to_owned(), vec![obj(bin, "main", "0cccccc")]),
                (
                    lib.to_owned(),
                    vec![obj(lib, "c1", "0aaaaaa"), obj(lib, "c2", "0aaaaaa")]
                ),
            ]
        );
    }

    #[test]
    fn without_named_objects_keeps_the_newest_invocation() {
        let tmp = tempfile::tempdir().unwrap();
        // New layout: a unit's out dir, objects named after the crate alone.
        let out = tmp.path().join("build/app/0123456789abcdef/out");
        let old = SystemTime::now() - Duration::from_secs(3600);
        for (invocation, time) in [("0old", old), ("0new", SystemTime::now())] {
            let path = out.join(obj("app", "x", invocation));
            write(&path, b"o");
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(FileTimes::new().set_modified(time))
                .unwrap();
        }
        write(&out.join("app"), b"no names here");
        assert_eq!(
            names(&stale_objects_in(&out, no_grace())),
            [("app".to_owned(), vec![obj("app", "x", "0old")])]
        );
    }

    #[test]
    fn keeps_invocations_within_the_grace() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        let now = SystemTime::now();
        let minutes = |m: u64| now - Duration::from_secs(60 * m);
        // Three compiles: an hour ago, 5 minutes ago (a binary from it may
        // still be running) and just now. The outputs name none.
        for (invocation, time) in [("0hour", minutes(60)), ("0five", minutes(5)), ("0now", now)] {
            for cgu in ["x", "y"] {
                let path = out.join(obj("app", cgu, invocation));
                write(&path, b"o");
                set_modified(&path, time);
            }
        }
        // One object of the hour-old compile was touched recently: the group
        // goes by its newest file.
        let touched = out.join(obj("lib", "x", "0old"));
        write(&touched, b"o");
        set_modified(&touched, minutes(60));
        let recent = out.join(obj("lib", "y", "0old"));
        write(&recent, b"o");
        set_modified(&recent, minutes(2));
        write(&out.join(obj("lib", "x", "0new")), b"o");

        let ten_minutes = minutes(10);
        assert_eq!(
            names(&stale_objects_in(&out, ten_minutes)),
            [(
                "app".to_owned(),
                vec![obj("app", "x", "0hour"), obj("app", "y", "0hour")]
            )]
        );
        // Without the grace, only the newest compile stays.
        assert_eq!(
            names(&stale_objects_in(&out, no_grace())),
            [
                (
                    "app".to_owned(),
                    vec![
                        obj("app", "x", "0five"),
                        obj("app", "x", "0hour"),
                        obj("app", "y", "0five"),
                        obj("app", "y", "0hour")
                    ]
                ),
                (
                    "lib".to_owned(),
                    vec![obj("lib", "x", "0old"), obj("lib", "y", "0old")]
                ),
            ]
        );
    }

    #[test]
    fn an_object_name_keeps_the_same_compiles_dwo() {
        let tmp = tempfile::tempdir().unwrap();
        let deps = tmp.path().join("deps");
        let lib = "dep-0123456789abcdef";
        let old = SystemTime::now() - Duration::from_secs(3600);
        // Split DWARF: the rlib's members are `.o`, the files left are `.dwo`.
        for (invocation, time) in [("0aaa", SystemTime::now()), ("0bbb", old)] {
            let path = deps.join(format!("{lib}.c1.{invocation}.rcgu.dwo"));
            write(&path, b"d");
            set_modified(&path, time);
        }
        let member = obj(lib, "c1", "0bbb");
        write(
            &deps.join(format!("lib{lib}.rlib")),
            &gnu_archive("", &[(&format!("{member}/"), b"code")]),
        );
        assert_eq!(
            names(&stale_objects_in(&deps, no_grace())),
            [(lib.to_owned(), vec![format!("{lib}.c1.0aaa.rcgu.dwo")])]
        );
    }

    #[test]
    fn reads_archive_member_names_not_their_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let short = "a-0123456789abcdef.c.0x.rcgu.o";
        let long = "app-0123456789abcdef.app.6a1b2c3d4e5f-cgu.07.0zzzzzz.rcgu.o";
        let in_data = "inside-0123456789abcdef.c.0y.rcgu.o";
        let bsd = tmp.path().join("bsd.rlib");
        fs::write(
            &bsd,
            bsd_archive(&[
                ("__.SYMDEF", b"symbols"),
                (short, format!("\0{in_data}\0").as_bytes()),
                (long, b"odd"),
                ("lib.rmeta", b"meta"),
            ]),
        )
        .unwrap();
        assert_eq!(
            named_objects(&bsd),
            HashSet::from([short.to_owned(), long.to_owned()])
        );

        let gnu = tmp.path().join("gnu.rlib");
        let table = format!("{long}/\n");
        fs::write(
            &gnu,
            gnu_archive(
                &table,
                &[
                    ("/", b"symbols"),
                    ("/0", in_data.as_bytes()),
                    ("lib.rmeta/", b"meta"),
                ],
            ),
        )
        .unwrap();
        assert_eq!(named_objects(&gnu), HashSet::from([long.to_owned()]));

        // A malformed archive is searched in full instead.
        let broken = tmp.path().join("broken.rlib");
        fs::write(&broken, format!("!<arch>\n#1/40   garbage {short}\0")).unwrap();
        assert_eq!(named_objects(&broken), HashSet::from([short.to_owned()]));
    }

    #[test]
    fn reads_a_mach_o_debug_map_from_its_string_table() {
        let tmp = tempfile::tempdir().unwrap();
        let mapped = "/t/debug/deps/app-0123456789abcdef.app.0abc.rcgu.o";
        let archived =
            "/t/debug/deps/libdep-0123456789abcdef.rlib(dep-0123456789abcdef.dep.0def.rcgu.o)";
        let mut strings = b"\0_main\0".to_vec();
        for s in [mapped, archived] {
            strings.extend(s.as_bytes());
            strings.push(0);
        }
        let bin = tmp.path().join("app");
        fs::write(&bin, macho(b"\0code.0nope.rcgu.o\0", &strings)).unwrap();
        assert_eq!(
            named_objects(&bin),
            HashSet::from([
                "app-0123456789abcdef.app.0abc.rcgu.o".to_owned(),
                "dep-0123456789abcdef.dep.0def.rcgu.o".to_owned(),
            ])
        );
    }

    /// Everything `output` names, read in full.
    fn named_in_full(output: &Path) -> HashSet<String> {
        let mut names = HashSet::new();
        search_names(&mut File::open(output).unwrap(), &mut names);
        names
    }

    /// The object names in a Mach-O file's debug map, found independently
    /// of [`named_objects`]: by walking its symbol table for `N_OSO` entries.
    /// An entry names an object or an rlib member: `/…/x.rcgu.o` or
    /// `/…/libx.rlib(x.rcgu.o)`.
    fn debug_map(binary: &[u8]) -> HashSet<String> {
        const N_OSO: u8 = 0x66;
        let u32_at = |at: usize| u32::from_le_bytes(binary[at..at + 4].try_into().unwrap());
        let count = u32_at(16) as usize;
        let mut at = 32;
        let (mut symoff, mut nsyms, mut stroff) = (0, 0, 0);
        for _ in 0..count {
            if u32_at(at) == LC_SYMTAB {
                (symoff, nsyms, stroff) = (
                    u32_at(at + 8) as usize,
                    u32_at(at + 12) as usize,
                    u32_at(at + 16) as usize,
                );
            }
            at += u32_at(at + 4) as usize;
        }
        (0..nsyms)
            .map(|i| symoff + 16 * i)
            .filter(|&entry| binary[entry + 4] == N_OSO)
            .map(|entry| {
                let start = stroff + u32_at(entry) as usize;
                let len = binary[start..].iter().position(|&b| b == 0).unwrap();
                let path = String::from_utf8_lossy(&binary[start..start + len]).into_owned();
                match path.split_once('(') {
                    Some((_, member)) => member.trim_end_matches(')').to_owned(),
                    None => path.rsplit('/').next().unwrap().to_owned(),
                }
            })
            .filter(|name| is_object(name))
            .collect()
    }

    /// Reading only part of real rustc output finds what reading it all
    /// does: this test binary's debug map (macOS), and the rlibs next to it.
    #[test]
    fn reading_part_of_real_outputs_finds_the_same_objects() {
        let exe = std::env::current_exe().unwrap();
        let parsed = named_objects(&exe);
        // A full read also finds string literals, such as this file's.
        assert!(parsed.is_subset(&named_in_full(&exe)));
        if cfg!(target_os = "macos") {
            let mapped = debug_map(&fs::read(&exe).unwrap());
            assert!(!mapped.is_empty(), "no debug map in {}", exe.display());
            assert_eq!(parsed, mapped);
        }
        let deps = exe.parent().unwrap();
        let rlibs = entries(deps).filter(|(name, _)| name.ends_with(".rlib"));
        for (name, path) in rlibs.take(20) {
            let parsed = named_objects(&path);
            assert!(!parsed.is_empty(), "{name}");
            // A full read also picks up the member names' bytes run on from
            // what precedes them in member data (`8toml-….rcgu.o`), never a
            // name the headers lack.
            for found in named_in_full(&path) {
                assert!(
                    parsed.iter().any(|p| found.ends_with(p.as_str())),
                    "{name}: {found} missing"
                );
            }
        }
    }

    #[test]
    fn finds_names_split_across_reads_and_ignores_dwo_prefixes() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("bin");
        let name = "app-0123456789abcdef.main.0aaa.rcgu.o";
        let dwo = "app-0123456789abcdef.main.0bbb.rcgu.dwo";
        let mut contents = vec![0u8; (1 << 20) - 10];
        contents.extend(name.as_bytes());
        contents.push(0);
        contents.extend(dwo.as_bytes());
        contents.push(0);
        write(&output, &contents);
        assert_eq!(
            named_objects(&output),
            HashSet::from([name.to_owned(), dwo.to_owned()])
        );
    }

    #[test]
    fn recognizes_layouts_and_profiles() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(
            &root.join("CACHEDIR.TAG"),
            b"Signature: x\n# created by cargo.\n",
        );
        fs::create_dir_all(root.join("debug/.fingerprint/a-0123456789abcdef")).unwrap();
        fs::create_dir_all(root.join("debug/build/serde/0123456789abcdef/fingerprint")).unwrap();
        fs::create_dir_all(root.join("debug/build/serde-0123456789abcdef")).unwrap();
        fs::create_dir_all(root.join("aarch64-apple-darwin/release/.fingerprint")).unwrap();
        fs::create_dir_all(root.join("doc/app")).unwrap();

        let found = profiles(root);
        assert_eq!(
            found,
            [
                Profile {
                    path: root.join("aarch64-apple-darwin/release"),
                    legacy: true,
                    v2: false
                },
                Profile {
                    path: root.join("debug"),
                    legacy: true,
                    v2: true
                },
            ]
        );
        let legacy = legacy_paths(&root.join("debug"));
        assert!(legacy.contains(&root.join("debug/.fingerprint")));
        assert!(legacy.contains(&root.join("debug/build/serde-0123456789abcdef")));
        assert!(!legacy.contains(&root.join("debug/build/serde")));
        assert_eq!(v2_units(&root.join("debug")).len(), 1);
        assert_eq!(
            find_root(&root.join("debug/build/serde")),
            Some(root.to_path_buf())
        );
    }

    #[test]
    fn finds_roots_by_their_rustc_cache_before_per_target_tags() {
        let tmp = tempfile::tempdir().unwrap();
        let tag = b"Signature: x\n# created by cargo.\n";
        // Cargo tags the per-target subdirectory too.
        let root = tmp.path().join("t");
        write(&root.join(".rustc_info.json"), b"{}");
        write(&root.join("CACHEDIR.TAG"), tag);
        write(&root.join("wasm32-unknown-unknown/CACHEDIR.TAG"), tag);
        let deps = root.join("wasm32-unknown-unknown/debug/deps");
        fs::create_dir_all(&deps).unwrap();
        assert_eq!(find_root(&deps), Some(root.clone()));
        // A target dir a script created before cargo ran has no tag.
        let bare = tmp.path().join("bare");
        write(&bare.join(".rustc_info.json"), b"{}");
        fs::create_dir_all(bare.join("debug/deps")).unwrap();
        assert_eq!(find_root(&bare.join("debug/deps")), Some(bare.clone()));
        assert_eq!(
            discover(&[tmp.path().to_path_buf()], 6),
            [bare, root.clone()]
        );
        assert_eq!(
            outermost(vec![root.join("wasm32-unknown-unknown"), root.clone()]),
            [root]
        );
    }

    #[test]
    fn cargo_download_caches_are_not_build_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = tmp.path().join("cargo-home/registry");
        write(
            &registry.join("CACHEDIR.TAG"),
            b"Signature: x\n# created by cargo.\n",
        );
        fs::create_dir_all(registry.join("index/index.crates.io-1949cf8c6b5b557f")).unwrap();
        write(
            &registry.join("src/index.crates.io-1949cf8c6b5b557f/serde-1.0.0/target/CACHEDIR.TAG"),
            b"Signature: x\n# created by cargo.\n",
        );
        assert!(!is_cargo_root(&registry));
        assert!(discover(&[tmp.path().to_path_buf()], 8).is_empty());
    }

    #[test]
    fn only_superseded_finalized_sessions_are_returned() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("app-1a2b3c");
        let old = "s-h1aaaa-xyz1-3abc";
        let new = "s-h2bbbb-xyz2-4def";
        let working = "s-h3cccc-xyz3-working";
        for name in [old, new, working] {
            write(&cache.join(name).join("dep-graph.bin"), b"g");
        }
        for lock in [
            "s-h1aaaa-xyz1.lock",
            "s-h2bbbb-xyz2.lock",
            "s-h3cccc-xyz3.lock",
        ] {
            write(&cache.join(lock), b"");
        }
        let past = SystemTime::now() - Duration::from_secs(3600);
        File::open(cache.join(old))
            .unwrap()
            .set_times(FileTimes::new().set_modified(past))
            .unwrap();
        let mut found = superseded_sessions(&cache);
        found.sort();
        assert_eq!(found, [cache.join(old), cache.join("s-h1aaaa-xyz1.lock")]);
        // A single finalized session is the live one.
        fs::remove_dir_all(cache.join(new)).unwrap();
        assert!(superseded_sessions(&cache).is_empty());
    }

    #[test]
    fn discovers_build_dirs_without_descending_into_them() {
        let tmp = tempfile::tempdir().unwrap();
        let tag = b"Signature: x\n# This file is a cache directory tag created by cargo.\n";
        write(&tmp.path().join("p/a/target/CACHEDIR.TAG"), tag);
        fs::create_dir_all(tmp.path().join("p/a/target/debug/.fingerprint")).unwrap();
        write(&tmp.path().join("p/a/target/nested/CACHEDIR.TAG"), tag);
        fs::create_dir_all(tmp.path().join("p/a/target/nested/debug/.fingerprint")).unwrap();
        write(
            &tmp.path().join("q/CACHEDIR.TAG"),
            b"Signature: x\n# made by some other tool\n",
        );
        write(
            &tmp.path().join("p/node_modules/x/target/CACHEDIR.TAG"),
            tag,
        );
        assert_eq!(
            discover(&[tmp.path().to_path_buf()], 6),
            [tmp.path().join("p/a/target")]
        );
    }

    /// A 60-byte archive member header.
    fn header(name: &str, size: &str, end: &str) -> Vec<u8> {
        format!(
            "{name:<16}{:<12}{:<6}{:<6}{:<8}{size:<10}{end}",
            0, 0, 0, 644
        )
        .into_bytes()
    }

    fn archive_parses(bytes: Vec<u8>) -> bool {
        archive_names(&mut io::Cursor::new(bytes), &mut HashSet::new())
    }

    #[test]
    fn malformed_archives_do_not_parse() {
        // A header cut short.
        assert!(!archive_parses(header("a.o", "4", "`\n")[..30].to_vec()));
        // A header that doesn't end as headers do.
        assert!(!archive_parses(header("a.o", "4", "xx")));
        // A size that isn't a number.
        assert!(!archive_parses(header("a.o", "four", "`\n")));
        // A BSD long name longer than its member.
        assert!(!archive_parses(header("#1/9", "4", "`\n")));
        // A BSD long name cut short.
        let mut cut = header("#1/8", "8", "`\n");
        cut.extend(b"abc");
        assert!(!archive_parses(cut));
        // A GNU name table too big to read, and one cut short.
        assert!(!archive_parses(header("// ", "9999999999", "`\n")));
        let mut cut = header("// ", "8", "`\n");
        cut.extend(b"abc");
        assert!(!archive_parses(cut));
        // A well-formed member still parses.
        let mut ok = header("a.x.0a.rcgu.o", "2", "`\n");
        ok.extend(b"zz");
        let mut names = HashSet::new();
        assert!(archive_names(&mut io::Cursor::new(ok), &mut names));
        assert_eq!(names, HashSet::from(["a.x.0a.rcgu.o".to_owned()]));
    }

    fn macho_parses(dir: &Path, words: &[u32]) -> bool {
        let path = dir.join("bin");
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        fs::write(&path, bytes).unwrap();
        let mut file = BufReader::new(File::open(&path).unwrap());
        // The magic is read before the rest is parsed.
        file.read_exact(&mut [0u8; 8]).unwrap();
        macho_names(&mut file, &mut HashSet::new())
    }

    #[test]
    fn malformed_mach_o_files_do_not_parse() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let head = |count: u32, size: u32| vec![MH_MAGIC_64, 0, 0, 2, count, size, 0, 0];
        // A header cut short.
        assert!(!macho_parses(dir, &[MH_MAGIC_64, 0, 0, 2]));
        // Load commands too big to believe, and ones cut short.
        assert!(!macho_parses(dir, &head(1, 32 << 20)));
        assert!(!macho_parses(dir, &head(1, 64)));
        // More commands than the commands' size holds.
        let mut words = head(2, 8);
        words.extend([0x19, 8]);
        assert!(!macho_parses(dir, &words));
        // A command too small to step past.
        let mut words = head(1, 8);
        words.extend([0x19, 4]);
        assert!(!macho_parses(dir, &words));
        // No symbol table.
        assert!(!macho_parses(dir, &head(0, 0)));
        // A file that doesn't parse is searched in full.
        let path = dir.join("named");
        let mut bytes: Vec<u8> = head(0, 0).iter().flat_map(|w| w.to_le_bytes()).collect();
        bytes.extend(b" app.x.0a.rcgu.o ");
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            named_objects(&path),
            HashSet::from(["app.x.0a.rcgu.o".to_owned()])
        );
    }

    #[test]
    fn a_missing_output_names_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(named_objects(&tmp.path().join("missing")).is_empty());
    }

    #[test]
    fn only_rcgu_objects_count_as_names() {
        let mut names = HashSet::new();
        find_names(
            b"a.rcgu.x b.rcgu.dwo c.rcgu.otmp d.rcgu.o",
            false,
            &mut names,
        );
        assert_eq!(
            names,
            HashSet::from(["b.rcgu.dwo".to_owned(), "d.rcgu.o".to_owned()])
        );
    }

    #[test]
    fn a_read_error_ends_reading() {
        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("broken"))
            }
        }
        assert_eq!(read_up_to(&mut Failing, &mut [0u8; 4]), 0);
    }

    #[test]
    fn a_superseded_session_without_a_lock_file_is_still_returned() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("app-1a2b3c");
        write(&cache.join("s-h1aaaa-xyz1-3abc/dep-graph.bin"), b"g");
        // A name that doesn't follow rustc's pattern has no lock to find.
        write(&cache.join("s-h0odd/dep-graph.bin"), b"g");
        write(&cache.join("s-h2bbbb-xyz2-4def/dep-graph.bin"), b"g");
        File::open(cache.join("s-h1aaaa-xyz1-3abc"))
            .unwrap()
            .set_times(FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(3600)))
            .unwrap();
        File::open(cache.join("s-h0odd"))
            .unwrap()
            .set_times(FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(7200)))
            .unwrap();
        let mut found = superseded_sessions(&cache);
        found.sort();
        assert_eq!(
            found,
            [cache.join("s-h0odd"), cache.join("s-h1aaaa-xyz1-3abc")]
        );
    }

    #[test]
    fn object_names_without_an_invocation_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("plain.rcgu.o"), b"o");
        assert!(stale_objects_in(tmp.path(), no_grace()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn objects_that_cannot_be_examined_are_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("deps");
        for invocation in ["0a", "0b"] {
            write(&dir.join(obj("app-1", "x", invocation)), b"o");
        }
        // Listable, but its entries can't be looked up.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o444)).unwrap();
        let stale = stale_objects_in(&dir, no_grace());
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(stale.is_empty(), "{}", stale.len());
    }

    #[test]
    fn reads_fingerprint_hashes_as_cargo_writes_them() {
        // `run-build-script-...` and the `deps` entry naming it, from a
        // Cargo 1.100 build of serde_core.
        assert_eq!(
            fingerprint_hash("bb77cc6cea257cfc"),
            Some(18_193_458_283_447_875_515)
        );
        assert_eq!(fingerprint_hash("bb77cc6cea257cf"), None);
        assert_eq!(fingerprint_hash("not a hash at al"), None);

        let tmp = tempfile::tempdir().unwrap();
        let fingerprint = tmp
            .path()
            .join("debug/build/serde/0123456789abcdef/fingerprint");
        write(&fingerprint.join("lib-serde"), b"b043a6746cb6a463\n");
        write(
            &fingerprint.join("lib-serde.json"),
            br#"{"path":1,"deps":[[11029742160753049355,"build_script_build",false,18193458283447875515]]}"#,
        );
        write(&fingerprint.join("dep-lib-serde"), b"0123456789abcdef");
        write(&fingerprint.join("invoked.timestamp"), b"");
        let nodes = v2_unit_nodes(&tmp.path().join("debug"));
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].hashes, [7_180_064_282_865_189_808]);
        assert_eq!(nodes[0].deps, [18_193_458_283_447_875_515]);
        assert!(nodes[0].readable);

        write(&fingerprint.join("lib-serde.json"), b"{\"deps\": 5}");
        assert!(!v2_unit_nodes(&tmp.path().join("debug"))[0].readable);
    }

    #[test]
    fn a_name_carried_into_the_next_read_isnt_recorded_cut_short() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("bin");
        let name = "clap_builder-a75bec2d0da09b09.clap_builder.65f45cd6fef9cfe4-cgu.06.rcgu.o";
        // Whole in the first read, but starting before the 256 bytes carried
        // into the second, which then begin partway into it.
        let mut contents = vec![0u8; (1 << 20) - 280];
        contents.extend(name.as_bytes());
        contents.resize(1 << 20, 0);
        contents.extend([0u8; 1000]);
        write(&output, &contents);
        assert_eq!(named_objects(&output), HashSet::from([name.to_owned()]));
    }
}
