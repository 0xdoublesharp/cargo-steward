//! Shared by the end-to-end tests.

// Each test binary uses only some of this.
#![allow(dead_code)]

use std::io;
use std::process::{Child, Command};
use std::thread::sleep;
use std::time::Duration;

/// Variables with which git picks a repository other than the one its
/// working directory is in. Git sets them for the hooks and `rebase --exec`
/// commands it runs; a test started from one must not act on that
/// repository.
pub const GIT_REPO_VARS: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_PREFIX",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
];

/// Removes [`GIT_REPO_VARS`] from `command`'s environment.
pub fn isolate_from_git(command: &mut Command) -> &mut Command {
    for var in GIT_REPO_VARS {
        command.env_remove(var);
    }
    command
}

/// Spawns `command`, retrying while Linux reports its program busy. A
/// program file can't be run while any process has it open for writing, and
/// a test thread that forks just as another copies a program carries the
/// copy's open handle until its child execs.
pub fn spawn_retrying(command: &mut Command) -> Child {
    for _ in 0..100 {
        match command.spawn() {
            Err(e) if e.kind() == io::ErrorKind::ExecutableFileBusy => {
                sleep(Duration::from_millis(20));
            }
            spawned => return spawned.unwrap(),
        }
    }
    command.spawn().unwrap()
}
