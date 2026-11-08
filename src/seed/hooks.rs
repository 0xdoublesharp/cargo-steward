//! `cargo steward hooks`: a git `post-checkout` hook that seeds the build
//! directory of each new worktree.
//!
//! When git creates a worktree it runs `post-checkout` in it with the null
//! object ID as the first argument; the hook then runs `cargo-steward seed`.
//! A hook that was already there is moved aside to
//! `post-checkout.pre-steward`, run first, and its exit status kept, and
//! `uninstall` moves it back.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::{self, Paths};

use super::git;

/// The hook git runs after a checkout, including the one that populates a
/// new worktree.
const HOOK: &str = "post-checkout";

/// Where a hook that was already installed is kept while ours chains it.
const CHAINED: &str = "post-checkout.pre-steward";

/// Marks a hook as ours, so it's never mistaken for someone else's.
const MARKER: &str = "# cargo-steward: seeds new worktrees' build directories";

/// `cargo steward hooks`.
#[derive(Debug, clap::Args)]
pub struct HooksArgs {
    #[command(subcommand)]
    pub action: HooksAction,
}

#[derive(Debug, clap::Subcommand)]
pub enum HooksAction {
    /// Seed each new worktree of this repository when git creates it
    Install {
        /// The repository [default: the current directory]
        #[arg(long, value_name = "DIR")]
        repo: Option<PathBuf>,
    },
    /// Remove the hook, restoring any it replaced
    Uninstall {
        /// The repository [default: the current directory]
        #[arg(long, value_name = "DIR")]
        repo: Option<PathBuf>,
    },
    /// Show whether the hook is installed
    Status {
        /// The repository [default: the current directory]
        #[arg(long, value_name = "DIR")]
        repo: Option<PathBuf>,
    },
}

/// `cargo steward hooks`.
pub fn run_hooks(args: &HooksArgs) -> Result<()> {
    let (HooksAction::Install { repo }
    | HooksAction::Uninstall { repo }
    | HooksAction::Status { repo }) = &args.action;
    let repo = match repo {
        Some(dir) => dir.clone(),
        None => std::env::current_dir().context("reading the current directory")?,
    };
    let dir = hooks_dir(&repo)?;
    match &args.action {
        HooksAction::Install { .. } => {
            let chained = install(&dir, &steward_exe())?;
            println!("Installed {}", dir.join(HOOK).display());
            if chained {
                println!("It runs the hook that was there first ({CHAINED}).");
            }
            if let Some(paths) = Paths::resolve()
                && !config::load_lenient(&paths).0.seed
            {
                println!(
                    "Note: seeding is turned off (`seed = false` in {}).",
                    paths.config().display()
                );
            }
        }
        HooksAction::Uninstall { .. } => {
            if uninstall(&dir)? {
                println!("Removed the cargo-steward hook from {}", dir.display());
                if dir.join(HOOK).exists() {
                    println!("Restored the {HOOK} hook it had replaced.");
                }
            } else {
                println!("No cargo-steward hook in {}", dir.display());
            }
        }
        HooksAction::Status { .. } => println!("{}", status(&dir)),
    }
    Ok(())
}

/// The repository's hooks directory: `core.hooksPath` if set, else the
/// common git directory's `hooks/`. Shared by all of its worktrees.
pub(crate) fn hooks_dir(repo: &Path) -> Result<PathBuf> {
    // `--git-path hooks` resolves `core.hooksPath` too.
    let Some(path) = git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-path", "hooks"],
    ) else {
        bail!("{} isn't in a git repository", repo.display());
    };
    Ok(PathBuf::from(path))
}

/// The cargo-steward the hook runs: this executable, else `cargo-steward`
/// on PATH.
fn steward_exe() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.into_os_string().into_string().ok())
        .unwrap_or_else(|| "cargo-steward".to_owned())
}

/// The hook script. `exe` runs `seed` for new worktrees; `chained` is the
/// hook it replaced, run first.
fn script(exe: &str, chained: &Path) -> String {
    format!(
        r#"#!/bin/sh
{MARKER}
# (`cargo steward hooks`). When git creates a worktree, start its build
# directory as a copy-on-write clone of the main worktree's. Never fails git.
# `cargo steward hooks uninstall` removes this and restores {CHAINED}.
status=0
if [ -x {chained} ]; then
    {chained} "$@"
    status=$?
fi
# A new worktree: the previous HEAD is the null object ID.
case "$1" in
    "" | *[!0]*) ;;
    *) {exe} seed --worktree "$PWD" --quiet </dev/null >/dev/null 2>&1 || true ;;
esac
exit $status
"#,
        chained = quote(&chained.to_string_lossy()),
        exe = quote(exe),
    )
}

/// Quotes `text` for a POSIX shell.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn is_ours(hook: &Path) -> bool {
    fs::read_to_string(hook).is_ok_and(|text| text.contains(MARKER))
}

/// Installs the hook in `dir`, moving a hook that was there aside to be
/// chained. Reinstalling updates ours in place. Returns whether a previous
/// hook is chained.
pub(crate) fn install(dir: &Path, exe: &str) -> Result<bool> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let hook = dir.join(HOOK);
    let chained = dir.join(CHAINED);
    let exists = fs::symlink_metadata(&hook).is_ok();
    if exists && !is_ours(&hook) {
        if fs::symlink_metadata(&chained).is_ok() {
            bail!(
                "{} exists and so does {}; move one of them before installing",
                hook.display(),
                chained.display()
            );
        }
        fs::rename(&hook, &chained).with_context(|| format!("moving {} aside", hook.display()))?;
    }
    let staging = dir.join(format!(".{HOOK}.steward-{}", std::process::id()));
    fs::write(&staging, script(exe, &chained))
        .with_context(|| format!("writing {}", staging.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o755))?;
    }
    fs::rename(&staging, &hook).with_context(|| format!("writing {}", hook.display()))?;
    Ok(fs::symlink_metadata(&chained).is_ok())
}

/// Removes our hook from `dir` and moves back the one it chained, exactly
/// as it was. Returns whether there was a hook of ours.
pub(crate) fn uninstall(dir: &Path) -> Result<bool> {
    let hook = dir.join(HOOK);
    if !is_ours(&hook) {
        return Ok(false);
    }
    match fs::remove_file(&hook) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", hook.display()));
        }
        _ => {}
    }
    let chained = dir.join(CHAINED);
    if fs::symlink_metadata(&chained).is_ok() {
        fs::rename(&chained, &hook).with_context(|| format!("restoring {}", hook.display()))?;
    }
    Ok(true)
}

/// A line describing the hook in `dir`.
fn status(dir: &Path) -> String {
    let hook = dir.join(HOOK);
    if fs::symlink_metadata(&hook).is_err() {
        return format!("Not installed (no {HOOK} hook in {}).", dir.display());
    }
    if !is_ours(&hook) {
        return format!(
            "Not installed: {} is another hook. `cargo steward hooks install` keeps it and runs it first.",
            hook.display()
        );
    }
    let mut line = format!("Installed: {}", hook.display());
    if fs::symlink_metadata(dir.join(CHAINED)).is_ok() {
        line.push_str(&format!(" (runs {CHAINED} first)"));
    }
    if let Some(paths) = Paths::resolve()
        && !config::load_lenient(&paths).0.seed
    {
        line.push_str("\nSeeding is turned off (`seed = false`), so the hook does nothing.");
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git_ok(dir: &Path, args: &[&str]) {
        let out = crate::seed::git_command()
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn chains_an_existing_hook_and_restores_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        git_ok(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join("file"), "x").unwrap();
        git_ok(&repo, &["add", "."]);
        git_ok(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "init",
            ],
        );
        let dir = hooks_dir(&repo).unwrap();
        assert!(dir.starts_with(&repo), "{}", dir.display());

        // Someone's hook: records that it ran, and fails.
        let theirs = format!(
            "#!/bin/sh\necho \"$1\" >> {}\nexit 3\n",
            quote(&root.join("theirs.log").to_string_lossy())
        );
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(HOOK), &theirs).unwrap();
        make_executable(&dir.join(HOOK));

        // Stands in for cargo-steward: records its arguments.
        let fake = root.join("fake-steward");
        fs::write(
            &fake,
            format!(
                "#!/bin/sh\necho \"$@\" >> {}\n",
                quote(&root.join("seeded.log").to_string_lossy())
            ),
        )
        .unwrap();
        make_executable(&fake);

        assert!(install(&dir, &fake.to_string_lossy()).unwrap());
        assert_eq!(fs::read_to_string(dir.join(CHAINED)).unwrap(), theirs);
        // Reinstalling keeps the chained hook.
        assert!(install(&dir, &fake.to_string_lossy()).unwrap());
        assert!(status(&dir).starts_with("Installed"), "{}", status(&dir));

        let wt = root.join("wt");
        let out = crate::seed::git_command()
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "add", "-q"])
            .arg(&wt)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        // The chained hook's failure is git's to report, as before.
        assert!(!out.status.success());
        assert!(wt.join("file").exists());
        let theirs_ran = fs::read_to_string(root.join("theirs.log")).unwrap();
        assert!(theirs_ran.starts_with("0000000000"), "{theirs_ran}");
        let seeded = fs::read_to_string(root.join("seeded.log")).unwrap();
        assert_eq!(
            seeded.trim(),
            format!("seed --worktree {} --quiet", wt.display())
        );

        // A plain checkout doesn't seed (and still reports the chained
        // hook's status).
        let out = crate::seed::git_command()
            .arg("-C")
            .arg(&wt)
            .args(["checkout", "-q", "-b", "other"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert_eq!(
            fs::read_to_string(root.join("theirs.log"))
                .unwrap()
                .lines()
                .count(),
            2
        );
        let seeded = fs::read_to_string(root.join("seeded.log")).unwrap();
        assert_eq!(seeded.lines().count(), 1);

        assert!(uninstall(&dir).unwrap());
        assert_eq!(fs::read_to_string(dir.join(HOOK)).unwrap(), theirs);
        assert!(!dir.join(CHAINED).exists());
        assert!(!uninstall(&dir).unwrap(), "theirs isn't ours to remove");
        assert_eq!(fs::read_to_string(dir.join(HOOK)).unwrap(), theirs);
    }

    #[cfg(unix)]
    #[test]
    fn installs_alone_and_never_fails_git() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("hooks");
        assert!(!install(&dir, "/nonexistent/cargo-steward").unwrap());
        assert!(!dir.join(CHAINED).exists());
        // A missing cargo-steward still exits 0 for a new worktree.
        let out = Command::new(dir.join(HOOK))
            .args(["0".repeat(40), "a".repeat(40), "1".into()])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(uninstall(&dir).unwrap());
        assert!(!dir.join(HOOK).exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_hook_that_cant_be_removed_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("hooks");
        install(&dir, "cargo-steward").unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        let result = uninstall(&dir);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("removing"), "{err}");
        assert!(is_ours(&dir.join(HOOK)));
    }

    #[test]
    fn refuses_to_clobber_a_parked_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::write(dir.join(HOOK), "#!/bin/sh\n").unwrap();
        fs::write(dir.join(CHAINED), "#!/bin/sh\nexit 0\n").unwrap();
        assert!(install(dir, "cargo-steward").is_err());
        assert_eq!(fs::read_to_string(dir.join(HOOK)).unwrap(), "#!/bin/sh\n");
    }

    #[test]
    fn quotes_for_the_shell() {
        assert_eq!(quote("/a b/it's"), r"'/a b/it'\''s'");
    }

    #[test]
    fn reports_status_of_each_kind_of_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("hooks");
        fs::create_dir_all(&dir).unwrap();
        assert!(status(&dir).starts_with("Not installed (no post-checkout hook in"));

        fs::write(dir.join(HOOK), "#!/bin/sh\necho mine\n").unwrap();
        assert!(status(&dir).contains("is another hook"), "{}", status(&dir));
        assert!(!uninstall(&dir).unwrap(), "someone else's hook stays");
        assert!(dir.join(HOOK).exists());

        assert!(install(&dir, "/bin/cargo-steward").unwrap());
        let line = status(&dir);
        assert!(line.starts_with("Installed: "), "{line}");
        assert!(line.contains(&format!("(runs {CHAINED} first)")), "{line}");
    }

    #[test]
    fn hooks_dir_needs_a_git_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let err = hooks_dir(tmp.path()).unwrap_err().to_string();
        assert!(err.contains("isn't in a git repository"), "{err}");

        git_ok(tmp.path(), &["init", "-q"]);
        // `.git/hooks`, unless the user's git config sets `core.hooksPath`.
        assert!(hooks_dir(tmp.path()).unwrap().is_absolute());
    }
}
