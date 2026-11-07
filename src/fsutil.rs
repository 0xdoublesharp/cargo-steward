//! Filesystem helpers: sizes, timestamps, free space, cargo's locks, and
//! deletion that never leaves a half-removed directory in place.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

/// When a file was last used: its newest access or modification time. A
/// directory's access time only records someone listing it (including us),
/// so for directories this is the modification time.
pub fn touched(meta: &fs::Metadata) -> SystemTime {
    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    if meta.is_dir() {
        return modified;
    }
    meta.accessed().map_or(modified, |a| a.max(modified))
}

/// Disk space a file takes up, as `du` counts it. Blocks shared with APFS or
/// btrfs clones are counted in full, so this can overstate what removing the
/// file gives back; free space on the volume is the measure that can't lie.
pub fn allocated(meta: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.blocks() * 512
    }
    #[cfg(not(unix))]
    {
        meta.len()
    }
}

/// [`allocated`] size of everything under `path`, counting hard links once
/// and not following symlinks.
pub fn tree_size(path: &Path) -> u64 {
    let mut seen = HashSet::new();
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&p) else {
            continue;
        };
        if meta.is_dir() {
            stack.extend(entries(&p).map(|(_, child)| child));
            continue;
        }
        if !first_link(&meta, &mut seen) {
            continue;
        }
        total += allocated(&meta);
    }
    total
}

/// Whether this is the first time a hard-linked file is seen. Files with a
/// single link always count.
fn first_link(meta: &fs::Metadata, seen: &mut HashSet<(u64, u64)>) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.nlink() <= 1 || seen.insert((meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = (meta, seen);
        true
    }
}

/// Space that removing all of `paths` gives back, as far as hard links go: a
/// file that still has a link elsewhere stays on disk.
pub fn freed_by(paths: &[PathBuf]) -> u64 {
    #[cfg(unix)]
    {
        use std::collections::HashMap;
        use std::os::unix::fs::MetadataExt;
        let mut files: HashMap<(u64, u64), (u64, u64, u64)> = HashMap::new();
        for meta in paths.iter().filter_map(|p| fs::symlink_metadata(p).ok()) {
            let file = files.entry((meta.dev(), meta.ino())).or_insert((
                0,
                meta.nlink(),
                allocated(&meta),
            ));
            file.0 += 1;
        }
        files
            .values()
            .filter(|(removed, links, _)| removed >= links)
            .map(|(_, _, bytes)| bytes)
            .sum()
    }
    #[cfg(not(unix))]
    {
        paths
            .iter()
            .filter_map(|p| fs::symlink_metadata(p).ok())
            .map(|m| allocated(&m))
            .sum()
    }
}

/// Bytes available to unprivileged users on the volume holding `path`.
pub fn free_space(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c_path` is a valid NUL-terminated string and `stat` is a
        // writable statvfs that outlives the call.
        if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)]
        Some(stat.f_bavail as u64 * stat.f_frsize as u64)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Identifies the volume a path is on, so build directories can be grouped
/// by the disk whose space they share.
pub fn device(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path).ok().map(|m| m.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Cargo's locks on a build profile directory, held exclusively.
#[derive(Debug)]
pub enum Lock {
    Held(#[allow(dead_code)] Vec<File>),
    /// A cargo process is building there.
    Busy,
}

/// Takes every lock cargo uses on a profile directory without waiting.
///
/// Cargo before 1.96 holds `.cargo-lock` exclusively while it builds. Later
/// versions hold `.cargo-build-lock` exclusively (shared with
/// `-Zfine-grain-locking`), `.cargo-artifact-lock` exclusively, and
/// `.cargo-lock` shared, so that older cargos wait for them. Holding all of
/// them exclusively excludes every version.
pub fn lock_profile(profile: &Path) -> Lock {
    let mut held = Vec::new();
    for name in [".cargo-build-lock", ".cargo-artifact-lock", ".cargo-lock"] {
        let Ok(file) = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(profile.join(name))
        else {
            continue;
        };
        match file.try_lock() {
            Ok(()) => held.push(file),
            Err(TryLockError::WouldBlock) => return Lock::Busy,
            // Filesystems without lock support: carry on, as cargo does.
            Err(TryLockError::Error(_)) => held.push(file),
        }
    }
    Lock::Held(held)
}

/// Takes an exclusive lock on `path`, creating it. `None` when another
/// process holds it.
pub fn try_lock_file(path: &Path) -> io::Result<Option<File>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(_)) => Ok(Some(file)),
    }
}

/// Prefix of directories renamed aside for deletion.
pub const TRASH_PREFIX: &str = ".steward-trash-";

/// Removes a file or directory tree. A directory is first renamed aside, so
/// nothing ever sees it half deleted; a missing path is not an error.
pub fn remove(path: &Path) -> io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !meta.is_dir() {
        return match fs::remove_file(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        };
    }
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let trash = path.with_file_name(format!(
        "{TRASH_PREFIX}{name}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    match fs::rename(path, &trash) {
        Ok(()) => fs::remove_dir_all(&trash),
        Err(_) => fs::remove_dir_all(path),
    }
}

/// Finishes deletions that an interrupted cleanup left renamed aside in `dir`.
pub fn sweep_trash(dir: &Path) {
    for (name, path) in entries(dir) {
        if name.starts_with(TRASH_PREFIX) {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

/// `(file name, path)` of each entry in `dir`; nothing if it can't be read.
pub fn entries(dir: &Path) -> impl Iterator<Item = (String, PathBuf)> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
}

/// Subdirectories of `dir`, not following symlinks.
pub fn subdirs(dir: &Path) -> impl Iterator<Item = PathBuf> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
}

/// Sizes the way people read them: `1.5 GiB`.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// How long ago `time` was, roughly: `40m`, `5h`, `12d`.
pub fn ago(time: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(time)
        .unwrap_or_default()
        .as_secs();
    match secs {
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 2 * 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_directories_without_leaving_trash() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("victim");
        fs::create_dir_all(dir.join("a/b")).unwrap();
        fs::write(dir.join("a/b/file"), "x").unwrap();
        remove(&dir).unwrap();
        assert!(!dir.exists());
        assert_eq!(entries(tmp.path()).count(), 0);
        // Already gone is fine.
        remove(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn counts_hard_links_once_and_frees_only_unlinked_files() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        fs::write(&a, vec![1u8; 64 * 1024]).unwrap();
        let single = tree_size(tmp.path());
        fs::hard_link(&a, tmp.path().join("b")).unwrap();
        assert_eq!(tree_size(tmp.path()), single);
        assert_eq!(freed_by(std::slice::from_ref(&a)), 0);
        assert_eq!(freed_by(&[a, tmp.path().join("b")]), single);
    }

    #[test]
    fn a_second_lock_on_a_profile_is_busy() {
        let tmp = tempfile::tempdir().unwrap();
        let first = lock_profile(tmp.path());
        assert!(matches!(first, Lock::Held(_)));
        assert!(matches!(lock_profile(tmp.path()), Lock::Busy));
        drop(first);
        assert!(matches!(lock_profile(tmp.path()), Lock::Held(_)));
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(3 << 29), "1.5 GiB");
    }
}
