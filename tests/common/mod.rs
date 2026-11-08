//! Shared by the end-to-end tests.

use std::process::Command;

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
