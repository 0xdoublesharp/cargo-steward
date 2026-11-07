//! Filesystem helpers: sizes, timestamps, free space, cargo's locks,
//! deletion that never leaves a half-removed directory in place, and
//! bounded parallel work over many directories.

mod unshared;

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::SystemTime;

pub use unshared::private_size;

/// The most threads [`parallel_map`] uses. Directory listing and stat calls
/// stop scaling well past this on the disks build output lives on.
const MAX_WORKERS: usize = 8;

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
    let mut total = 0;
    walk_files(path, |_, meta| total += allocated(meta));
    total
}

/// What a directory tree takes up, counted two ways.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    /// As `du` counts it: every file's blocks, shared or not.
    pub allocated: u64,
    /// Only blocks no copy-on-write clone shares ([`private_size`]). `None`
    /// where the filesystem can't tell.
    pub private: Option<u64>,
}

/// [`Usage`] of everything under `path`, in one walk. Removing the tree
/// frees at least `private` and at most `allocated`: blocks its own files
/// share with each other count in neither's private size.
pub fn tree_usage(path: &Path) -> Usage {
    let mut allocated_total = 0;
    let mut private_total = 0;
    let mut supported = false;
    let mut files = 0usize;
    walk_files(path, |file, meta| {
        let bytes = allocated(meta);
        allocated_total += bytes;
        files += 1;
        if !meta.is_file() {
            // Symlinks and special files take a block at most, unshared.
            private_total += bytes;
            return;
        }
        match private_size(file) {
            Some(private) => {
                supported = true;
                private_total += private.min(bytes);
            }
            // Gone since it was listed, or unreadable: count it in full.
            None => private_total += bytes,
        }
    });
    Usage {
        allocated: allocated_total,
        private: (supported || files == 0).then_some(private_total),
    }
}

/// Bytes under `path` that no copy-on-write clone shares; see [`tree_usage`].
#[allow(dead_code)] // Library surface for callers that only need this half.
pub fn tree_private_size(path: &Path) -> Option<u64> {
    tree_usage(path).private
}

/// Calls `visit` with every non-directory under `path` (`path` itself if it
/// isn't a directory), not following symlinks and visiting each hard-linked
/// file once.
fn walk_files(path: &Path, mut visit: impl FnMut(&Path, &fs::Metadata)) {
    let mut seen = HashSet::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&p) else {
            continue;
        };
        if meta.is_dir() {
            stack.extend(entries(&p).map(|(_, child)| child));
            continue;
        }
        if first_link(&meta, &mut seen) {
            visit(&p, &meta);
        }
    }
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

/// [`free_space`] of the volume holding `path`, or once `path` is gone, of
/// its nearest remaining ancestor.
pub fn free_space_near(path: &Path) -> Option<u64> {
    path.ancestors().find_map(free_space)
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

/// Runs `f` on each of `items` on up to [`MAX_WORKERS`] threads (fewer if the
/// machine has fewer cores), and returns the results in the items' order.
/// Items are handed out one at a time, so one slow item doesn't hold up a
/// whole share of the others. A panic in `f` is passed on to the caller.
pub fn parallel_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let workers = thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_WORKERS)
        .min(items.len());
    if workers <= 1 {
        return items.iter().map(f).collect();
    }
    let next = AtomicUsize::new(0);
    let mut done: Vec<(usize, R)> = thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(i) else {
                            break;
                        };
                        out.push((i, f(item)));
                    }
                    out
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| {
                h.join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    });
    done.sort_by_key(|(i, _)| *i);
    done.into_iter().map(|(_, r)| r).collect()
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
        // A process another test thread forks at the wrong moment can hold a
        // copy of the descriptor until it execs, so allow a moment.
        let freed = (0..50).any(|_| {
            let held = matches!(lock_profile(tmp.path()), Lock::Held(_));
            if !held {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            held
        });
        assert!(freed, "the lock was never released");
    }

    #[test]
    fn parallel_map_keeps_the_order_of_its_items() {
        let items: Vec<u64> = (0..100).collect();
        let doubled = parallel_map(&items, |n| {
            // Uneven work, so items finish out of order.
            std::thread::sleep(std::time::Duration::from_micros((n % 7) * 50));
            n * 2
        });
        assert_eq!(doubled, items.iter().map(|n| n * 2).collect::<Vec<_>>());
        assert!(parallel_map(&[] as &[u8], |_| 0).is_empty());
    }

    #[test]
    fn free_space_falls_back_to_an_ancestor_once_a_path_is_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let gone = tmp.path().join("removed/deeper");
        assert!(free_space(&gone).is_none() || cfg!(not(unix)));
        if cfg!(unix) {
            assert!(free_space_near(&gone).is_some());
        }
    }

    #[test]
    fn usage_counts_both_ways() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a"), vec![1u8; 256 * 1024]).unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub/b"), vec![2u8; 64 * 1024]).unwrap();
        // Flush so the blocks are allocated before either measurement; APFS
        // can allocate delayed writes between them.
        for f in ["a", "sub/b"] {
            fs::File::open(tmp.path().join(f))
                .unwrap()
                .sync_all()
                .unwrap();
        }
        let usage = tree_usage(tmp.path());
        assert_eq!(usage.allocated, tree_size(tmp.path()));
        if let Some(private) = usage.private {
            // Nothing here is a clone: every block is private.
            assert_eq!(private, usage.allocated);
        }
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            tree_usage(empty.path()),
            Usage {
                allocated: 0,
                private: Some(0)
            }
        );
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(3 << 29), "1.5 GiB");
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn usage_counts_symlinks_as_private() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a"), vec![1u8; 64 * 1024]).unwrap();
        std::os::unix::fs::symlink("a", tmp.path().join("link")).unwrap();
        let usage = tree_usage(tmp.path());
        assert_eq!(usage.allocated, tree_size(tmp.path()));
        assert!(
            usage.private.is_none_or(|p| p == usage.allocated),
            "{usage:?}"
        );
        assert_eq!(tree_private_size(tmp.path()), usage.private);
    }

    #[cfg(unix)]
    #[test]
    fn walks_past_entries_it_cannot_look_up() {
        let tmp = tempfile::tempdir().unwrap();
        let hidden = tmp.path().join("hidden");
        fs::create_dir(&hidden).unwrap();
        fs::write(hidden.join("a"), vec![1u8; 64 * 1024]).unwrap();
        // Listable, but its entries can't be looked up.
        set_mode(&hidden, 0o444);
        let mut visited = Vec::new();
        walk_files(tmp.path(), |p, _| visited.push(p.to_path_buf()));
        set_mode(&hidden, 0o755);
        assert!(visited.is_empty(), "{visited:?}");
    }

    #[test]
    fn a_profile_whose_lock_files_cannot_be_made_is_not_busy() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing");
        assert!(matches!(lock_profile(&missing), Lock::Held(held) if held.is_empty()));
    }

    #[test]
    fn remove_reports_a_path_it_cannot_look_up() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("file");
        fs::write(&file, "x").unwrap();
        // A path through a file isn't missing, it's an error.
        let err = remove(&file.join("below")).unwrap_err();
        assert_ne!(err.kind(), io::ErrorKind::NotFound);
        assert!(file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn remove_empties_a_directory_it_cannot_rename_aside() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("parent");
        let dir = parent.join("victim");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/file"), "x").unwrap();
        set_mode(&parent, 0o555);
        let result = remove(&dir);
        set_mode(&parent, 0o755);
        // Deleted in place as far as the read-only parent allows.
        assert!(result.is_err());
        assert_eq!(entries(&dir).count(), 0);
        assert_eq!(entries(&parent).count(), 1, "no trash left behind");
    }

    #[test]
    fn sweep_trash_finishes_interrupted_removals_only() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = tmp.path().join(format!("{TRASH_PREFIX}old-1-0"));
        fs::create_dir_all(trash.join("deep")).unwrap();
        fs::write(trash.join("deep/file"), "x").unwrap();
        fs::create_dir(tmp.path().join("kept")).unwrap();
        sweep_trash(tmp.path());
        assert!(!trash.exists());
        assert!(tmp.path().join("kept").exists());
    }
}
