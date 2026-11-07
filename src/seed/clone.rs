//! Copy-on-write clones of files and directory trees.
//!
//! A clone shares its data blocks with the original until either is written,
//! so it costs no space and almost no time. Cargo decides what to rebuild by
//! comparing file modification times, so every clone keeps the original's
//! times and permissions.
//!
//! - macOS (APFS): `clonefile(2)` clones a whole tree in one call.
//! - Linux (btrfs, XFS, bcachefs): the tree is walked and each file cloned
//!   with the `FICLONE` ioctl.
//!
//! Where the filesystem can't clone, or the source and destination are on
//! different volumes, the result is [`Cloned::Unsupported`] and nothing is
//! left behind. There is deliberately no fallback to copying bytes: a copy
//! of a build directory costs as much space as the original and takes
//! minutes.

use std::fs::{self, File, FileTimes};
use std::io;
use std::path::Path;

/// What a clone did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cloned {
    /// The destination now exists and shares the source's blocks.
    Done,
    /// The filesystem can't clone, or source and destination are on
    /// different volumes. Nothing was created.
    Unsupported,
}

/// Clones the directory tree (or file) `src` to `dst`, which must not exist.
/// Symlinks are recreated, not followed; sockets and FIFOs are skipped.
pub(crate) fn clone_tree(src: &Path, dst: &Path) -> io::Result<Cloned> {
    imp::clone_tree(src, dst)
}

/// Clones the regular file `src` to `dst`, which must not exist, keeping
/// `src`'s access and modification times and permissions.
pub(crate) fn clone_file(src: &Path, dst: &Path) -> io::Result<Cloned> {
    imp::clone_file(src, dst)
}

/// Gives `path` the access and modification times and the permissions in
/// `like`. Times come first: changing permissions doesn't touch them.
pub(crate) fn set_times_and_mode(path: &Path, like: &fs::Metadata) -> io::Result<()> {
    let mut times = FileTimes::new();
    if let Ok(accessed) = like.accessed() {
        times = times.set_accessed(accessed);
    }
    if let Ok(modified) = like.modified() {
        times = times.set_modified(modified);
    }
    File::open(path)?.set_times(times)?;
    fs::set_permissions(path, like.permissions())
}

/// Whether an error means "this filesystem can't clone here" rather than a
/// real failure.
pub(crate) fn is_unsupported(err: &io::Error) -> bool {
    #[cfg(unix)]
    {
        // Not a match pattern: ENOTSUP and EOPNOTSUPP are equal on Linux.
        err.raw_os_error()
            .is_some_and(|c| c == libc::EXDEV || c == libc::ENOTSUP || c == libc::EOPNOTSUPP)
    }
    #[cfg(not(unix))]
    {
        err.kind() == io::ErrorKind::Unsupported
    }
}

/// Whether the filesystem holding `path` is one that can clone files: APFS
/// on macOS; btrfs, XFS or bcachefs on Linux. A quick check before work
/// that only pays off if clones are possible; the clone itself has the
/// final say.
pub(crate) fn may_clone(path: &Path) -> bool {
    imp::may_clone(path)
}

/// Identifies the data `path` holds on APFS: files that are pure clones of
/// one another (sharing every block) have the same ID, and writing to a
/// clone gives it a new one. `None` where this isn't known.
pub(crate) fn clone_id(path: &Path) -> Option<u64> {
    imp::clone_id(path)
}

#[cfg(unix)]
fn c_path(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

#[cfg(target_os = "macos")]
mod imp {
    use std::io;
    use std::path::Path;

    use super::{Cloned, c_path, is_unsupported, set_times_and_mode};

    /// `<sys/clonefile.h>`: don't follow a symlink at `src`.
    const CLONE_NOFOLLOW: u32 = 0x0001;

    pub(super) fn clone_tree(src: &Path, dst: &Path) -> io::Result<Cloned> {
        // clonefile copies times, permissions and extended attributes along
        // with the tree.
        clonefile(src, dst)
    }

    pub(super) fn clone_file(src: &Path, dst: &Path) -> io::Result<Cloned> {
        let meta = std::fs::symlink_metadata(src)?;
        if !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} isn't a regular file", src.display()),
            ));
        }
        if clonefile(src, dst)? == Cloned::Unsupported {
            return Ok(Cloned::Unsupported);
        }
        // clonefile already keeps the times; set them anyway so the result
        // doesn't depend on that.
        if let Err(e) = set_times_and_mode(dst, &meta) {
            let _ = std::fs::remove_file(dst);
            return Err(e);
        }
        Ok(Cloned::Done)
    }

    fn clonefile(src: &Path, dst: &Path) -> io::Result<Cloned> {
        let c_src = c_path(src)?;
        let c_dst = c_path(dst)?;
        // SAFETY: both are valid NUL-terminated paths that outlive the call.
        let rc = unsafe { libc::clonefile(c_src.as_ptr(), c_dst.as_ptr(), CLONE_NOFOLLOW) };
        if rc == 0 {
            return Ok(Cloned::Done);
        }
        let err = io::Error::last_os_error();
        if is_unsupported(&err) {
            Ok(Cloned::Unsupported)
        } else {
            Err(err)
        }
    }

    pub(super) fn may_clone(path: &Path) -> bool {
        let Ok(c) = c_path(path) else {
            return false;
        };
        // SAFETY: an all-zero statfs is a valid value of this plain C struct.
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c` is a valid NUL-terminated path and `stat` is writable
        // and outlives the call.
        if unsafe { libc::statfs(c.as_ptr(), &mut stat) } != 0 {
            return false;
        }
        let name: Vec<u8> = stat
            .f_fstypename
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| b as u8)
            .collect();
        name == b"apfs"
    }

    pub(super) fn clone_id(path: &Path) -> Option<u64> {
        let c = c_path(path).ok()?;
        let mut list = libc::attrlist {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: libc::ATTR_CMN_RETURNED_ATTRS,
            volattr: 0,
            dirattr: 0,
            fileattr: 0,
            forkattr: libc::ATTR_CMNEXT_CLONEID,
        };
        // u32 length, a 20-byte attribute_set_t of what was returned, then
        // the clone ID as a u64.
        let mut buf = [0u8; 64];
        // SAFETY: `c` is a valid path, `list` a valid attrlist, and `buf` is
        // writable for the length passed; all outlive the call.
        let rc = unsafe {
            libc::getattrlist(
                c.as_ptr(),
                (&raw mut list).cast(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                libc::FSOPT_NOFOLLOW | libc::FSOPT_ATTR_CMN_EXTENDED,
            )
        };
        if rc != 0 {
            return None;
        }
        let word = |at: usize| buf.get(at..at + 4).and_then(|b| b.try_into().ok());
        let length = u32::from_ne_bytes(word(0)?) as usize;
        // attribute_set_t: commonattr, volattr, dirattr, fileattr, forkattr.
        let returned_fork = u32::from_ne_bytes(word(4 + 16)?);
        if returned_fork & libc::ATTR_CMNEXT_CLONEID == 0 || length < 32 {
            return None;
        }
        Some(u64::from_ne_bytes(buf.get(24..32)?.try_into().ok()?))
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::fs::{self, File, OpenOptions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    use super::{Cloned, c_path, is_unsupported, set_times_and_mode};

    pub(super) fn clone_tree(src: &Path, dst: &Path) -> io::Result<Cloned> {
        let meta = fs::symlink_metadata(src)?;
        if !meta.is_dir() {
            return clone_entry(src, dst, &meta);
        }
        // Fails if `dst` exists, so the cleanup below only ever removes
        // what this call made.
        fs::create_dir(dst)?;
        match fill_dir(src, dst, &meta) {
            Ok(Cloned::Done) => Ok(Cloned::Done),
            other => {
                // Leave nothing half made.
                let _ = fs::remove_dir_all(dst);
                other
            }
        }
    }

    /// Clones the contents of directory `src` into the new directory `dst`.
    fn fill_dir(src: &Path, dst: &Path, meta: &fs::Metadata) -> io::Result<Cloned> {
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let from = entry.path();
            let to = dst.join(entry.file_name());
            let child = match fs::symlink_metadata(&from) {
                Ok(m) => m,
                // Removed while we were looking: nothing to clone.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let cloned = if child.is_dir() {
                fs::create_dir(&to)?;
                fill_dir(&from, &to, &child)?
            } else {
                match clone_entry(&from, &to, &child) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    other => other?,
                }
            };
            if cloned == Cloned::Unsupported {
                return Ok(Cloned::Unsupported);
            }
        }
        // Last, since creating the children changed the directory's times.
        set_times_and_mode(dst, meta)?;
        Ok(Cloned::Done)
    }

    /// Clones one non-directory entry: a file, or a symlink.
    fn clone_entry(src: &Path, dst: &Path, meta: &fs::Metadata) -> io::Result<Cloned> {
        if meta.file_type().is_symlink() {
            // Cargo never looks at a symlink's own times.
            std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
            return Ok(Cloned::Done);
        }
        if !meta.is_file() {
            // Sockets, FIFOs and devices have no data to share.
            return Ok(Cloned::Done);
        }
        clone_file(src, dst)
    }

    pub(super) fn clone_file(src: &Path, dst: &Path) -> io::Result<Cloned> {
        let from = File::open(src)?;
        let meta = from.metadata()?;
        if !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} isn't a regular file", src.display()),
            ));
        }
        let to = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dst)?;
        // SAFETY: both descriptors are open for the duration of the call;
        // FICLONE takes the source descriptor as its argument.
        let rc = unsafe { libc::ioctl(to.as_raw_fd(), libc::FICLONE, from.as_raw_fd()) };
        let result = if rc == 0 {
            drop(to);
            set_times_and_mode(dst, &meta).map(|()| Cloned::Done)
        } else {
            let err = io::Error::last_os_error();
            // EINVAL: the filesystem can't share these files' blocks;
            // ENOTTY: it has no clone ioctl at all.
            if is_unsupported(&err)
                || matches!(err.raw_os_error(), Some(libc::EINVAL | libc::ENOTTY))
            {
                Ok(Cloned::Unsupported)
            } else {
                Err(err)
            }
        };
        if !matches!(result, Ok(Cloned::Done)) {
            let _ = fs::remove_file(dst);
        }
        result
    }

    pub(super) fn may_clone(path: &Path) -> bool {
        const BTRFS: i64 = 0x9123_683e;
        const XFS: i64 = 0x5846_5342;
        const BCACHEFS: i64 = 0xca45_1a4e;
        let Ok(c) = c_path(path) else {
            return false;
        };
        // SAFETY: an all-zero statfs is a valid value of this plain C struct.
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c` is a valid NUL-terminated path and `stat` is writable
        // and outlives the call.
        if unsafe { libc::statfs(c.as_ptr(), &mut stat) } != 0 {
            return false;
        }
        #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
        let kind = i64::from(stat.f_type as u32);
        [BTRFS, XFS, BCACHEFS].contains(&kind)
    }

    pub(super) fn clone_id(_path: &Path) -> Option<u64> {
        None
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use std::io;
    use std::path::Path;

    use super::Cloned;

    pub(super) fn clone_tree(_src: &Path, _dst: &Path) -> io::Result<Cloned> {
        Ok(Cloned::Unsupported)
    }

    pub(super) fn clone_file(_src: &Path, _dst: &Path) -> io::Result<Cloned> {
        Ok(Cloned::Unsupported)
    }

    pub(super) fn may_clone(_path: &Path) -> bool {
        false
    }

    pub(super) fn clone_id(_path: &Path) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn set_mtime(path: &Path, time: SystemTime) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(time).set_accessed(time))
            .unwrap();
    }

    fn mtime(path: &Path) -> SystemTime {
        fs::symlink_metadata(path).unwrap().modified().unwrap()
    }

    /// A small tree with old timestamps, a nested file and a symlink.
    fn fixture(root: &Path) -> SystemTime {
        let old = SystemTime::now() - Duration::from_secs(86_400);
        fs::create_dir_all(root.join("debug/deps")).unwrap();
        fs::write(
            root.join("debug/deps/libdep-0123456789abcdef.rlib"),
            vec![7u8; 100_000],
        )
        .unwrap();
        fs::write(root.join(".rustc_info.json"), "{}").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("deps", root.join("debug/link")).unwrap();
        set_mtime(&root.join("debug/deps/libdep-0123456789abcdef.rlib"), old);
        old
    }

    #[test]
    fn clones_a_tree_keeping_contents_and_times() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let old = fixture(&src);
        let dst = tmp.path().join("dst");
        match clone_tree(&src, &dst).unwrap() {
            Cloned::Unsupported => {
                assert!(!may_clone(tmp.path()) || cfg!(target_os = "linux"));
                assert!(!dst.exists(), "an unsupported clone leaves nothing");
                return;
            }
            Cloned::Done => {}
        }
        let rlib = PathBuf::from("debug/deps/libdep-0123456789abcdef.rlib");
        assert_eq!(fs::read(dst.join(&rlib)).unwrap(), vec![7u8; 100_000]);
        assert_eq!(mtime(&dst.join(&rlib)), old);
        assert_eq!(fs::read(dst.join(".rustc_info.json")).unwrap(), b"{}");
        #[cfg(unix)]
        assert_eq!(
            fs::read_link(dst.join("debug/link")).unwrap(),
            PathBuf::from("deps")
        );
        // The clone is independent of the original.
        fs::write(dst.join(&rlib), b"changed").unwrap();
        assert_eq!(fs::read(src.join(&rlib)).unwrap().len(), 100_000);
        // A destination that exists is an error, not an overwrite.
        assert!(clone_tree(&src, &dst).is_err());
    }

    #[test]
    fn clones_a_file_keeping_times_and_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a");
        fs::write(&src, vec![3u8; 70_000]).unwrap();
        let old = SystemTime::now() - Duration::from_secs(3600);
        set_mtime(&src, old);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&src, fs::Permissions::from_mode(0o751)).unwrap();
        }
        let dst = tmp.path().join("b");
        if clone_file(&src, &dst).unwrap() == Cloned::Unsupported {
            assert!(!dst.exists());
            return;
        }
        assert_eq!(fs::read(&dst).unwrap(), vec![3u8; 70_000]);
        assert_eq!(mtime(&dst), old);
        assert_eq!(
            fs::metadata(&dst).unwrap().permissions(),
            fs::metadata(&src).unwrap().permissions()
        );
        if cfg!(target_os = "macos") && may_clone(tmp.path()) {
            // A pure clone of the original, until it's written to.
            assert!(clone_id(&dst).is_some());
            assert_eq!(clone_id(&dst), clone_id(&src));
            fs::write(&dst, b"changed").unwrap();
            assert_ne!(clone_id(&dst), clone_id(&src));
        }
    }

    #[cfg(unix)]
    #[test]
    fn classifies_unsupported_errors() {
        for code in [libc::EXDEV, libc::ENOTSUP, libc::EOPNOTSUPP] {
            assert!(is_unsupported(&io::Error::from_raw_os_error(code)));
        }
        assert!(!is_unsupported(&io::Error::from_raw_os_error(libc::ENOENT)));
        assert!(!is_unsupported(&io::Error::from_raw_os_error(libc::EEXIST)));
    }

    /// Across volumes nothing is cloned and nothing is copied either.
    #[cfg(unix)]
    #[test]
    fn cross_volume_clones_are_unsupported() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fixture(&src);
        let here = fs::metadata(tmp.path()).unwrap().dev();
        // Somewhere writable on another volume; skip if there's none.
        // Not the home directory: a test mustn't write there. On macOS,
        // cloning into devfs fails before anything is created.
        let candidates: &[&str] = if cfg!(target_os = "macos") {
            &["/dev"]
        } else {
            &["/dev/shm"]
        };
        let Some(other) = candidates
            .iter()
            .map(PathBuf::from)
            .find(|p| fs::metadata(p).is_ok_and(|m| m.is_dir() && m.dev() != here))
        else {
            return;
        };
        let dst = other.join(format!(".cargo-steward-xdev-test-{}", std::process::id()));
        let result = clone_tree(&src, &dst);
        let leftover = dst.exists();
        let _ = fs::remove_dir_all(&dst);
        assert_eq!(result.unwrap(), Cloned::Unsupported);
        assert!(!leftover, "nothing may be copied across volumes");
        let file = other.join(format!(".cargo-steward-xdev-file-{}", std::process::id()));
        let result = clone_file(&src.join(".rustc_info.json"), &file);
        let leftover = file.exists();
        let _ = fs::remove_file(&file);
        assert_eq!(result.unwrap(), Cloned::Unsupported);
        assert!(!leftover);
    }

    #[test]
    fn clone_file_rejects_what_isnt_a_regular_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dir");
        fs::create_dir(&dir).unwrap();
        let dst = tmp.path().join("copy");
        let err = clone_file(&dir, &dst).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!dst.exists());
        assert!(clone_file(&tmp.path().join("missing"), &dst).is_err());
    }

    #[test]
    fn may_clone_and_clone_id_answer_no_for_what_they_cant_read() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing");
        assert!(!may_clone(&missing));
        assert!(!may_clone(Path::new("a\0b")));
        assert_eq!(clone_id(&missing), None);
        assert_eq!(clone_id(Path::new("a\0b")), None);
        // Devices have no clone ID.
        #[cfg(unix)]
        assert_eq!(clone_id(Path::new("/dev/null")), None);
    }

    /// A source that can't be read is an error, and leaves nothing.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_unreadable_source_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        if !may_clone(tmp.path()) {
            return;
        }
        let src = tmp.path().join("a");
        fs::write(&src, "x").unwrap();
        // Write-only.
        fs::set_permissions(&src, fs::Permissions::from_mode(0o200)).unwrap();
        let dst = tmp.path().join("b");
        let result = clone_file(&src, &dst);
        fs::set_permissions(&src, fs::Permissions::from_mode(0o600)).unwrap();
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert!(fs::symlink_metadata(&dst).is_err());
    }
}
