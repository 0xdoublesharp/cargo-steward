//! The compile limit (`max-compiles`): at most N compiles at once,
//! machine-wide, however many cargo builds start them.
//!
//! Each compile holds an exclusive `flock` on one of N slot files in
//! `<steward home>/compile-slots/` while its rustc runs. The descriptor is
//! close-on-exec, as every file Rust opens is, so rustc and anything it
//! starts never hold a slot, and the kernel releases it however the wrapper
//! ends. Memory-mode waiters block on a notification pipe; one head checks
//! for crashed owners at most once a second. Count mode blocks on a slot.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A held compile slot, released when dropped.
#[derive(Debug)]
pub struct Slot {
    _file: File,
    path: PathBuf,
    token: u128,
}

// sccache hashes all CARGO_* variables: private, changing lease markers
// must stay outside that namespace or every invocation becomes a miss.
const OWNER_PATH: &str = "_STEWARD_COMPILE_SLOT";
const OWNER_TOKEN: &str = "_STEWARD_COMPILE_TOKEN";

impl Slot {
    pub fn describe_child(&self, command: &mut std::process::Command) {
        command
            .env(OWNER_PATH, &self.path)
            .env(OWNER_TOKEN, self.token.to_string());
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        // Explicitly unlock: a sibling thread may have forked a transient
        // copy of the descriptor just before its close-on-exec takes effect.
        let _ = self._file.write_at(&[b' '; 96], 0);
        let _ = self._file.unlock();
        if let Some(dir) = self.path.parent()
            && let Ok(mut wake) = OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(dir.join("wake.fifo"))
            && wake.metadata().is_ok_and(|m| m.file_type().is_fifo())
        {
            let _ = wake.write(&[1]);
        }
    }
}

/// A nested compiler, including one launched by a cache server, may share
/// its producer's live lease. A stale environment never shares a free lease.
fn inherited(dir: &Path) -> bool {
    let Some(path) = std::env::var_os(OWNER_PATH).map(PathBuf::from) else {
        return false;
    };
    if path.parent() != Some(dir) {
        return false;
    }
    let Some(token) = std::env::var(OWNER_TOKEN)
        .ok()
        .and_then(|s| s.parse::<u128>().ok())
    else {
        return false;
    };
    inherited_lease(dir, &path, token)
}

fn inherited_lease(dir: &Path, path: &Path, token: u128) -> bool {
    if token == 0 || path.parent() != Some(dir) {
        return false;
    }
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(TryLockError::WouldBlock) => {
            let (pid, _, owner) = record(&file);
            owner == Some(token) && pid.is_some_and(crate::fsutil::process_alive)
        }
        Err(_) => false,
    }
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
    if limit == 0 || inherited(dir) {
        return None;
    }
    for index in 0..limit {
        let file = open(dir, index)?;
        match file.try_lock() {
            Ok(()) => return Some(claim_memory(file, 0, slot_path(dir, index))),
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
    Some(claim_memory(file, 0, slot_path(dir, index)))
}

/// Admit count slots and estimated memory together. A single oversized
/// compile runs alone, so an estimate can never deadlock the build.
/// The head waiter holds a turnstile while active compiles drain, so new
/// small requests cannot continually overtake an admitted large waiter.
/// Slot completions notify the head; a one-second timeout recovers from
/// crashed holders that could not notify. Other waiters block on the turnstile.
pub fn acquire_memory(
    dir: &Path,
    limit: u32,
    budget: u64,
    estimate: u64,
    fallback: u64,
) -> Option<Slot> {
    if limit == 0 || budget == 0 {
        return acquire(dir, limit);
    }
    if inherited(dir) {
        return None;
    }
    fs::create_dir_all(dir).ok()?;
    let ancestors = crate::proxy::ancestor_pids();
    let mut indexes: std::collections::BTreeSet<u32> = (0..limit).collect();
    indexes.extend(fs::read_dir(dir).ok()?.flatten().filter_map(|entry| {
        entry
            .file_name()
            .to_str()?
            .strip_suffix(".lock")?
            .parse::<u32>()
            .ok()
    }));
    let mut files: Vec<_> = indexes
        .iter()
        .map(|&index| Some((index, open(dir, index)?)))
        .collect::<Option<_>>()?;
    // Reentrancy is checked before waiting for the turnstile: a current
    // head may itself be waiting for the enclosing compiler to finish.
    for (_, file) in &files {
        match file.try_lock() {
            Ok(()) => {
                file.unlock().ok()?;
            }
            Err(TryLockError::WouldBlock)
                if record(file).0.is_some_and(|p| ancestors.contains(&p)) =>
            {
                return None;
            }
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(_)) => return None,
        }
    }
    let admission = open_slot(&dir.join("admission.lock")).ok()?;
    let requested = estimate.max(1).min(budget);
    admission.lock().ok()?;
    let mut notifications = Notifications::open(dir);
    // A client with a larger count limit may have created higher slots
    // while we waited for the turnstile. Include those reservations too.
    for entry in fs::read_dir(dir).ok()?.flatten() {
        if let Some(index) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.strip_suffix(".lock"))
            .and_then(|s| s.parse::<u32>().ok())
            && indexes.insert(index)
        {
            files.push((index, open(dir, index)?));
        }
    }
    loop {
        let mut free = None;
        let mut reserved = 0u64;
        let mut wait_count = None;
        let mut wait_memory = None;
        for (index, file) in &files {
            match file.try_lock() {
                Ok(()) => {
                    if free.is_none() && *index < limit {
                        free = Some((*index, file));
                    } else {
                        file.unlock().ok()?;
                    }
                }
                Err(TryLockError::WouldBlock) => {
                    let memory = record(file).1.filter(|&n| n > 0).unwrap_or(fallback.max(1));
                    reserved = reserved.saturating_add(memory);
                    if *index < limit && wait_count.is_none() {
                        wait_count = Some(file);
                    }
                    if wait_memory.as_ref().is_none_or(|(_, n)| memory > *n) {
                        wait_memory = Some((file, memory));
                    }
                }
                Err(TryLockError::Error(_)) => return None,
            }
        }
        if reserved.saturating_add(requested) <= budget
            && let Some((index, file)) = free.take()
        {
            let slot = claim_memory(file.try_clone().ok()?, requested, slot_path(dir, index));
            admission.unlock().ok()?;
            return Some(slot);
        }
        let count_full = free.is_none();
        if let Some((_, file)) = free {
            file.unlock().ok()?;
        }
        if let Some(wake) = &mut notifications {
            if wake.wait().is_ok() {
                continue;
            }
            notifications = None;
        }
        let wake = if count_full {
            wait_count
        } else {
            wait_memory.map(|(file, _)| file)
        }?;
        wake.lock().ok()?;
        wake.unlock().ok()?;
    }
}

fn retry_interrupted<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match operation() {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

struct Notifications(File);

impl Notifications {
    fn open(dir: &Path) -> Option<Self> {
        let path = dir.join("wake.fifo");
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: name is a live NUL-terminated path; mode has no other pointers.
        if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } != 0
            && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
        {
            return None;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .ok()?;
        file.metadata()
            .ok()?
            .file_type()
            .is_fifo()
            .then_some(Self(file))
    }

    fn wait(&mut self) -> io::Result<()> {
        let mut event = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        retry_interrupted(|| {
            // SAFETY: event points to one live pollfd; poll writes only that entry.
            let ready = unsafe { libc::poll(&mut event, 1, 1000) };
            if ready < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(ready)
            }
        })?;
        let mut bytes = [0; 256];
        loop {
            match retry_interrupted(|| self.0.read(&mut bytes)) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// A malformed record remains a live reservation of unknown size.
fn record(file: &File) -> (Option<u32>, Option<u64>, Option<u128>) {
    let mut bytes = [0u8; 96];
    let Some(text) = file
        .read_at(&mut bytes, 0)
        .ok()
        .and_then(|n| std::str::from_utf8(&bytes[..n]).ok())
    else {
        return (None, None, None);
    };
    let mut fields = text.split_whitespace();
    (
        fields.next().and_then(|s| s.parse().ok()),
        fields.next().and_then(|s| s.parse().ok()),
        fields.next().and_then(|s| s.parse().ok()),
    )
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
fn claim_memory(file: File, memory: u64, path: PathBuf) -> Slot {
    let token = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let _ = file.write_at(
        format!("{:<10}\n{memory:<20}\n{token:<39}\n", std::process::id()).as_bytes(),
        0,
    );
    Slot {
        _file: file,
        path,
        token,
    }
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
    fn interrupted_notifications_are_retried() {
        let mut calls = 0;
        let result = retry_interrupted(|| {
            calls += 1;
            if calls == 1 {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                Ok(7)
            }
        });
        assert_eq!(result.unwrap(), 7);
        let denied = retry_interrupted(|| Err::<(), _>(io::ErrorKind::PermissionDenied.into()));
        assert_eq!(denied.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn unknown_holders_use_the_configured_fallback_not_a_small_waiters_estimate() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let file = open(&dir, 0).unwrap();
        file.lock().unwrap();
        let held = claim_memory(file, 0, slot_path(&dir, 0));
        let (sent, got) = mpsc::channel();
        let worker = thread::spawn(move || {
            let slot = acquire_memory(&dir, 2, 100, 1, 100);
            sent.send(slot.is_some()).unwrap();
        });
        let early = got.recv_timeout(Duration::from_millis(100));
        drop(held);
        let admitted = early
            .as_ref()
            .copied()
            .unwrap_or_else(|_| got.recv_timeout(Duration::from_secs(5)).unwrap());
        worker.join().unwrap();
        assert!(
            early.is_err(),
            "an unknown live holder was charged only one byte"
        );
        assert!(admitted);
    }

    #[test]
    fn any_completed_compile_can_wake_the_head_waiter() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let held: Vec<_> = (0..2)
            .map(|i| {
                let file = open(&dir, i).unwrap();
                file.lock().unwrap();
                claim_memory(file, 100, slot_path(&dir, i))
            })
            .collect();
        let (sent, got) = mpsc::channel();
        let worker = thread::spawn(move || {
            let slot = acquire_memory(&dir, 2, 200, 100, 100);
            sent.send(slot.is_some()).unwrap();
        });
        assert!(got.recv_timeout(Duration::from_millis(100)).is_err());
        let mut held = held;
        drop(held.pop());
        let woke = got.recv_timeout(Duration::from_millis(500));
        drop(held);
        if woke.is_err() {
            got.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        worker.join().unwrap();
        assert_eq!(
            woke,
            Ok(true),
            "a completed slot left the waiter blocked on another compiler"
        );
    }

    #[test]
    fn inherited_leases_require_a_live_matching_owner() {
        let temp = tempfile::tempdir().unwrap();
        let slot = acquire(temp.path(), 1).unwrap();
        assert!(inherited_lease(temp.path(), &slot.path, slot.token));
        assert!(!inherited_lease(temp.path(), &slot.path, slot.token + 1));
        let path = slot.path.clone();
        let token = slot.token;
        drop(slot);
        assert!(!inherited_lease(temp.path(), &path, token));
        let replacement = acquire(temp.path(), 1).unwrap();
        assert!(!inherited_lease(temp.path(), &path, token));
        drop(replacement);
    }

    #[test]
    fn memory_reservations_include_slots_above_this_clients_count_limit() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let file = open(&dir, 3).unwrap();
        file.lock().unwrap();
        let held = claim_memory(file, 100, slot_path(&dir, 3));
        let (sent, got) = mpsc::channel();
        let worker = thread::spawn(move || {
            let slot = acquire_memory(&dir, 1, 100, 100, 100);
            sent.send(slot.is_some()).unwrap();
        });
        let early = got.recv_timeout(Duration::from_millis(100));
        drop(held);
        let admitted = early
            .as_ref()
            .copied()
            .unwrap_or_else(|_| got.recv_timeout(Duration::from_secs(5)).unwrap());
        worker.join().unwrap();
        assert!(early.is_err(), "a high-numbered reservation was missed");
        assert!(admitted);
    }

    #[test]
    fn an_oversized_head_waiter_is_not_overtaken_by_small_compiles() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let file = open(&dir, 0).unwrap();
        file.lock().unwrap();
        let held = claim_memory(file, 10, slot_path(&dir, 0));
        let (sent, got) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let heavy_dir = dir.clone();
        let heavy_sent = sent.clone();
        let heavy = thread::spawn(move || {
            let _slot = acquire_memory(&heavy_dir, 4, 100, 300, 100).unwrap();
            heavy_sent.send("heavy").unwrap();
            released.recv().unwrap();
        });
        let admission = open_slot(&dir.join("admission.lock")).unwrap();
        let head_waiting = (0..500).any(|_| {
            if admission.try_lock().is_ok() {
                admission.unlock().unwrap();
                thread::sleep(Duration::from_millis(2));
                false
            } else {
                true
            }
        });
        assert!(head_waiting);
        let small = thread::spawn(move || {
            let _slot = acquire_memory(&dir, 4, 100, 10, 100).unwrap();
            sent.send("small").unwrap();
        });
        assert!(got.recv_timeout(Duration::from_millis(100)).is_err());
        drop(held);
        assert_eq!(got.recv_timeout(Duration::from_secs(5)).unwrap(), "heavy");
        release.send(()).unwrap();
        assert_eq!(got.recv_timeout(Duration::from_secs(5)).unwrap(), "small");
        heavy.join().unwrap();
        small.join().unwrap();
    }

    #[test]
    fn corrupt_slot_records_do_not_turn_off_memory_admission() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let held = open(&dir, 0).unwrap();
        held.lock().unwrap();
        held.write_at(&[255, 254, 253], 0).unwrap();
        let (sent, got) = mpsc::channel();
        let worker = thread::spawn(move || {
            let slot = acquire_memory(&dir, 1, 100, 100, 100);
            sent.send(slot.is_some()).unwrap();
        });
        let early = got.recv_timeout(Duration::from_millis(100));
        held.unlock().unwrap();
        drop(held);
        let admitted = early
            .as_ref()
            .copied()
            .unwrap_or_else(|_| got.recv_timeout(Duration::from_secs(5)).unwrap());
        worker.join().unwrap();
        assert!(
            early.is_err(),
            "an unreadable record bypassed the live reservation"
        );
        assert!(admitted);
    }

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
        // A process another test thread forks at that moment can hold a copy
        // of the descriptor until it execs: allow a moment.
        let released = (0..50).any(|_| {
            let free = !taken(0);
            if !free {
                thread::sleep(Duration::from_millis(20));
            }
            free
        });
        assert!(released && taken(1));
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
