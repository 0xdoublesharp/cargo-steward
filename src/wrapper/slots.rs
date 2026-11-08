//! The compile limit (`max-compiles`): at most N compiles at once,
//! machine-wide, however many cargo builds start them.
//!
//! Each compile holds an exclusive `flock` on one of N slot files in
//! `<steward home>/compile-slots/` while its rustc runs. The descriptor is
//! close-on-exec, as every file Rust opens is, so rustc and anything it
//! starts never hold a slot, and the kernel releases it however the wrapper
//! ends. Waiting blocks in the kernel on one slot; nothing polls.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// A held compile slot, released when dropped.
#[derive(Debug)]
pub struct Slot {
    _file: File,
}

/// Takes one of `limit` slots under `dir`: the first free one, or, when all
/// are taken, the one this process's ID picks, waiting until it frees.
///
/// `None` when there is no limit, or when a slot can't be had for any other
/// reason than waiting: the compile then runs without one. Also `None`,
/// without waiting, when a process this one descends from holds a slot (a
/// compile started from inside a compile, such as by a proc macro), which
/// at `max-compiles = 1` would otherwise wait forever.
pub fn acquire(dir: &Path, limit: u32) -> Option<Slot> {
    if limit == 0 {
        return None;
    }
    for index in 0..limit {
        let file = open(dir, index)?;
        match file.try_lock() {
            Ok(()) => return Some(claim(file)),
            Err(TryLockError::WouldBlock) => continue,
            Err(TryLockError::Error(_)) => return None,
        }
    }
    if (0..limit).any(|index| holder(dir, index).is_some_and(crate::proxy::is_ancestor)) {
        return None;
    }
    let index = std::process::id() % limit;
    let file = open(dir, index)?;
    file.lock().ok()?;
    Some(claim(file))
}

fn slot_path(dir: &Path, index: u32) -> PathBuf {
    dir.join(format!("{index}.lock"))
}

/// Opens slot `index`, creating it and its directory as needed.
fn open(dir: &Path, index: u32) -> Option<File> {
    let path = slot_path(dir, index);
    match open_slot(&path) {
        Ok(file) => Some(file),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(dir).ok()?;
            open_slot(&path).ok()
        }
        Err(_) => None,
    }
}

fn open_slot(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Records this process as the slot's holder, in a fixed width so that one
/// write replaces the previous holder's ID.
fn claim(file: File) -> Slot {
    let _ = file.write_at(format!("{:<10}\n", std::process::id()).as_bytes(), 0);
    Slot { _file: file }
}

/// The process ID the slot's file names: its holder, or its last one.
fn holder(dir: &Path, index: u32) -> Option<u32> {
    fs::read_to_string(slot_path(dir, index))
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn takes_free_slots_first_and_releases_them_when_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("compile-slots");
        let taken = |index| {
            open_slot(&slot_path(&dir, index))
                .unwrap()
                .try_lock()
                .is_err()
        };
        let first = acquire(&dir, 2).unwrap();
        assert!(taken(0) && !taken(1));
        let second = acquire(&dir, 2).unwrap();
        assert!(taken(0) && taken(1));
        assert_eq!(holder(&dir, 0), Some(std::process::id()));
        drop(first);
        assert!(!taken(0) && taken(1));
        let _again = acquire(&dir, 2).unwrap();
        assert!(taken(0));
        drop(second);
    }

    #[test]
    fn no_limit_takes_no_slot() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(acquire(tmp.path(), 0).is_none());
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn waits_while_every_slot_is_taken() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let held = acquire(&dir, 1).unwrap();
        // Not one of this process's ancestors, so the waiter waits.
        fs::write(slot_path(&dir, 0), "999999999\n").unwrap();
        let (sent, got) = mpsc::channel();
        let waiter = thread::spawn(move || {
            let slot = acquire(&dir, 1);
            sent.send(slot.is_some()).unwrap();
        });
        assert!(
            got.recv_timeout(Duration::from_millis(300)).is_err(),
            "took a slot that was held"
        );
        drop(held);
        assert_eq!(got.recv_timeout(Duration::from_secs(10)), Ok(true));
        waiter.join().unwrap();
    }

    #[test]
    fn a_compile_inside_a_compile_doesnt_wait_for_its_own_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let held = acquire(tmp.path(), 1).unwrap();
        // As if the wrapper that started this process held the slot.
        // SAFETY: plain syscall with no arguments.
        let parent = unsafe { libc::getppid() };
        fs::write(slot_path(tmp.path(), 0), format!("{parent}\n")).unwrap();
        assert!(acquire(tmp.path(), 1).is_none());
        drop(held);
    }

    #[test]
    fn an_unusable_slot_directory_means_no_slot() {
        let tmp = tempfile::tempdir().unwrap();
        // A file where the directory goes: it can't be created.
        let file = tmp.path().join("compile-slots");
        fs::write(&file, "").unwrap();
        assert!(acquire(&file, 2).is_none());
        // A slot that is a symlink isn't followed.
        let dir = tmp.path().join("dir");
        fs::create_dir(&dir).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("elsewhere"), slot_path(&dir, 0)).unwrap();
        assert!(acquire(&dir, 1).is_none());
        assert!(!tmp.path().join("elsewhere").exists());
    }

    #[test]
    fn rustc_never_inherits_a_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let slot = acquire(tmp.path(), 1).unwrap();
        // SAFETY: F_GETFD only reads the descriptor's flags; the descriptor
        // stays open for the call, as `slot` owns it.
        let flags = unsafe { libc::fcntl(slot._file.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0);
        assert_ne!(flags & libc::FD_CLOEXEC, 0, "the slot is inherited on exec");
    }
}
