//! The build gate: at most `max-builds` cargo builds at once, machine-wide.
//!
//! Each build holds an exclusive `flock` on one of N slot files. The
//! descriptor is left open and inheritable across `exec`, so the slot is
//! held for exactly as long as the cargo process (and anything it leaves
//! running with the descriptor) lives, and released by the kernel however
//! it ends. Cargo run from a build script inherits the descriptor and the
//! environment variables naming it, and reuses the slot instead of waiting
//! for a second one (which could deadlock at `max-builds = 1`).

use std::ffi::OsString;
use std::fs::{self, OpenOptions, TryLockError};
use std::io;
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
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
const NON_BUILD: [&str; 22] = [
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

/// Takes a slot under `dir`, waiting (with one message) while all `limit`
/// are taken, or reuses the one this process inherited. Adds the variables
/// that name the slot to `env`.
pub fn acquire(dir: &Path, limit: u32, env: &mut Vec<(OsString, OsString)>) -> io::Result<()> {
    if reuse_inherited(
        dir,
        std::env::var_os(FD_VAR).as_deref(),
        std::env::var_os(SLOT_VAR).as_deref(),
    ) {
        return Ok(());
    }
    fs::create_dir_all(dir)?;
    let mut waiting = false;
    loop {
        if let Some((fd, path)) = try_any(dir, limit)? {
            env.push((FD_VAR.into(), fd.to_string().into()));
            env.push((SLOT_VAR.into(), path.into()));
            return Ok(());
        }
        if !waiting {
            eprintln!("cargo-steward: waiting for one of {limit} Cargo build slots");
            waiting = true;
        }
        sleep(POLL);
    }
}

/// One pass over the slots. The descriptor of the one taken is left open
/// and inheritable.
fn try_any(dir: &Path, limit: u32) -> io::Result<Option<(RawFd, PathBuf)>> {
    for index in 0..limit {
        let path = dir.join(format!("{index}.lock"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => {
                set_inheritable(file.as_raw_fd())?;
                return Ok(Some((file.into_raw_fd(), path)));
            }
            Err(TryLockError::WouldBlock) => continue,
            Err(TryLockError::Error(e)) => return Err(e),
        }
    }
    Ok(None)
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
    use std::fs::File;

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
        let (fd, path) = try_any(&dir, 2).unwrap().unwrap();
        assert_eq!(path, dir.join("1.lock"));
        // Taken slot fds are inheritable.
        // SAFETY: fd was just opened by try_any.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, 0);
        assert!(try_any(&dir, 2).unwrap().is_none());
        // SAFETY: closing the descriptor try_any leaked on purpose.
        unsafe { libc::close(fd) };
        let (fd, _) = try_any(&dir, 2).unwrap().unwrap();
        // SAFETY: as above.
        unsafe { libc::close(fd) };
    }

    #[test]
    fn acquire_waits_for_a_held_slot_then_takes_it() {
        let tmp = tempfile::tempdir().unwrap();
        // Created by `acquire` itself.
        let dir = tmp.path().join("new/slots");
        let mut env = Vec::new();
        acquire(&dir, 1, &mut env).unwrap();
        let fd: RawFd = env[0].1.to_str().unwrap().parse().unwrap();
        assert_eq!(env[0].0, FD_VAR);
        assert_eq!(env[1], (SLOT_VAR.into(), dir.join("0.lock").into()));

        // The only slot is held: `acquire` waits until it's released.
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = {
            let dir = dir.clone();
            std::thread::spawn(move || {
                let mut env = Vec::new();
                acquire(&dir, 1, &mut env).unwrap();
                tx.send(env).unwrap();
            })
        };
        assert!(
            rx.recv_timeout(Duration::from_millis(400)).is_err(),
            "took a held slot"
        );
        // SAFETY: releasing the descriptor the first acquire left open.
        unsafe { libc::close(fd) };
        let env = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        waiter.join().unwrap();
        assert_eq!(env[1].1, dir.join("0.lock").into_os_string());
        let fd: RawFd = env[0].1.to_str().unwrap().parse().unwrap();
        // SAFETY: as above.
        unsafe { libc::close(fd) };
    }

    #[test]
    fn acquire_fails_when_the_slots_cant_be_made() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("file");
        fs::write(&file, "").unwrap();
        let mut env = Vec::new();
        assert!(acquire(&file.join("slots"), 1, &mut env).is_err());
        // A slot that's a symlink is refused, never followed.
        let dir = tmp.path().join("slots");
        fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&file, dir.join("0.lock")).unwrap();
        assert!(try_any(&dir, 1).is_err());
        assert!(env.is_empty());
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
