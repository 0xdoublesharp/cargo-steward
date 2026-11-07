//! How much of a file is its own: the blocks no copy-on-write clone shares.
//!
//! `du` (and [`super::allocated`]) counts every file's blocks in full, so a
//! build directory seeded as a clone of another looks as big as the
//! original while taking almost nothing extra. Its private size is what
//! removing it alone gives back.

use std::path::Path;

/// Bytes of the file at `path` that no clone shares, without following a
/// symlink. `None` when the filesystem can't tell: on macOS anything but
/// APFS, on Linux filesystems without `FIEMAP`, and elsewhere always.
pub fn private_size(path: &Path) -> Option<u64> {
    imp::private_size(path)
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// `ATTR_CMN_RETURNED_ATTRS`: report which attributes were returned.
    const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
    /// `ATTR_CMNEXT_PRIVATESIZE`, in the fork group under
    /// `FSOPT_ATTR_CMN_EXTENDED`.
    const ATTR_CMNEXT_PRIVATESIZE: u32 = 0x8;
    const FSOPT_NOFOLLOW: u32 = 0x1;
    const FSOPT_ATTR_CMN_EXTENDED: u32 = 0x20;

    /// `getattrlist`'s answer: its length, the attributes returned (common,
    /// volume, directory, file, fork), then the private size as an `off_t`.
    /// Attributes are packed at 4-byte alignment.
    #[repr(C, packed(4))]
    struct Reply {
        length: u32,
        returned: [u32; 5],
        private_size: i64,
    }

    pub fn private_size(path: &Path) -> Option<u64> {
        let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut request = libc::attrlist {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: ATTR_CMN_RETURNED_ATTRS,
            volattr: 0,
            dirattr: 0,
            fileattr: 0,
            forkattr: ATTR_CMNEXT_PRIVATESIZE,
        };
        let mut reply = Reply {
            length: 0,
            returned: [0; 5],
            private_size: 0,
        };
        // SAFETY: `c_path` is NUL-terminated, `request` is a valid attrlist,
        // and `reply` is writable for the size passed; all outlive the call.
        let status = unsafe {
            libc::getattrlist(
                c_path.as_ptr(),
                (&raw mut request).cast(),
                (&raw mut reply).cast(),
                size_of::<Reply>(),
                FSOPT_NOFOLLOW | FSOPT_ATTR_CMN_EXTENDED,
            )
        };
        if status != 0 {
            return None;
        }
        let length = reply.length;
        let returned = reply.returned;
        if (length as usize) < size_of::<Reply>() || returned[4] & ATTR_CMNEXT_PRIVATESIZE == 0 {
            return None;
        }
        u64::try_from(reply.private_size).ok()
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    /// `struct fiemap`, the request header.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Fiemap {
        start: u64,
        length: u64,
        flags: u32,
        mapped_extents: u32,
        extent_count: u32,
        reserved: u32,
    }

    /// `struct fiemap_extent`.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Extent {
        logical: u64,
        physical: u64,
        length: u64,
        reserved64: [u64; 2],
        flags: u32,
        reserved: [u32; 3],
    }

    /// Extents asked for per call.
    const BATCH: usize = 64;

    /// A header followed by room for [`BATCH`] extents, as the kernel
    /// expects them.
    #[repr(C)]
    struct Request {
        head: Fiemap,
        extents: [Extent; BATCH],
    }

    const FS_IOC_FIEMAP: libc::Ioctl = libc::_IOWR::<Fiemap>(b'f' as u32, 11);
    const FIEMAP_EXTENT_LAST: u32 = 0x1;
    const FIEMAP_EXTENT_SHARED: u32 = 0x2000;

    /// Sums the extents the filesystem doesn't mark shared. Extents not yet
    /// written out (delayed allocation) count as private.
    pub fn private_size(path: &Path) -> Option<u64> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .ok()?;
        let mut private = 0u64;
        let mut start = 0u64;
        loop {
            let mut request = Request {
                head: Fiemap {
                    start,
                    length: u64::MAX,
                    extent_count: BATCH as u32,
                    ..Fiemap::default()
                },
                extents: [Extent::default(); BATCH],
            };
            // SAFETY: `request` is a fiemap header followed by room for
            // `extent_count` extents, which is all the kernel writes; it
            // outlives the call, and the descriptor is open.
            let status = unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_FIEMAP, &raw mut request) };
            if status != 0 {
                return None;
            }
            let mapped = (request.head.mapped_extents as usize).min(BATCH);
            if mapped == 0 {
                return Some(private);
            }
            for extent in &request.extents[..mapped] {
                if extent.flags & FIEMAP_EXTENT_SHARED == 0 {
                    private = private.saturating_add(extent.length);
                }
                if extent.flags & FIEMAP_EXTENT_LAST != 0 {
                    return Some(private);
                }
                start = extent.logical.saturating_add(extent.length);
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use std::path::Path;

    pub fn private_size(_path: &Path) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn a_plain_file_is_all_its_own() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("plain");
        fs::write(&file, vec![9u8; 1 << 20]).unwrap();
        let Some(private) = private_size(&file) else {
            return; // Not a filesystem that can tell.
        };
        assert_eq!(
            private,
            crate::fsutil::allocated(&fs::metadata(&file).unwrap())
        );
        assert_eq!(private_size(&tmp.path().join("missing")), None);
    }

    /// A clone shares every block with its original, so neither has any of
    /// its own, while `du` counts both in full.
    #[cfg(target_os = "macos")]
    #[test]
    fn clones_share_their_blocks() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("original");
        fs::write(&original, vec![3u8; 4 << 20]).unwrap();
        if private_size(&original).is_none() {
            return; // Not APFS.
        }
        let copy = tmp.path().join("copy");
        let src = CString::new(original.as_os_str().as_bytes()).unwrap();
        let dst = CString::new(copy.as_os_str().as_bytes()).unwrap();
        // SAFETY: both paths are NUL-terminated and outlive the call.
        assert_eq!(unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), 0) }, 0);
        assert_eq!(private_size(&original), Some(0));
        assert_eq!(private_size(&copy), Some(0));
        let usage = crate::fsutil::tree_usage(tmp.path());
        assert_eq!(usage.private, Some(0));
        assert!(usage.allocated >= 8 << 20, "{usage:?}");
    }
}
