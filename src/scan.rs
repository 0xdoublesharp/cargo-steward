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
use std::io::Read;
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
    for child in subdirs(root) {
        if let Some(profile) = as_profile(&child) {
            found.push(profile);
            continue;
        }
        found.extend(subdirs(&child).filter_map(|g| as_profile(&g)));
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
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
/// Non-incremental objects (`...-cgu.<n>.rcgu.o`) are overwritten in place by
/// each compile and are left alone.
pub fn stale_objects_in(dir: &Path) -> Vec<StaleObjects> {
    struct Object {
        name: String,
        path: PathBuf,
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
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        units
            .entry(unit)
            .or_default()
            .entry(invocation.to_owned())
            .or_default()
            .push(Object {
                name,
                path,
                modified,
            });
    }

    let mut stale = Vec::new();
    for (unit, invocations) in units {
        if invocations.len() < 2 {
            continue;
        }
        let named: HashSet<String> = others
            .iter()
            .filter(|(name, path)| {
                name.contains(&unit)
                    && !name.ends_with(".d")
                    && !name.ends_with(".rmeta")
                    && path.is_file()
            })
            .flat_map(|(_, path)| named_objects(path))
            .collect();
        let mut keep: HashSet<&str> = invocations
            .iter()
            .filter(|(_, objects)| objects.iter().any(|o| named.contains(&o.name)))
            .map(|(invocation, _)| invocation.as_str())
            .collect();
        if keep.is_empty() {
            let newest = |objects: &Vec<Object>| objects.iter().map(|o| o.modified).max();
            let latest = invocations.values().filter_map(newest).max();
            keep = invocations
                .iter()
                .filter(|(_, objects)| newest(objects) == latest)
                .map(|(invocation, _)| invocation.as_str())
                .collect();
        }
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

/// Names of the codegen-unit object files that `output` mentions anywhere in
/// its bytes, such as `/…/deps/app-0123456789abcdef.0a1b.1x2y.rcgu.o` in a
/// debug map or an rlib member name.
pub fn named_objects(output: &Path) -> HashSet<String> {
    const CHUNK: usize = 1 << 20;
    // Carried between reads so a name split across two is still found.
    // File names are at most 255 bytes.
    const OVERLAP: usize = 256;
    let is_name_byte = |b: &u8| b.is_ascii_alphanumeric() || b"._-".contains(b);

    let mut names = HashSet::new();
    let Ok(mut file) = File::open(output) else {
        return names;
    };
    let mut buf: Vec<u8> = Vec::with_capacity(OVERLAP + CHUNK);
    loop {
        let kept = buf.len();
        buf.resize(kept + CHUNK, 0);
        let read = match file.read(&mut buf[kept..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.truncate(kept + read);
        for suffix in OBJECT_SUFFIXES.map(str::as_bytes) {
            for end in buf
                .windows(suffix.len())
                .enumerate()
                .filter(|(_, w)| *w == suffix)
                .map(|(i, _)| i + suffix.len())
            {
                // A match followed by more name bytes (`x.rcgu.o.tmp`) isn't
                // an object's name.
                if buf.get(end).is_some_and(is_name_byte) {
                    continue;
                }
                let start = buf[..end]
                    .iter()
                    .rposition(|b| !is_name_byte(b))
                    .map_or(0, |i| i + 1);
                names.insert(String::from_utf8_lossy(&buf[start..end]).into_owned());
            }
        }
        buf.drain(..buf.len().saturating_sub(OVERLAP));
    }
    names
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
            names(&stale_objects_in(&deps)),
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
            names(&stale_objects_in(&out)),
            [("app".to_owned(), vec![obj("app", "x", "0old")])]
        );
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
}
