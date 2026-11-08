//! `cargo steward dedup`: replaces identical files across build directories
//! with copy-on-write clones of one copy.
//!
//! Worktrees of one repository build the same dependencies into separate
//! build directories, byte for byte. Cargo names each output after its unit
//! hash, so identical outputs have the same name and size; those are
//! compared byte by byte, and each duplicate is replaced by a clone of the
//! kept copy, which shares its blocks. Cargo can't tell the difference: the
//! clone has the duplicate's contents, times and permissions.
//!
//! Hard links are never made (cargo and rustc write some outputs in place,
//! which would change every linked copy), and files that already have other
//! links are left alone. Incremental caches are skipped: rustc treats each
//! one as a unit.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::{self, Paths};
use crate::fsutil::{self, Lock, human};
use crate::gc::{self, BuildDir};
use crate::json;
use crate::seed::clone::{self, Cloned};

/// Smaller files aren't worth the bookkeeping.
pub const MIN_SIZE: u64 = 64 * 1024;

/// Marks the temporary name a clone is made under before it replaces a
/// duplicate.
const TEMP_MARK: &str = ".steward-dedup-";

/// Identical files: one kept, the others replaced by clones of it.
#[derive(Debug, Clone, Default)]
pub struct Group {
    /// Size of each file.
    pub size: u64,
    pub kept: PathBuf,
    /// Replaced by clones of `kept`, or with `apply` off, would be.
    pub replaced: Vec<PathBuf>,
}

/// What a deduplication did, or would do.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub groups: Vec<Group>,
    /// Duplicates replaced (or that would be).
    pub files: usize,
    /// Their combined size: what sharing blocks gives back, at most.
    pub bytes: u64,
    /// Duplicates skipped because a build held their directory's locks.
    pub busy: usize,
    /// Duplicates that changed since they were compared, or couldn't be
    /// replaced.
    pub skipped: usize,
    /// Volumes holding build directories whose filesystem can't clone.
    pub unsupported_volumes: usize,
    /// With `apply`: the measured change in free space on the volumes
    /// involved, in bytes. Other activity on them counts too.
    pub freed: Option<i64>,
}

/// `cargo steward dedup`.
pub fn run(apply: bool, verbose: bool, as_json: bool) -> Result<()> {
    let paths = Paths::resolve().context("cannot locate the home directory")?;
    let config = config::load_warning(&paths);
    let (_, dirs) = gc::inventory(&paths, &config.scan_roots, apply)?;
    if dirs.is_empty() && !as_json {
        println!("No build directories found yet.");
        return Ok(());
    }
    let started = SystemTime::now();
    let summary = if dirs.is_empty() {
        Summary::default()
    } else {
        dedup(&dirs, apply, None)?
    };
    if apply && summary.busy == 0 {
        record_pass(&paths, started);
    }
    if as_json {
        return json::print(&Document::new(&summary, dirs.len(), apply));
    }
    print!("{}", report(&summary, dirs.len(), apply, verbose));
    Ok(())
}

/// `dedup --json`.
#[derive(Serialize)]
struct Document {
    version: u32,
    applied: bool,
    /// Build directories examined.
    build_dirs: usize,
    /// Duplicates replaced by clones (or that would be).
    files: usize,
    /// Their combined size: what sharing blocks gives back, at most.
    bytes: u64,
    groups: Vec<GroupDoc>,
    /// Duplicates skipped because a build held their directory's locks.
    busy: usize,
    /// Duplicates that changed since comparing, or couldn't be replaced.
    skipped: usize,
    unsupported_volumes: usize,
    /// With `--apply`: the measured change in free space, which other
    /// activity on the disks moves too.
    freed_on_disk_bytes: Option<i64>,
}

#[derive(Serialize)]
struct GroupDoc {
    /// Each file's size.
    size_bytes: u64,
    kept: String,
    replaced: Vec<String>,
}

impl Document {
    fn new(summary: &Summary, dirs: usize, apply: bool) -> Document {
        Document {
            version: json::VERSION,
            applied: apply,
            build_dirs: dirs,
            files: summary.files,
            bytes: summary.bytes,
            groups: summary
                .groups
                .iter()
                .map(|g| GroupDoc {
                    size_bytes: g.size,
                    kept: json::path(&g.kept),
                    replaced: g.replaced.iter().map(|p| json::path(p)).collect(),
                })
                .collect(),
            busy: summary.busy,
            skipped: summary.skipped,
            unsupported_volumes: summary.unsupported_volumes,
            freed_on_disk_bytes: summary.freed,
        }
    }
}

fn report(summary: &Summary, dirs: usize, apply: bool, verbose: bool) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if verbose {
        for group in &summary.groups {
            let _ = writeln!(out, "{:>10}  {}", human(group.size), group.kept.display());
            for path in &group.replaced {
                let _ = writeln!(out, "{:>10}  = {}", "", path.display());
            }
        }
        if !summary.groups.is_empty() {
            let _ = writeln!(out);
        }
    }
    if summary.files == 0 {
        let _ = writeln!(
            out,
            "No duplicate files to replace in {dirs} build directories."
        );
    } else {
        let verb = if apply { "Replaced" } else { "Would replace" };
        let _ = writeln!(
            out,
            "{verb} {} duplicate files ({}) in {} groups with clones.",
            summary.files,
            human(summary.bytes),
            summary.groups.len()
        );
    }
    if summary.busy > 0 {
        let _ = writeln!(
            out,
            "Skipped {} files in build directories a build was using.",
            summary.busy
        );
    }
    if summary.skipped > 0 {
        let _ = writeln!(
            out,
            "Skipped {} files that changed or couldn't be replaced.",
            summary.skipped
        );
    }
    if summary.unsupported_volumes > 0 {
        let _ = writeln!(
            out,
            "Skipped {} volume(s) whose filesystem can't clone files.",
            summary.unsupported_volumes
        );
    }
    match summary.freed {
        _ if summary.files == 0 => {}
        Some(freed) if freed >= 0 => {
            let _ = writeln!(
                out,
                "Free space went up by {}.",
                human(freed.unsigned_abs())
            );
        }
        Some(freed) => {
            let _ = writeln!(
                out,
                "Free space went down by {} (other activity on the disk counts too).",
                human(freed.unsigned_abs())
            );
        }
        None => {
            let _ = writeln!(out, "Run `cargo steward dedup --apply` to replace them.");
        }
    }
    out
}

/// A file that may have duplicates.
#[derive(Debug)]
struct Candidate {
    path: PathBuf,
    /// The profile directory holding it, whose locks cover it.
    profile: PathBuf,
    /// Read before its contents were compared.
    meta: fs::Metadata,
    /// See [`clone::clone_id`].
    clone_id: Option<u64>,
}

/// Files with identical contents that already share one copy of their
/// data (pure clones of each other), or a single file.
type Cluster = Vec<Candidate>;

/// Deduplicates what changed in `dirs` since the last applied pass, as
/// cleanup does after it runs: files created or replaced since then, and the
/// files at the same place in the other build directories on their volume,
/// where the same unit's outputs are ([`dedup`] with `since`). The first
/// pass, with none recorded, is a full one. A pass that a build kept from
/// replacing anything isn't recorded, so the next one looks again.
pub fn after_cleanup(paths: &Paths, dirs: &[BuildDir]) -> Result<Summary> {
    let started = SystemTime::now();
    let summary = dedup(dirs, true, last_pass(paths))?;
    if summary.busy == 0 {
        record_pass(paths, started);
    }
    Ok(summary)
}

/// When the last applied pass started. `None` before the first, or when the
/// record can't be read or is in the future (the clock went back).
pub fn last_pass(paths: &Paths) -> Option<SystemTime> {
    let secs: u64 = fs::read_to_string(paths.dedup_stamp())
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let when = UNIX_EPOCH + Duration::from_secs(secs);
    (when <= SystemTime::now()).then_some(when)
}

/// Records that an applied pass started at `started`. Best effort: without
/// it the next pass is a full one.
fn record_pass(paths: &Paths, started: SystemTime) {
    let secs = started
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let _ = config::write_atomic(&paths.dedup_stamp(), format!("{secs}\n").as_bytes());
}

/// Finds identical files of at least [`MIN_SIZE`] across the profiles of
/// `dirs`, on the same volume, and with `apply` replaces each duplicate with
/// a clone of one kept copy. Each replacement holds cargo's locks on the
/// profiles of both files; one a build is using is skipped. Files that are
/// already clones of the kept copy (known on APFS) are left alone.
///
/// With `since`, only files created or replaced since then are looked for
/// (in directories changed since then), with the files at the same path in
/// the other build directories; otherwise every file is.
pub fn dedup(dirs: &[BuildDir], apply: bool, since: Option<SystemTime>) -> Result<Summary> {
    let mut summary = Summary::default();
    // Volume → a build directory on it, for volumes that can clone.
    let mut volumes: BTreeMap<u64, PathBuf> = BTreeMap::new();
    let mut unsupported: HashSet<u64> = HashSet::new();
    let mut eligible: Vec<&BuildDir> = Vec::new();
    for dir in dirs {
        let Some(device) = dir.device else {
            continue;
        };
        if unsupported.contains(&device) {
            continue;
        }
        if let Entry::Vacant(entry) = volumes.entry(device) {
            if !clone::may_clone(&dir.root) {
                unsupported.insert(device);
                continue;
            }
            entry.insert(dir.root.clone());
        }
        eligible.push(dir);
    }
    summary.unsupported_volumes = unsupported.len();
    let found = match since {
        None => fsutil::parallel_map(&eligible, |dir| {
            dir.profiles
                .iter()
                .flat_map(|p| collect(&p.path))
                .collect::<Vec<_>>()
        })
        .into_iter()
        .flatten()
        .collect(),
        Some(since) => changed_and_counterparts(&eligible, since),
    };
    let mut by_key: HashMap<(u64, OsString, u64), Vec<Candidate>> = HashMap::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for candidate in found {
        if !seen.insert(candidate.path.clone()) {
            continue;
        }
        let Some(name) = candidate.path.file_name() else {
            continue;
        };
        let key = (dev(&candidate.meta), name.to_owned(), candidate.meta.len());
        by_key.entry(key).or_default().push(candidate);
    }

    let before: BTreeMap<u64, u64> = if apply {
        volumes
            .iter()
            .filter_map(|(&d, root)| Some((d, fsutil::free_space(root)?)))
            .collect()
    } else {
        BTreeMap::new()
    };

    let mut keys: Vec<_> = by_key.keys().cloned().collect();
    keys.sort();
    let mut failed_volumes: HashSet<u64> = HashSet::new();
    for key in keys {
        let Some(group) = by_key.remove(&key) else {
            continue;
        };
        if failed_volumes.contains(&key.0) {
            continue;
        }
        // Pure clones hold the same data already: one of each is compared,
        // and a group that is all one clone has nothing to gain.
        let clusters = by_clone_id(group);
        if clusters.len() < 2 {
            continue;
        }
        for class in partition(clusters) {
            if failed_volumes.contains(&key.0) {
                break;
            }
            if let Some(found) = replace_class(class, apply, &mut summary) {
                summary.groups.push(found);
            } else if apply {
                // `None` with apply set only when cloning proved unsupported.
                failed_volumes.insert(key.0);
            }
        }
    }
    summary.unsupported_volumes += failed_volumes.len();
    summary.groups.retain(|g| !g.replaced.is_empty());

    if apply {
        let mut freed = 0i64;
        for (device, was) in before {
            if let Some(now) = volumes.get(&device).and_then(|r| fsutil::free_space(r)) {
                freed += now as i64 - was as i64;
            }
        }
        summary.freed = Some(freed);
    }
    Ok(summary)
}

/// Replaces the files in a class of identical ones with clones of one
/// kept copy: one from the largest cluster, whose members already share its
/// data and stay as they are. Returns the group, or `None` if the
/// filesystem turned out not to clone.
fn replace_class(mut class: Vec<Cluster>, apply: bool, summary: &mut Summary) -> Option<Group> {
    let largest = (0..class.len())
        .max_by_key(|&i| (class[i].len(), std::cmp::Reverse(i)))
        .unwrap_or(0);
    // The rest of the kept file's cluster are clones of it already.
    let Some(kept) = class.get_mut(largest).and_then(Vec::pop) else {
        return Some(Group::default());
    };
    class.remove(largest);
    let mut group = Group {
        size: kept.meta.len(),
        kept: kept.path.clone(),
        replaced: Vec::new(),
    };
    for cluster in class {
        let mut any = false;
        for dup in cluster {
            if apply {
                match replace(&kept, &dup) {
                    Replaced::Done => {}
                    Replaced::Busy => {
                        summary.busy += 1;
                        continue;
                    }
                    Replaced::Skipped => {
                        summary.skipped += 1;
                        continue;
                    }
                    Replaced::Unsupported => {
                        summary.groups.push(group);
                        return None;
                    }
                }
            }
            summary.files += 1;
            any = true;
            group.replaced.push(dup.path);
        }
        // A cluster's members share one copy of the data, so replacing
        // them gives back one file's worth.
        if any {
            summary.bytes += group.size;
        }
    }
    Some(group)
}

enum Replaced {
    Done,
    Busy,
    Skipped,
    Unsupported,
}

/// Replaces `dup` with a clone of `kept` holding cargo's locks on both
/// files' profiles, if neither changed since their contents were compared.
fn replace(kept: &Candidate, dup: &Candidate) -> Replaced {
    let mut profiles = vec![&kept.profile, &dup.profile];
    profiles.dedup();
    let mut locks = Vec::new();
    for profile in profiles {
        match fsutil::lock_profile(profile) {
            Lock::Busy => return Replaced::Busy,
            lock => locks.push(lock),
        }
    }
    // A change to a file's contents changes its ctime, which nothing can
    // set back, so an unchanged stamp means the comparison still holds.
    if !unchanged(kept) || !unchanged(dup) {
        return Replaced::Skipped;
    }
    let (Some(dir), Some(name)) = (dup.path.parent(), dup.path.file_name()) else {
        return Replaced::Skipped;
    };
    let temp = dir.join(format!(
        ".{}{TEMP_MARK}{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    let _ = fs::remove_file(&temp);
    match clone::clone_file(&kept.path, &temp) {
        Ok(Cloned::Done) => {}
        Ok(Cloned::Unsupported) => return Replaced::Unsupported,
        Err(_) => {
            let _ = fs::remove_file(&temp);
            return Replaced::Skipped;
        }
    }
    if clone::set_times_and_mode(&temp, &dup.meta).is_err() || fs::rename(&temp, &dup.path).is_err()
    {
        let _ = fs::remove_file(&temp);
        return Replaced::Skipped;
    }
    drop(locks);
    Replaced::Done
}

/// Regular files of at least [`MIN_SIZE`] with a single link, under
/// `profile`, outside its incremental caches.
fn collect(profile: &Path) -> Vec<Candidate> {
    let mut found = Vec::new();
    let mut stack = vec![profile.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for (name, path) in fsutil::entries(&dir) {
            if skipped(&name, dir == profile) {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
            } else if let Some(candidate) = candidate(path, profile, meta) {
                found.push(candidate);
            }
        }
    }
    found
}

/// Entries never looked at: what's being deleted or replaced, and the
/// incremental caches (`top`: directly in the profile).
fn skipped(name: &str, top: bool) -> bool {
    name.starts_with(fsutil::TRASH_PREFIX)
        || name.contains(TEMP_MARK)
        || (top && name == "incremental")
}

/// The file at `path` in `profile`, if it may have duplicates worth
/// replacing: a regular file of at least [`MIN_SIZE`] with a single link.
fn candidate(path: PathBuf, profile: &Path, meta: fs::Metadata) -> Option<Candidate> {
    (meta.is_file() && meta.len() >= MIN_SIZE && links(&meta) == 1).then(|| Candidate {
        clone_id: clone::clone_id(&path),
        path,
        profile: profile.to_path_buf(),
        meta,
    })
}

/// [`collect`], only for files created or replaced since `since`. Those
/// change their directory, so only directories changed since then are
/// listed in full; every directory is still visited for its subdirectories.
fn collect_changed(profile: &Path, since: SystemTime) -> Vec<Candidate> {
    let mut found = Vec::new();
    let Ok(meta) = fs::symlink_metadata(profile) else {
        return found;
    };
    let mut stack = vec![(profile.to_path_buf(), changed_since(&meta, since))];
    while let Some((dir, changed)) = stack.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = entry.file_name();
            if skipped(&name.to_string_lossy(), dir == profile) {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if let Ok(meta) = entry.metadata() {
                    stack.push((entry.path(), changed_since(&meta, since)));
                }
            } else if changed
                && kind.is_file()
                && let Ok(meta) = entry.metadata()
                && changed_since(&meta, since)
                && let Some(candidate) = candidate(entry.path(), profile, meta)
            {
                found.push(candidate);
            }
        }
    }
    found
}

/// Files changed since `since` in `dirs` ([`collect_changed`]), and the
/// files at the same path in the other build directories on the same
/// volume: where the same unit's outputs are, which may be older.
fn changed_and_counterparts(dirs: &[&BuildDir], since: SystemTime) -> Vec<Candidate> {
    let changed = fsutil::parallel_map(dirs, |dir| {
        dir.profiles
            .iter()
            .flat_map(|p| collect_changed(&p.path, since))
            .collect::<Vec<_>>()
    });
    let mut found = Vec::new();
    // Each path is looked up once per volume, however many directories it
    // changed in.
    let mut looked_up: HashSet<(Option<u64>, &Path)> = HashSet::new();
    for (dir, files) in dirs.iter().zip(&changed) {
        for file in files {
            let (Ok(path), Ok(profile)) = (
                file.path.strip_prefix(&dir.root),
                file.profile.strip_prefix(&dir.root),
            ) else {
                continue;
            };
            if !looked_up.insert((dir.device, path)) {
                continue;
            }
            for other in dirs
                .iter()
                .filter(|o| o.device == dir.device && o.root != dir.root)
            {
                let at = other.root.join(path);
                if let Ok(meta) = fs::symlink_metadata(&at)
                    && let Some(candidate) = candidate(at, &other.root.join(profile), meta)
                {
                    found.push(candidate);
                }
            }
        }
    }
    found.extend(changed.into_iter().flatten());
    found
}

/// Whether a file's status changed at or after `since` (less a second for
/// coarse timestamps). Its change time, unlike its modification time,
/// can't be set back, and moves when it's written, created or renamed.
fn changed_since(meta: &fs::Metadata, since: SystemTime) -> bool {
    changed(meta)
        >= since
            .checked_sub(Duration::from_secs(1))
            .unwrap_or(UNIX_EPOCH)
}

#[cfg(unix)]
fn changed(meta: &fs::Metadata) -> SystemTime {
    use std::os::unix::fs::MetadataExt;
    let secs = u64::try_from(meta.ctime()).unwrap_or(0);
    let nanos = u32::try_from(meta.ctime_nsec()).unwrap_or(0);
    UNIX_EPOCH + Duration::new(secs, nanos)
}

#[cfg(not(unix))]
fn changed(meta: &fs::Metadata) -> SystemTime {
    meta.modified().unwrap_or(UNIX_EPOCH)
}

/// Groups files that are pure clones of each other. Files whose clone ID
/// isn't known are each their own cluster.
fn by_clone_id(files: Vec<Candidate>) -> Vec<Cluster> {
    let mut clusters: Vec<Cluster> = Vec::new();
    for file in files {
        let same = file.clone_id.and_then(|id| {
            clusters
                .iter()
                .position(|c| c.first().and_then(|f| f.clone_id) == Some(id))
        });
        match same {
            Some(i) => clusters[i].push(file),
            None => clusters.push(vec![file]),
        }
    }
    clusters
}

/// Splits clusters of files of the same size into classes of identical
/// contents, comparing the first file of each, and drops clusters that
/// match no other.
fn partition(mut clusters: Vec<Cluster>) -> Vec<Vec<Cluster>> {
    let mut classes = Vec::new();
    while clusters.len() >= 2 {
        let first = clusters.remove(0);
        let Some(probe) = first.first().map(|f| f.path.clone()) else {
            continue;
        };
        let (same, rest): (Vec<Cluster>, Vec<Cluster>) = clusters.into_iter().partition(|c| {
            c.first()
                .is_some_and(|f| same_contents(&probe, &f.path).unwrap_or(false))
        });
        if !same.is_empty() {
            let mut class = vec![first];
            class.extend(same);
            classes.push(class);
        }
        clusters = rest;
    }
    classes
}

/// Whether two files hold the same bytes, read side by side.
fn same_contents(a: &Path, b: &Path) -> io::Result<bool> {
    const CHUNK: usize = 256 * 1024;
    let mut a = File::open(a)?;
    let mut b = File::open(b)?;
    let mut buf_a = vec![0u8; CHUNK];
    let mut buf_b = vec![0u8; CHUNK];
    loop {
        let n = read_full(&mut a, &mut buf_a)?;
        let m = read_full(&mut b, &mut buf_b)?;
        if n != m || buf_a[..n] != buf_b[..m] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

/// Reads until `buf` is full or the file ends.
fn read_full(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Whether the file is still the one that was compared: same inode, size,
/// and modification and change times.
fn unchanged(candidate: &Candidate) -> bool {
    fs::symlink_metadata(&candidate.path).is_ok_and(|now| stamp(&now) == stamp(&candidate.meta))
}

#[cfg(unix)]
fn stamp(meta: &fs::Metadata) -> (u64, u64, u64, i64, i64, i64, i64) {
    use std::os::unix::fs::MetadataExt;
    (
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    )
}

#[cfg(not(unix))]
fn stamp(meta: &fs::Metadata) -> (u64, Option<std::time::SystemTime>) {
    (meta.len(), meta.modified().ok())
}

fn dev(meta: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.dev()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0
    }
}

fn links(meta: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.nlink()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan;
    use std::fs::FileTimes;
    use std::time::{Duration, SystemTime};

    const DEP: &str = "debug/deps/libdep-0123456789abcdef.rlib";
    const DIFF: &str = "debug/deps/libdiff-0123456789abcdef.rlib";
    const SMALL: &str = "debug/deps/libsmall-0123456789abcdef.rmeta";

    fn write(path: &Path, contents: &[u8], age: Duration) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
        let time = SystemTime::now() - age;
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(time).set_accessed(time))
            .unwrap();
    }

    fn build_dir(root: &Path, diff: u8, age: Duration) -> BuildDir {
        fs::create_dir_all(root.join("debug/.fingerprint")).unwrap();
        fs::write(root.join(".rustc_info.json"), "{}").unwrap();
        let mut dep: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        write(&root.join(DEP), &dep, age);
        dep[150_000] = diff;
        write(&root.join(DIFF), &dep, age);
        write(&root.join(SMALL), &[9u8; 1000], age);
        // Incremental caches are never touched.
        write(
            &root.join("debug/incremental/app-1/s-1/query-cache.bin"),
            &[5u8; MIN_SIZE as usize],
            age,
        );
        BuildDir {
            root: root.to_path_buf(),
            profiles: scan::profiles(root),
            last_used: SystemTime::now(),
            workspaces: Default::default(),
            pinned: false,
            device: fsutil::device(root),
            compile_ms: 0,
            linked: Default::default(),
            scans: Vec::new(),
        }
    }

    fn ino(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path).unwrap().ino()
    }

    fn fixture(tmp: &Path) -> Vec<BuildDir> {
        vec![
            build_dir(&tmp.join("a/target"), 1, Duration::from_secs(7200)),
            build_dir(&tmp.join("b/target"), 2, Duration::from_secs(3600)),
        ]
    }

    #[test]
    fn finds_identical_files_and_replaces_them_with_clones() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = fixture(tmp.path());
        let a = tmp.path().join("a/target");
        let b = tmp.path().join("b/target");
        let before = [ino(&a.join(DEP)), ino(&b.join(DEP)), ino(&b.join(DIFF))];
        let b_mtime = fs::metadata(b.join(DEP)).unwrap().modified().unwrap();

        let preview = dedup(&dirs, false, None).unwrap();
        if preview.unsupported_volumes > 0 {
            // This filesystem can't clone: nothing to find or do.
            assert_eq!(preview.files, 0);
            return;
        }
        assert_eq!(preview.files, 1, "{preview:?}");
        assert_eq!(preview.bytes, 200_000);
        assert_eq!(preview.groups.len(), 1);
        assert_eq!(preview.freed, None);
        assert_eq!(
            [ino(&a.join(DEP)), ino(&b.join(DEP)), ino(&b.join(DIFF))],
            before,
            "a preview changes nothing"
        );

        let applied = dedup(&dirs, true, None).unwrap();
        assert_eq!(applied.files, 1, "{applied:?}");
        assert!(applied.freed.is_some());
        let group = &applied.groups[0];
        let replaced = &group.replaced[0];
        assert_ne!(&group.kept, replaced);
        // Contents and times are intact; the different file is untouched.
        assert_eq!(
            fs::read(a.join(DEP)).unwrap(),
            fs::read(b.join(DEP)).unwrap()
        );
        assert_eq!(fs::read(a.join(DEP)).unwrap().len(), 200_000);
        assert_eq!(
            fs::metadata(b.join(DEP)).unwrap().modified().unwrap(),
            b_mtime
        );
        assert_ne!(
            fs::read(a.join(DIFF)).unwrap(),
            fs::read(b.join(DIFF)).unwrap()
        );
        assert_eq!(ino(&b.join(DIFF)), before[2]);
        assert_eq!(ino(&a.join(DEP)), before[0], "the kept copy stays");
        assert_ne!(ino(&b.join(DEP)), before[1], "the duplicate is a new file");
        // No hard links, and no temporary files left.
        assert_eq!(links(&fs::metadata(b.join(DEP)).unwrap()), 1);
        assert!(
            fsutil::entries(&b.join("debug/deps")).all(|(n, _)| !n.contains(TEMP_MARK)),
            "temporary file left behind"
        );
        if cfg!(target_os = "macos") {
            // The copies are pure clones now, so a second run finds nothing
            // to do.
            assert!(clone::clone_id(&b.join(DEP)).is_some());
            assert_eq!(clone::clone_id(&b.join(DEP)), clone::clone_id(&a.join(DEP)));
            assert_eq!(dedup(&dirs, true, None).unwrap().files, 0);
        }
    }

    #[test]
    fn after_a_pass_only_what_changed_since_is_looked_at() {
        let tmp = tempfile::tempdir().unwrap();
        let mut dirs = fixture(tmp.path());
        if dedup(&dirs, false, None).unwrap().unsupported_volumes > 0 {
            return; // This filesystem can't clone.
        }
        let ctime = |path: &Path| changed(&fs::metadata(path).unwrap());
        let old = ctime(&tmp.path().join("a/target").join(DEP));
        // Nothing changed since: the duplicate pair isn't looked at.
        let later = old + Duration::from_secs(5);
        assert_eq!(dedup(&dirs, false, Some(later)).unwrap().files, 0);

        // A new build directory: its new file is looked at, and found to
        // duplicate the files at the same place in the others, however old.
        while SystemTime::now() < old + Duration::from_millis(1200) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let since = old + Duration::from_millis(1100);
        let c = tmp.path().join("c/target");
        dirs.push(build_dir(&c, 3, Duration::ZERO));
        let preview = dedup(&dirs, false, Some(since)).unwrap();
        assert_eq!(preview.files, 2, "{preview:?}");
        let mut paths: Vec<&PathBuf> = preview
            .groups
            .iter()
            .flat_map(|g| g.replaced.iter().chain([&g.kept]))
            .collect();
        paths.sort();
        let expected = ["a", "b", "c"].map(|d| tmp.path().join(d).join("target").join(DEP));
        assert_eq!(paths, expected.iter().collect::<Vec<_>>());
        // A full pass finds the same.
        assert_eq!(dedup(&dirs, false, None).unwrap().files, 2);
    }

    #[test]
    fn records_when_the_last_pass_started() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: tmp.path().to_path_buf(),
        };
        assert_eq!(last_pass(&paths), None);
        let started = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        record_pass(&paths, started);
        assert_eq!(last_pass(&paths), Some(started));
        // In the future (the clock went back), or unreadable: a full pass.
        record_pass(&paths, SystemTime::now() + Duration::from_secs(3600));
        assert_eq!(last_pass(&paths), None);
        fs::write(paths.dedup_stamp(), "soon\n").unwrap();
        assert_eq!(last_pass(&paths), None);

        // A cleanup's pass records itself.
        let dirs = fixture(tmp.path());
        let before = SystemTime::now() - Duration::from_secs(1);
        after_cleanup(&paths, &dirs).unwrap();
        assert!(last_pass(&paths).is_some_and(|t| t >= before));
    }

    #[test]
    fn skips_files_a_build_is_using() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = fixture(tmp.path());
        if dedup(&dirs, false, None).unwrap().unsupported_volumes > 0 {
            return;
        }
        let b = tmp.path().join("b/target");
        let before = ino(&b.join(DEP));
        let held = fsutil::lock_profile(&b.join("debug"));
        let applied = dedup(&dirs, true, None).unwrap();
        assert_eq!((applied.files, applied.busy), (0, 1), "{applied:?}");
        assert_eq!(ino(&b.join(DEP)), before);
        drop(held);
        // Child processes other tests start can briefly hold a copy of the
        // lock, so allow a few tries.
        let mut replaced = 0;
        for _ in 0..30 {
            replaced = dedup(&dirs, true, None).unwrap().files;
            if replaced > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(replaced, 1);
    }

    #[test]
    fn a_file_changed_after_comparing_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f");
        fs::write(&path, b"one").unwrap();
        let candidate = Candidate {
            path: path.clone(),
            profile: tmp.path().to_path_buf(),
            meta: fs::symlink_metadata(&path).unwrap(),
            clone_id: None,
        };
        assert!(unchanged(&candidate));
        // Same size, and the modification time put back: the change time
        // still gives it away.
        let mtime = candidate.meta.modified().unwrap();
        fs::write(&path, b"two").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(mtime))
            .unwrap();
        assert!(!unchanged(&candidate));
    }

    #[test]
    fn compares_contents_across_chunks() {
        let tmp = tempfile::tempdir().unwrap();
        let big: Vec<u8> = (0..700_000u32).map(|i| (i % 253) as u8).collect();
        let mut other = big.clone();
        other[600_000] ^= 1;
        let paths = ["a", "b", "c"].map(|n| tmp.path().join(n));
        fs::write(&paths[0], &big).unwrap();
        fs::write(&paths[1], &big).unwrap();
        fs::write(&paths[2], &other).unwrap();
        assert!(same_contents(&paths[0], &paths[1]).unwrap());
        assert!(!same_contents(&paths[0], &paths[2]).unwrap());
    }

    #[test]
    fn report_lists_groups_and_every_skip_reason() {
        let summary = Summary {
            groups: vec![Group {
                size: 2048,
                kept: PathBuf::from("/a/kept"),
                replaced: vec![PathBuf::from("/b/dup")],
            }],
            files: 1,
            bytes: 2048,
            busy: 2,
            skipped: 3,
            unsupported_volumes: 4,
            freed: None,
        };
        let text = report(&summary, 2, false, true);
        assert!(text.contains("/a/kept"), "{text}");
        assert!(text.contains("= /b/dup"), "{text}");
        assert!(text.contains("Would replace 1 duplicate files"), "{text}");
        assert!(text.contains("in 1 groups"), "{text}");
        assert!(
            text.contains("Skipped 2 files in build directories a build"),
            "{text}"
        );
        assert!(text.contains("Skipped 3 files that changed"), "{text}");
        assert!(text.contains("Skipped 4 volume(s)"), "{text}");
        assert!(text.contains("dedup --apply"), "{text}");

        // Without --verbose the groups aren't listed.
        let quiet = report(&summary, 2, false, false);
        assert!(!quiet.contains("/a/kept"), "{quiet}");
    }

    #[test]
    fn report_states_the_change_in_free_space_after_applying() {
        let mut summary = Summary {
            files: 1,
            bytes: 2048,
            freed: Some(4096),
            ..Summary::default()
        };
        let up = report(&summary, 2, true, false);
        assert!(up.contains("Replaced 1 duplicate files"), "{up}");
        assert!(up.contains("Free space went up by"), "{up}");
        assert!(!up.contains("--apply"), "{up}");
        summary.freed = Some(-4096);
        let down = report(&summary, 2, true, false);
        assert!(down.contains("Free space went down by"), "{down}");
    }

    #[test]
    fn report_with_nothing_to_replace_says_so_and_nothing_else() {
        let text = report(&Summary::default(), 3, false, true);
        assert_eq!(
            text,
            "No duplicate files to replace in 3 build directories.\n"
        );
    }

    #[test]
    fn dirs_without_a_device_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let mut dirs = fixture(tmp.path());
        for dir in &mut dirs {
            dir.device = None;
        }
        let summary = dedup(&dirs, false, None).unwrap();
        assert_eq!(summary.files, 0);
        assert_eq!(summary.unsupported_volumes, 0);
    }

    #[test]
    fn a_file_listed_twice_is_not_its_own_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = fixture(tmp.path());
        // The same build dir twice: every file is seen twice.
        let twice = vec![dirs[0].clone(), dirs[0].clone()];
        let summary = dedup(&twice, false, None).unwrap();
        assert_eq!(summary.files, 0);
    }

    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn candidate(path: &Path, profile: &Path) -> Candidate {
        Candidate {
            path: path.to_path_buf(),
            profile: profile.to_path_buf(),
            meta: fs::symlink_metadata(path).unwrap(),
            clone_id: None,
        }
    }

    #[test]
    fn a_volume_that_cannot_clone_is_skipped_once() {
        let tmp = tempfile::tempdir().unwrap();
        let mut dirs = fixture(tmp.path());
        // Gone since it was found: it can't be asked whether it clones.
        let gone = tmp.path().join("gone");
        for dir in &mut dirs {
            dir.root = gone.clone();
        }
        let summary = dedup(&dirs, false, None).unwrap();
        assert_eq!(summary.unsupported_volumes, 1);
        assert_eq!(summary.files, 0);
    }

    #[test]
    fn replace_skips_a_duplicate_changed_since_comparing() {
        let tmp = tempfile::tempdir().unwrap();
        let kept = tmp.path().join("kept");
        let dup = tmp.path().join("dup");
        fs::write(&kept, b"same").unwrap();
        fs::write(&dup, b"same").unwrap();
        let class = vec![
            vec![candidate(&kept, tmp.path())],
            vec![candidate(&dup, tmp.path())],
        ];
        fs::write(&dup, b"diff").unwrap();
        let mut summary = Summary::default();
        let group = replace_class(class, true, &mut summary).unwrap();
        assert!(group.replaced.is_empty());
        assert_eq!((summary.skipped, summary.files), (1, 0));
        assert_eq!(fs::read(&dup).unwrap(), b"diff");
    }

    #[test]
    fn replace_skips_a_path_without_a_file_name() {
        let tmp = tempfile::tempdir().unwrap();
        let kept = tmp.path().join("kept");
        fs::write(&kept, b"same").unwrap();
        let root = candidate(Path::new("/"), tmp.path());
        assert!(matches!(
            replace(&candidate(&kept, tmp.path()), &root),
            Replaced::Skipped
        ));
    }

    #[test]
    fn replace_skips_when_the_clone_cannot_be_made() {
        let tmp = tempfile::tempdir().unwrap();
        let kept = tmp.path().join("kept");
        fs::write(&kept, b"same").unwrap();
        let ro = tmp.path().join("ro");
        fs::create_dir(&ro).unwrap();
        let dup = ro.join("dup");
        fs::write(&dup, b"same").unwrap();
        let (k, d) = (candidate(&kept, tmp.path()), candidate(&dup, tmp.path()));
        set_mode(&ro, 0o555);
        let replaced = replace(&k, &d);
        set_mode(&ro, 0o755);
        assert!(matches!(replaced, Replaced::Skipped));
        assert_eq!(fsutil::entries(&ro).count(), 1, "nothing left behind");
    }

    #[test]
    fn replace_skips_when_the_clone_cannot_take_the_duplicates_place() {
        let tmp = tempfile::tempdir().unwrap();
        let kept = tmp.path().join("kept");
        fs::write(&kept, b"same").unwrap();
        // A non-empty directory where the duplicate was: a file can't
        // be renamed over it.
        let dup = tmp.path().join("dup");
        fs::create_dir(&dup).unwrap();
        fs::write(dup.join("inside"), b"x").unwrap();
        let replaced = replace(&candidate(&kept, tmp.path()), &candidate(&dup, tmp.path()));
        assert!(matches!(replaced, Replaced::Skipped));
        assert!(dup.join("inside").exists());
        assert!(
            fsutil::entries(tmp.path()).all(|(n, _)| !n.contains(TEMP_MARK)),
            "temporary file left behind"
        );
    }

    #[test]
    fn files_that_cannot_be_examined_are_not_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("debug");
        let hidden = profile.join("deps");
        fs::create_dir_all(&hidden).unwrap();
        fs::write(hidden.join("big"), vec![1u8; MIN_SIZE as usize]).unwrap();
        fs::write(profile.join("seen"), vec![1u8; MIN_SIZE as usize]).unwrap();
        // Listable, but its entries can't be looked up.
        set_mode(&hidden, 0o444);
        let found = collect(&profile);
        set_mode(&hidden, 0o755);
        let names: Vec<&Path> = found.iter().map(|c| c.path.as_path()).collect();
        assert_eq!(names, [profile.join("seen").as_path()]);
    }

    #[test]
    fn a_read_error_is_not_a_match() {
        let tmp = tempfile::tempdir().unwrap();
        let mut dir = File::open(tmp.path()).unwrap();
        assert!(read_full(&mut dir, &mut [0u8; 8]).is_err());
        assert!(same_contents(tmp.path(), tmp.path()).is_err());
    }
}
