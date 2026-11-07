//! Running cargo while holding a build slot ([`super::gate`]).
//!
//! The proxy stays as cargo's parent rather than `exec`ing it, and keeps the
//! slot's lock on a descriptor nothing it starts inherits. So the slot is
//! held exactly while cargo works: programs a build leaves running (a test's
//! daemon) don't keep it, and once cargo replaces itself with the program it
//! was asked to run (`cargo run`), the slot is let go. Nested cargo
//! invocations find the slot through [`super::gate::HOLDER_VAR`].

use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{self, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::gate::{FD_VAR, HOLDER_VAR, SLOT_VAR};

/// How often the running command is checked for having become the program
/// it runs.
const WATCH: Duration = Duration::from_millis(100);

/// The cargo process, for the signal handler.
static CHILD: AtomicI32 = AtomicI32::new(0);

/// Runs `command` (the real cargo) holding `slot`, and exits as it does.
/// `cargo_like`: this executable and the real cargo, resolved.
pub fn run(
    mut command: Command,
    slot: File,
    slot_path: &Path,
    cargo_like: &[Option<PathBuf>],
) -> ! {
    command
        .env(SLOT_VAR, slot_path)
        .env(HOLDER_VAR, process::id().to_string())
        .env_remove(FD_VAR);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("error: cargo-steward: failed to run cargo: {e}");
            process::exit(127);
        }
    };
    let pid = child.id();
    CHILD.store(i32::try_from(pid).unwrap_or(0), Ordering::SeqCst);
    forward_signals();

    let slot = Arc::new(Mutex::new(Some(slot)));
    let done = Arc::new(AtomicBool::new(false));
    {
        let (slot, done) = (Arc::clone(&slot), Arc::clone(&done));
        let cargo_like: Vec<PathBuf> = cargo_like.iter().flatten().cloned().collect();
        thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                thread::sleep(WATCH);
                if exe_of(pid).is_some_and(|exe| !is_cargo_like(&exe, &cargo_like)) {
                    release(&slot);
                    return;
                }
            }
        });
    }
    let status = child.wait();
    done.store(true, Ordering::SeqCst);
    release(&slot);
    match status {
        Ok(status) => exit_like(status),
        Err(e) => {
            eprintln!("error: cargo-steward: waiting for cargo: {e}");
            process::exit(1);
        }
    }
}

fn release(slot: &Mutex<Option<File>>) {
    if let Ok(mut held) = slot.lock() {
        held.take();
    }
}

/// Whether `exe` is still cargo (or rustup choosing one, or a `cargo-*`
/// subcommand, which builds through it) rather than a program it runs.
pub fn is_cargo_like(exe: &Path, cargo_like: &[PathBuf]) -> bool {
    let name = exe.file_name().and_then(OsStr::to_str).unwrap_or_default();
    name == "cargo"
        || name == "rustup"
        || name.starts_with("cargo-")
        || cargo_like.iter().any(|c| c == exe)
}

/// The executable a process runs now.
pub fn exe_of(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let pid = libc::c_int::try_from(pid).ok()?;
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let size = u32::try_from(buf.len()).ok()?;
        // SAFETY: `buf` is writable for `size` bytes and outlives the call.
        let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), size) };
        let len = usize::try_from(len)
            .ok()
            .filter(|&n| n > 0 && n <= buf.len())?;
        buf.truncate(len);
        Some(PathBuf::from(OsString::from_vec(buf)))
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok()
    }
}

/// Passes termination signals sent to the proxy alone on to cargo. Ctrl-C at
/// a terminal reaches cargo directly (they share its process group), so
/// there the proxy only outlives it rather than sending a second one, which
/// a program run by `cargo run` could take as "stop now".
fn forward_signals() {
    extern "C" fn forward(signal: libc::c_int) {
        let pid = CHILD.load(Ordering::SeqCst);
        if pid > 0 {
            // SAFETY: kill is async-signal-safe.
            unsafe {
                libc::kill(pid, signal);
            }
        }
    }
    extern "C" fn outlive(_: libc::c_int) {}
    // SAFETY: plain syscalls on this process's own descriptors and group.
    let interactive = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) == libc::getpgrp() };
    let forward_handler = forward as extern "C" fn(libc::c_int) as libc::sighandler_t;
    let outlive_handler = outlive as extern "C" fn(libc::c_int) as libc::sighandler_t;
    for signal in [libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: the handler only calls kill, which is async-signal-safe.
        unsafe {
            libc::signal(signal, forward_handler);
        }
    }
    let keyboard = if interactive {
        outlive_handler
    } else {
        forward_handler
    };
    for signal in [libc::SIGINT, libc::SIGQUIT] {
        // SAFETY: as above; `outlive` does nothing.
        unsafe {
            libc::signal(signal, keyboard);
        }
    }
}

/// Exits with cargo's exit code, or dies of the signal that killed it.
fn exit_like(status: ExitStatus) -> ! {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        process::exit(code);
    }
    if let Some(signal) = status.signal() {
        // SAFETY: restoring the default action and raising it on this
        // process; nothing else runs afterwards.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
        process::exit(128 + signal);
    }
    process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_cargo_from_the_programs_it_runs() {
        let real = PathBuf::from("/opt/toolchains/stable/bin/cargo-real");
        let like = [real.clone()];
        for cargo in [
            "/h/.cargo/bin/cargo",
            "/h/.rustup/toolchains/stable/bin/cargo",
            "/h/.cargo/bin/rustup",
            "/h/.cargo/bin/cargo-nextest",
        ] {
            assert!(is_cargo_like(Path::new(cargo), &like), "{cargo}");
        }
        assert!(is_cargo_like(&real, &like));
        for program in [
            "/w/target/debug/server",
            "/w/target/debug/deps/app-0123456789abcdef",
        ] {
            assert!(!is_cargo_like(Path::new(program), &like), "{program}");
        }
    }

    #[test]
    fn finds_what_a_process_runs() {
        let me = std::env::current_exe().unwrap().canonicalize().unwrap();
        assert_eq!(exe_of(process::id()), Some(me));
        let mut sleeper = Command::new("sleep").arg("5").spawn().unwrap();
        let exe = exe_of(sleeper.id());
        let _ = sleeper.kill();
        let _ = sleeper.wait();
        assert_eq!(
            exe.as_deref().and_then(Path::file_name),
            Some(OsStr::new("sleep"))
        );
    }
}
