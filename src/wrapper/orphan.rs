//! Ends a compile whose build is gone.
//!
//! When Cargo dies alone (a supervisor's timeout kills it, not its process
//! group), its compiles keep running. Nothing will use what they produce,
//! and a whole-program link can hold tens of gigabytes for an hour. A thread
//! here notices that the process that started the wrapper has exited, and
//! ends the compile: the wrapper's child and everything that child started.
//!
//! Nothing is ended while the process that asked for the compile exists.

use std::io;
use std::os::unix::process::parent_id;
use std::process::{self, Child, Command};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

/// How often the parent is looked at: one `getppid` call.
const POLL: Duration = Duration::from_secs(1);

#[derive(Default)]
struct State {
    /// The running compile's process.
    child: Option<u32>,
    /// The compile has ended; its process ID may belong to another process.
    finished: bool,
}

/// Watches for the wrapper's parent exiting, from before the wait for a
/// compile slot until the compile ends.
pub struct Guard(Arc<Mutex<State>>);

impl Guard {
    pub fn watch() -> Guard {
        let state = Arc::new(Mutex::new(State::default()));
        let watched = Arc::clone(&state);
        let parent = parent_id();
        // Best effort: without the thread the compile runs unwatched.
        let _ = thread::Builder::new()
            .name("orphan-guard".into())
            .spawn(move || watch(&watched, parent));
        Guard(state)
    }

    /// Starts the compile. The lock is held across the spawn so the watcher
    /// either sees no compile and leaves before one starts, or sees this one.
    pub fn spawn(&self, command: &mut Command) -> io::Result<Child> {
        let mut state = lock(&self.0);
        let child = command.spawn()?;
        state.child = Some(child.id());
        Ok(child)
    }

    /// The compile has been waited for.
    pub fn finished(&self) {
        lock(&self.0).finished = true;
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn watch(state: &Mutex<State>, parent: u32) {
    while !orphaned(parent, parent_id()) {
        thread::sleep(POLL);
    }
    let state = lock(state);
    if state.finished {
        return;
    }
    match state.child {
        // The wrapper then exits with the compile's status, as always.
        Some(child) => terminate(child),
        // Still waiting for a slot: leave instead of starting the compile.
        None => process::exit(128 + libc::SIGTERM),
    }
}

/// Whether the process that started this one has exited, given its ID then
/// and the parent's ID now.
fn orphaned(original: u32, now: u32) -> bool {
    if cfg!(target_os = "macos") {
        // An orphan's parent becomes launchd, process 1. A debugger that
        // attaches can also appear as the parent, which isn't an exit.
        now == 1 && original != 1
    } else {
        // An orphan's parent becomes init or any subreaper.
        now != original
    }
}

/// Sends SIGTERM to `child` and every process below it.
fn terminate(child: u32) {
    // Listed before any is signalled: once a process exits, the processes
    // it started can no longer be found from it.
    let below = crate::proxy::descendant_pids(child);
    for pid in std::iter::once(child).chain(below) {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            continue;
        };
        if pid > 1 {
            // SAFETY: kill takes no pointers. The ID is the wrapper's own
            // unreaped child, or was just listed as a process below it.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_parents_exit_counts_as_orphaned() {
        assert!(!orphaned(4121, 4121));
        assert!(orphaned(4121, 1));
        // Started by init itself, as cargo is when it is a container's
        // first process.
        assert!(!orphaned(1, 1));
        // A subreaper adopts orphans on Linux; macOS has none.
        assert_eq!(orphaned(4121, 977), !cfg!(target_os = "macos"));
    }
}
