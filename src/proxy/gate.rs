//! The build gate: at most `max-builds` cargo builds at once, machine-wide.
//!
//! Each build holds an exclusive `flock` on one of N slot files, on a
//! descriptor nothing it starts inherits; the kernel releases it however the
//! holder ends. The proxy that takes a slot stays as cargo's parent and holds
//! it while cargo builds ([`super::hold`]). A cargo run inside that build (by
//! a build script or a `cargo-*` subcommand) finds the holder through
//! [`HOLDER_VAR`] and reuses the slot instead of waiting for a second one,
//! which could deadlock at `max-builds = 1`. A slot file names its holder,
//! for the message a waiting build prints.

use std::ffi::OsString;
use std::fs::File;
use std::fs::{self, OpenOptions, TryLockError};
use std::io;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::RawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use super::route::own_args;

/// The inherited slot's descriptor number.
pub const FD_VAR: &str = "CARGO_STEWARD_SLOT_FD";
/// The inherited slot's file.
pub const SLOT_VAR: &str = "CARGO_STEWARD_SLOT";

const POLL: Duration = Duration::from_millis(100);

/// Commands that never compile, so they never wait for a slot. Anything
/// else does, including unknown commands: cargo expands aliases itself, and
/// an alias for a build must not bypass the gate.
const NON_BUILD: [&str; 23] = [
    "metadata",
    "fmt",
    "tree",
    "help",
    "version",
    "locate-project",
    "pkgid",
    "read-manifest",
    "fetch",
    "update",
    "generate-lockfile",
    "clean",
    "search",
    "info",
    "login",
    "logout",
    "owner",
    "yank",
    "uninstall",
    "new",
    "init",
    "steward",
    // A watcher, not a build: each build it starts goes through the proxy
    // and takes a slot of its own, rather than one held while it idles.
    "watch",
];

/// Whether a cargo invocation may compile, and so takes a build slot.
pub fn is_build(args: &[OsString]) -> bool {
    command(args).is_some_and(|c| !NON_BUILD.contains(&c.as_str()))
}

/// The cargo subcommand, skipping `+toolchain` and global options. `None`
/// for `--help`, `--version` and the like.
pub fn command(args: &[OsString]) -> Option<String> {
    let args = own_args(args);
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return None;
    }
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let text = arg.to_string_lossy();
        if matches!(
            text.as_ref(),
            "--config" | "--color" | "-C" | "-Z" | "--explain"
        ) {
            iter.next();
        } else if !text.starts_with(['+', '-']) {
            return Some(text.into_owned());
        }
    }
    None
}

/// The proxy holding the slot a build runs in. A nested cargo (run from a
/// build script, or by a cargo subcommand) reuses that slot only while this
/// process is one of its ancestors, so a program the build left running
/// (a test's daemon) can't pass the gate with it.
pub const HOLDER_VAR: &str = "CARGO_STEWARD_SLOT_PID";

/// A build slot.
#[derive(Debug)]
pub enum Slot {
    /// One an enclosing build holds, reused.
    Inherited,
    /// Taken by this process: its lock lives as long as `file` stays open.
    /// The descriptor is closed on exec, so nothing cargo starts holds it.
    Held { file: File, path: PathBuf },
}

/// Takes a slot under `dir`, waiting (with one message naming who holds
/// them) while all `limit` are taken, or reuses the one an enclosing build
/// holds. `about` describes this build for anyone waiting on it.
pub fn acquire(dir: &Path, limit: u32, about: &str) -> io::Result<Slot> {
    let slot = std::env::var_os(SLOT_VAR);
    if reuse_inherited(dir, std::env::var_os(FD_VAR).as_deref(), slot.as_deref())
        || held_by_ancestor(
            dir,
            slot.as_deref(),
            std::env::var_os(HOLDER_VAR).as_deref(),
        )
    {
        return Ok(Slot::Inherited);
    }
    fs::create_dir_all(dir)?;
    let mut waiting = false;
    loop {
        if let Some((mut file, path)) = try_any(dir, limit)? {
            let _ = describe(&mut file, about);
            return Ok(Slot::Held { file, path });
        }
        if !waiting {
            let holders = holders(dir, limit);
            if holders.is_empty() {
                eprintln!("cargo-steward: waiting for one of {limit} Cargo build slots");
            } else {
                eprintln!(
                    "cargo-steward: waiting for one of {limit} Cargo build slots (held by {})",
                    holders.join("; ")
                );
            }
            waiting = true;
        }
        sleep(POLL);
    }
}

/// One pass over the slots: the first free one, locked.
fn try_any(dir: &Path, limit: u32) -> io::Result<Option<(File, PathBuf)>> {
    for index in 0..limit {
        let path = dir.join(format!("{index}.lock"));
        let file = open_slot(&path)?;
        match file.try_lock() {
            Ok(()) => return Ok(Some((file, path))),
            Err(TryLockError::WouldBlock) => continue,
            Err(TryLockError::Error(e)) => return Err(e),
        }
    }
    Ok(None)
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

/// Writes who holds a slot into its file: `pid <pid>: <about>`.
fn describe(file: &mut File, about: &str) -> io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "pid {}: {about}", std::process::id())
}

/// What the held slots' files say about their holders.
fn holders(dir: &Path, limit: u32) -> Vec<String> {
    (0..limit)
        .filter_map(|index| {
            let path = dir.join(format!("{index}.lock"));
            let file = open_slot(&path).ok()?;
            if file.try_lock().is_ok() {
                return None; // Free since.
            }
            let text = fs::read_to_string(&path).ok()?;
            let line = text.lines().next()?.trim();
            (!line.is_empty()).then(|| line.to_owned())
        })
        .collect()
}

/// Whether an enclosing build's proxy holds the slot named in `slot`: the
/// variables name a slot file in `dir` and a process that is one of this
/// process's ancestors.
pub fn held_by_ancestor(
    dir: &Path,
    slot: Option<&std::ffi::OsStr>,
    holder: Option<&std::ffi::OsStr>,
) -> bool {
    let (Some(slot), Some(holder)) = (slot, holder) else {
        return false;
    };
    let Some(holder) = holder.to_str().and_then(|s| s.parse::<u32>().ok()) else {
        return false;
    };
    Path::new(slot).parent() == Some(dir)
        && fs::symlink_metadata(slot).is_ok_and(|m| m.is_file())
        && holder != std::process::id()
        && is_ancestor(holder)
}

/// Whether `pid` is this process's parent, or its parent's, and so on.
pub fn is_ancestor(pid: u32) -> bool {
    // SAFETY: plain syscall with no arguments.
    let mut current = unsafe { libc::getppid() };
    for _ in 0..256 {
        let Ok(id) = u32::try_from(current) else {
            return false;
        };
        if id == pid {
            return true;
        }
        if id <= 1 {
            return false;
        }
        match parent_of(current) {
            Some(parent) => current = parent,
            None => return false,
        }
    }
    false
}

#[cfg(target_os = "macos")]
fn parent_of(pid: libc::pid_t) -> Option<libc::pid_t> {
    // SAFETY: `proc_bsdinfo` is plain data, for which all zeroes is valid.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: `info` is writable for `size` bytes and outlives the call.
    let written =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    (written == size).then(|| libc::pid_t::try_from(info.pbi_ppid).ok())?
}

#[cfg(not(target_os = "macos"))]
fn parent_of(pid: libc::pid_t) -> Option<libc::pid_t> {
    // `<pid> (<command>) <state> <ppid> ...`; the command may hold spaces.
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Whether this process inherited a slot it can keep using: the variables
/// name a slot file in `dir`, the descriptor is open on that very file, and
/// it holds the lock. A leftover variable without the open descriptor
/// never bypasses the gate.
pub fn reuse_inherited(
    dir: &Path,
    fd: Option<&std::ffi::OsStr>,
    slot: Option<&std::ffi::OsStr>,
) -> bool {
    let (Some(fd), Some(slot)) = (fd, slot) else {
        return false;
    };
    let Some(fd) = fd.to_str().and_then(|s| s.parse::<RawFd>().ok()) else {
        return false;
    };
    let slot = Path::new(slot);
    if fd < 0 || slot.parent() != Some(dir) {
        return false;
    }
    let Ok(meta) = fs::symlink_metadata(slot) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    // SAFETY: `libc::stat` is plain data, for which all zeroes is valid.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstat writes only into `stat`; a descriptor that isn't open
    // makes it fail with EBADF.
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return false;
    }
    if stat.st_dev as u64 != meta.dev() || stat.st_ino as u64 != meta.ino() {
        return false;
    }
    // Our open file description already holds the lock if the parent held
    // it, so this succeeds at once; otherwise someone else holds it.
    // SAFETY: plain syscall on a descriptor checked to be open.
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return false;
    }
    set_inheritable(fd).is_ok()
}

/// Clears close-on-exec, so the descriptor survives into cargo.
fn set_inheritable(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor this process has open; reads and sets
    // only its descriptor flags.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn classifies_commands() {
        for build in [
            &["build"][..],
            &["+nightly", "check", "--all-targets"],
            &["-v", "test"],
            &["--config", "build.jobs=2", "clippy"],
            &["-Zunstable-options", "-C", "dir", "run"],
            &["--color", "never", "doc"],
            // Unknown commands and aliases might build.
            &["b"],
            &["nextest", "run"],
            &["install", "ripgrep"],
            &["run", "--", "--help"],
        ] {
            assert!(is_build(&os(build)), "{build:?}");
        }
        for other in [
            &[][..],
            &["--version"],
            &["-V"],
            &["--list"],
            &["build", "--help"],
            &["-h"],
            &["help", "build"],
            &["metadata", "--format-version", "1"],
            &["+stable", "fmt"],
            &["tree"],
            &["fetch"],
            &["clean"],
            &["locate-project"],
            &["steward", "status"],
            &["--color", "build", "metadata"],
            &["--explain", "E0308"],
        ] {
            assert!(!is_build(&os(other)), "{other:?}");
        }
        assert_eq!(
            command(&os(&["+1.88", "-q", "check"])).as_deref(),
            Some("check")
        );
    }

    fn open(path: &Path) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap()
    }

    #[test]
    fn reuses_only_a_held_inherited_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("slots");
        fs::create_dir_all(&dir).unwrap();
        let slot = dir.join("0.lock");
        let held = open(&slot);
        held.lock().unwrap();
        let fd = held.as_raw_fd().to_string();
        let fd = std::ffi::OsStr::new(&fd);
        assert!(reuse_inherited(&dir, Some(fd), Some(slot.as_os_str())));

        // Variables without both parts, or that don't match, never count.
        assert!(!reuse_inherited(&dir, None, Some(slot.as_os_str())));
        assert!(!reuse_inherited(&dir, Some(fd), None));
        assert!(!reuse_inherited(
            &dir,
            Some("x".as_ref()),
            Some(slot.as_os_str())
        ));
        assert!(!reuse_inherited(
            &dir,
            Some("-1".as_ref()),
            Some(slot.as_os_str())
        ));
        let other = dir.join("1.lock");
        open(&other);
        assert!(!reuse_inherited(&dir, Some(fd), Some(other.as_os_str())));
        let elsewhere = tmp.path().join("0.lock");
        fs::copy(&slot, &elsewhere).unwrap();
        assert!(!reuse_inherited(
            &dir,
            Some(fd),
            Some(elsewhere.as_os_str())
        ));
        // A descriptor that's closed (a stale variable) doesn't either.
        let stale = open(&slot);
        let stale_fd = stale.as_raw_fd().to_string();
        drop(stale);
        assert!(!reuse_inherited(
            &dir,
            Some(stale_fd.as_ref()),
            Some(slot.as_os_str())
        ));
        // Nor one open on the right file whose lock someone else holds.
        let unlocked = open(&slot);
        let unlocked_fd = unlocked.as_raw_fd().to_string();
        assert!(!reuse_inherited(
            &dir,
            Some(unlocked_fd.as_ref()),
            Some(slot.as_os_str())
        ));
        drop(held);
    }

    #[test]
    fn takes_free_slots_and_skips_held_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("slots");
        fs::create_dir_all(&dir).unwrap();
        let first = open(&dir.join("0.lock"));
        first.lock().unwrap();
        let (taken, path) = try_any(&dir, 2).unwrap().unwrap();
        assert_eq!(path, dir.join("1.lock"));
        // Nothing cargo starts inherits a taken slot.
        // SAFETY: fcntl only reads the open descriptor's flags.
        let flags = unsafe { libc::fcntl(taken.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        assert!(try_any(&dir, 2).unwrap().is_none());
        drop(taken);
        // A process another test thread forks at that moment can hold a copy
        // of the descriptor until it execs: allow a moment.
        let freed = (0..50).any(|_| {
            let free = try_any(&dir, 2).unwrap().is_some();
            if !free {
                std::thread::sleep(Duration::from_millis(20));
            }
            free
        });
        assert!(freed, "the slot was never released");
    }

    #[test]
    fn names_who_holds_each_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("slots");
        fs::create_dir_all(&dir).unwrap();
        let (mut held, _) = try_any(&dir, 2).unwrap().unwrap();
        describe(&mut held, "`cargo test` in /w/a").unwrap();
        // A slot that's free again says nothing, whatever its file holds.
        let (mut freed, _) = try_any(&dir, 2).unwrap().unwrap();
        describe(&mut freed, "`cargo build` in /w/b").unwrap();
        drop(freed);
        // A process another test thread forks at that moment can hold a copy
        // of the freed slot's descriptor until it execs: allow a moment.
        let expected = [format!("pid {}: `cargo test` in /w/a", std::process::id())];
        let settled = (0..50).any(|_| {
            let found = holders(&dir, 2) == expected;
            if !found {
                std::thread::sleep(Duration::from_millis(20));
            }
            found
        });
        assert!(settled, "{:?}", holders(&dir, 2));
        // A shorter description replaces a longer one whole.
        describe(&mut held, "x").unwrap();
        assert_eq!(holders(&dir, 2), [format!("pid {}: x", std::process::id())]);
    }

    #[test]
    fn reuses_a_slot_only_an_ancestor_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("slots");
        fs::create_dir_all(&dir).unwrap();
        let slot = dir.join("0.lock");
        open(&slot);
        let parent = std::os::unix::process::parent_id().to_string();
        let held =
            |holder: &str| held_by_ancestor(&dir, Some(slot.as_os_str()), Some(holder.as_ref()));
        assert!(held(&parent));
        assert!(is_ancestor(1), "init holds everything");
        // A process that isn't an ancestor (a daemon's long-gone holder
        // whose ID something else now has), or this one, doesn't count.
        let mut other = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        assert!(!held(&other.id().to_string()));
        let _ = other.kill();
        let _ = other.wait();
        assert!(!held(&std::process::id().to_string()));
        assert!(!held("x"));
        // Nor a slot outside the slot directory.
        let elsewhere = tmp.path().join("0.lock");
        open(&elsewhere);
        assert!(!held_by_ancestor(
            &dir,
            Some(elsewhere.as_os_str()),
            Some(parent.as_ref())
        ));
    }

    #[test]
    fn acquire_waits_for_a_held_slot_then_takes_it() {
        let tmp = tempfile::tempdir().unwrap();
        // Created by `acquire` itself.
        let dir = tmp.path().join("new/slots");
        let Slot::Held { file, path } = acquire(&dir, 1, "first").unwrap() else {
            panic!("took no slot");
        };
        assert_eq!(path, dir.join("0.lock"));

        // The only slot is held: `acquire` waits until it's released.
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = {
            let dir = dir.clone();
            std::thread::spawn(move || {
                tx.send(acquire(&dir, 1, "second").unwrap()).unwrap();
            })
        };
        assert!(
            rx.recv_timeout(Duration::from_millis(400)).is_err(),
            "took a held slot"
        );
        drop(file);
        let second = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        waiter.join().unwrap();
        assert!(matches!(second, Slot::Held { path, .. } if path == dir.join("0.lock")));
        assert!(
            fs::read_to_string(dir.join("0.lock"))
                .unwrap()
                .ends_with(": second\n")
        );
    }

    #[test]
    fn acquire_fails_when_the_slots_cant_be_made() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("file");
        fs::write(&file, "").unwrap();
        assert!(acquire(&file.join("slots"), 1, "x").is_err());
        // A slot that's a symlink is refused, never followed.
        let dir = tmp.path().join("slots");
        fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&file, dir.join("0.lock")).unwrap();
        assert!(try_any(&dir, 1).is_err());
    }

    /// macOS can't flock a FIFO (EOPNOTSUPP): an error, not a busy slot.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_slot_that_cant_be_locked_is_an_error() {
        use std::os::unix::ffi::OsStrExt;
        let tmp = tempfile::tempdir().unwrap();
        let fifo =
            std::ffi::CString::new(tmp.path().join("0.lock").as_os_str().as_bytes()).unwrap();
        // SAFETY: mkfifo reads only the NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let taken = try_any(tmp.path(), 1);
        assert!(taken.is_err(), "{taken:?}");
    }

    #[test]
    fn an_inherited_slot_must_be_a_regular_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let held = open(&dir.join("held"));
        let fd = held.as_raw_fd().to_string();
        let missing = dir.join("missing.lock");
        assert!(!reuse_inherited(
            &dir,
            Some(fd.as_ref()),
            Some(missing.as_os_str())
        ));
        let sub = dir.join("sub.lock");
        fs::create_dir(&sub).unwrap();
        assert!(!reuse_inherited(
            &dir,
            Some(fd.as_ref()),
            Some(sub.as_os_str())
        ));
    }
}
