//! Warm starts for new worktrees: `cargo steward seed` and the git hooks
//! that run it.
//!
//! A new worktree normally starts with an empty build directory and compiles
//! every dependency from scratch. Seeding starts it instead as a
//! copy-on-write clone of the repository's main build directory: the clone
//! takes a fraction of a second and no space, and since it keeps every
//! file's modification time, cargo finds the dependencies fresh and only
//! rebuilds crates whose sources are newer than their outputs.

pub(crate) mod clone;
mod hooks;

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::config::{self, Paths};
use crate::fsutil::{self, Lock};
use crate::{gc, scan};

pub use hooks::{HooksArgs, run_hooks};

use clone::Cloned;

/// How long to wait for a build in the baseline directory to finish before
/// giving up on seeding.
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// `cargo steward seed`. Its flags are a contract: the `cargo` proxy runs
/// `cargo-steward seed --target-dir <new> --from <baseline> --worktree
/// <checkout> --quiet`.
#[derive(Debug, Default, clap::Args)]
pub struct SeedArgs {
    /// The build directory to start [default: the worktree's, as `cargo
    /// metadata` reports it]
    #[arg(long, value_name = "DIR")]
    pub target_dir: Option<PathBuf>,
    /// The build directory to clone [default: that of the repository's main
    /// worktree]
    #[arg(long, value_name = "DIR")]
    pub from: Option<PathBuf>,
    /// The worktree [default: the current directory]
    #[arg(long, value_name = "DIR")]
    pub worktree: Option<PathBuf>,
    /// Print nothing
    #[arg(short, long)]
    pub quiet: bool,
}

/// What seeding one build directory did. Only a real I/O failure is an
/// error; each of these exits 0.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Outcome {
    Seeded(Duration),
    /// The target already holds a build.
    AlreadyBuilt,
    /// The target exists and holds something else; it's left alone.
    Occupied,
    /// There is no build to clone.
    NoBaseline,
    /// Target and baseline are the same directory, or one holds the other.
    Same,
    /// A build kept the baseline locked.
    Busy,
    /// The filesystem can't clone, or the two are on different volumes.
    Unsupported,
}

/// `cargo steward seed`.
pub fn run_seed(args: &SeedArgs) -> Result<()> {
    let paths = Paths::resolve().context("cannot locate the home directory")?;
    let config = config::load(&paths)?;
    let say = |line: String| {
        if !args.quiet {
            println!("{line}");
        }
    };
    if !config.seed {
        say("Seeding is turned off (`seed = false`).".into());
        return Ok(());
    }
    let worktree = match &args.worktree {
        Some(dir) => dir.clone(),
        None => env::current_dir().context("reading the current directory")?,
    };
    let worktree = std::path::absolute(&worktree).unwrap_or(worktree);

    let pairs = plan(args, &worktree);
    if pairs.is_empty() {
        say(format!(
            "Nothing to seed: no cargo workspace or main worktree found for {}",
            worktree.display()
        ));
    }
    for (target, baseline) in pairs {
        let outcome = seed_dir(&target, &baseline, LOCK_WAIT)?;
        let line = describe(&outcome, &target, &baseline);
        say(line);
    }
    Ok(())
}

/// The line `seed` prints for what seeding `target` from `baseline` did.
fn describe(outcome: &Outcome, target: &Path, baseline: &Path) -> String {
    match outcome {
        Outcome::Seeded(took) => format!(
            "Seeded {} from {} ({:.2} s, copy-on-write)",
            target.display(),
            baseline.display(),
            took.as_secs_f64()
        ),
        Outcome::AlreadyBuilt => format!("{} already holds a build", target.display()),
        Outcome::Occupied => format!(
            "Not seeding {}: it exists and isn't empty",
            target.display()
        ),
        Outcome::NoBaseline => format!("No build to seed from at {}", baseline.display()),
        Outcome::Same => format!(
            "{} is the main build directory; nothing to seed",
            target.display()
        ),
        Outcome::Busy => format!(
            "Not seeding {}: a build is using {}",
            target.display(),
            baseline.display()
        ),
        Outcome::Unsupported => format!(
            "Not seeding {}: its filesystem can't clone from {}",
            target.display(),
            baseline.display()
        ),
    }
}

/// A workspace's directories: where intermediate build output goes (the
/// build directory) and where final artifacts go (the target directory).
/// They're the same unless `build.build-dir` is set.
#[derive(Debug, Clone, PartialEq)]
struct Dirs {
    build: PathBuf,
    target: PathBuf,
}

impl Dirs {
    fn one(dir: &Path) -> Dirs {
        Dirs {
            build: dir.to_path_buf(),
            target: dir.to_path_buf(),
        }
    }
}

/// Pairs of (directory to start, directory to clone it from). Empty when
/// there's nothing to do, such as outside a cargo workspace.
fn plan(args: &SeedArgs, worktree: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mine = match &args.target_dir {
        Some(dir) => Some(Dirs::one(dir)),
        None => cargo_dirs(worktree),
    };
    let Some(mine) = mine else {
        return Vec::new();
    };
    let theirs = match &args.from {
        Some(dir) => Some(Dirs::one(dir)),
        None => main_worktree(worktree).and_then(|main| cargo_dirs(&main)),
    };
    let Some(theirs) = theirs else {
        return Vec::new();
    };
    let mut pairs = vec![(mine.build.clone(), theirs.build.clone())];
    // With a separate build-dir, final artifacts live in the target dir.
    if mine.target != mine.build && theirs.target != theirs.build {
        pairs.push((mine.target, theirs.target));
    }
    pairs
}

/// The workspace directories of the cargo project at `dir`, as `cargo
/// metadata` reports them, so the project's config and the environment are
/// respected. `None` if `dir` isn't in a cargo project.
fn cargo_dirs(dir: &Path) -> Option<Dirs> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let output = Command::new(cargo)
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_metadata(&output.stdout)
}

fn parse_metadata(json: &[u8]) -> Option<Dirs> {
    #[derive(Deserialize)]
    struct Metadata {
        target_directory: PathBuf,
        /// Reported by Cargo 1.91 and later.
        build_directory: Option<PathBuf>,
    }
    let meta: Metadata = serde_json::from_slice(json).ok()?;
    Some(Dirs {
        build: meta
            .build_directory
            .unwrap_or_else(|| meta.target_directory.clone()),
        target: meta.target_directory,
    })
}

/// The directory in the repository's main worktree that corresponds to
/// `dir` in another worktree: `dir`'s path below its worktree's top, joined
/// to the main worktree. `None` outside git, or for a bare repository.
fn main_worktree(dir: &Path) -> Option<PathBuf> {
    let common = PathBuf::from(git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?);
    let main = if common.file_name().is_some_and(|n| n == ".git") {
        common.parent()?.to_path_buf()
    } else {
        // A repository whose git directory is elsewhere: git lists the main
        // worktree first, unless the repository is bare.
        let list = git(dir, &["worktree", "list", "--porcelain"])?;
        let mut lines = list.lines();
        let first = lines.next()?.strip_prefix("worktree ")?;
        if lines.next() == Some("bare") {
            return None;
        }
        // Git lists the git directory itself when it doesn't know where the
        // main worktree is (`git init --separate-git-dir`).
        let first = PathBuf::from(first);
        if first == common {
            return None;
        }
        first
    };
    let top = PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"])?);
    let here = dir.canonicalize().ok()?;
    let below = here.strip_prefix(top.canonicalize().ok()?).ok()?;
    Some(main.join(below))
}

/// Runs git in `dir` and returns its output without the final newline, or
/// `None` if it fails or prints nothing. Variables a hook inherits from git
/// are cleared, so `dir` alone decides which repository this is.
pub(crate) fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_PREFIX")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim_end_matches(['\n', '\r']);
    (!text.is_empty()).then(|| text.to_owned())
}

/// Starts `target` as a clone of the build directory `baseline`.
///
/// The clone is made under a temporary name next to `target` while holding
/// cargo's locks on every profile of `baseline` (waiting up to `wait` for a
/// running build), then renamed into place, so no build ever sees a half
/// made directory. A pin is not inherited.
pub(crate) fn seed_dir(target: &Path, baseline: &Path, wait: Duration) -> Result<Outcome> {
    let target = std::path::absolute(target).unwrap_or_else(|_| target.to_path_buf());
    let Ok(baseline) = baseline.canonicalize() else {
        return Ok(Outcome::NoBaseline);
    };
    if !scan::is_cargo_root(&baseline) {
        return Ok(Outcome::NoBaseline);
    }
    let resolved = target.canonicalize().unwrap_or_else(|_| target.clone());
    if resolved.starts_with(&baseline) || baseline.starts_with(&resolved) {
        return Ok(Outcome::Same);
    }
    if holds_build(&target) {
        return Ok(Outcome::AlreadyBuilt);
    }
    if !is_empty_or_missing(&target) {
        return Ok(Outcome::Occupied);
    }
    let (Some(parent), Some(name)) = (target.parent(), target.file_name()) else {
        return Ok(Outcome::Same);
    };
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    if fsutil::device(parent) != fsutil::device(&baseline) {
        return Ok(Outcome::Unsupported);
    }

    let Some(locks) = lock_all(&baseline, wait) else {
        return Ok(Outcome::Busy);
    };
    let started = Instant::now();
    let staging = parent.join(format!(
        ".{}.steward-seed-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    let _ = fsutil::remove(&staging);
    let cloned = clone::clone_tree(&baseline, &staging);
    drop(locks);
    match cloned {
        Ok(Cloned::Done) => {}
        Ok(Cloned::Unsupported) => {
            let _ = fsutil::remove(&staging);
            return Ok(Outcome::Unsupported);
        }
        Err(e) => {
            let _ = fsutil::remove(&staging);
            return Err(e).with_context(|| {
                format!("cloning {} to {}", baseline.display(), staging.display())
            });
        }
    }
    let _ = fs::remove_file(staging.join(gc::PIN_FILE));
    fsutil::sweep_trash(&staging);

    // Replaces `target` only if it's missing or an empty directory.
    if let Err(e) = fs::rename(&staging, &target) {
        let _ = fsutil::remove(&staging);
        if !is_empty_or_missing(&target) {
            return Ok(if holds_build(&target) {
                Outcome::AlreadyBuilt
            } else {
                Outcome::Occupied
            });
        }
        return Err(e).with_context(|| format!("moving the clone to {}", target.display()));
    }
    Ok(Outcome::Seeded(started.elapsed()))
}

/// Whether a build has started in `dir`.
fn holds_build(dir: &Path) -> bool {
    scan::is_cargo_root(dir) || !scan::profiles(dir).is_empty()
}

fn is_empty_or_missing(dir: &Path) -> bool {
    match fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_none(),
        Err(e) => e.kind() == io::ErrorKind::NotFound,
    }
}

/// Cargo's locks on every profile of the build directory `root`, retrying
/// until `wait` has passed. `None` if a build kept any of them.
fn lock_all(root: &Path, wait: Duration) -> Option<Vec<Lock>> {
    let deadline = Instant::now() + wait;
    loop {
        let mut held = Vec::new();
        let mut busy = false;
        for profile in scan::profiles(root) {
            match fsutil::lock_profile(&profile.path) {
                Lock::Busy => {
                    busy = true;
                    break;
                }
                lock => held.push(lock),
            }
        }
        if !busy {
            return Some(held);
        }
        drop(held);
        if Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, FileTimes};
    use std::time::SystemTime;

    /// A pinned build directory with one dependency built a day ago.
    fn build_dir(root: &Path) -> PathBuf {
        let rlib = root.join("debug/deps/libdep-0123456789abcdef.rlib");
        fs::create_dir_all(root.join("debug/.fingerprint/dep-0123456789abcdef")).unwrap();
        fs::create_dir_all(rlib.parent().unwrap()).unwrap();
        fs::write(root.join(".rustc_info.json"), "{}").unwrap();
        fs::write(
            root.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n# created by cargo.\n",
        )
        .unwrap();
        fs::write(&rlib, vec![1u8; 80_000]).unwrap();
        let old = SystemTime::now() - Duration::from_secs(86_400);
        File::options()
            .write(true)
            .open(&rlib)
            .unwrap()
            .set_times(FileTimes::new().set_modified(old))
            .unwrap();
        fs::write(root.join(gc::PIN_FILE), "keep").unwrap();
        rlib
    }

    #[test]
    fn seeds_a_new_build_dir_from_the_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("main/target");
        let rlib = build_dir(&base);
        let target = tmp.path().join("wt/target");
        let outcome = seed_dir(&target, &base, Duration::ZERO).unwrap();
        if outcome == Outcome::Unsupported {
            assert!(!target.exists());
            return;
        }
        assert!(matches!(outcome, Outcome::Seeded(_)), "{outcome:?}");
        let copy = target.join("debug/deps/libdep-0123456789abcdef.rlib");
        assert_eq!(fs::read(&copy).unwrap(), fs::read(&rlib).unwrap());
        assert_eq!(
            fs::metadata(&copy).unwrap().modified().unwrap(),
            fs::metadata(&rlib).unwrap().modified().unwrap()
        );
        assert!(!target.join(gc::PIN_FILE).exists(), "pins aren't inherited");
        assert!(base.join(gc::PIN_FILE).exists());
        // Nothing is left under a temporary name.
        let names: Vec<String> = fsutil::entries(&tmp.path().join("wt"))
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, ["target"]);

        // Seeding again changes nothing.
        assert_eq!(
            seed_dir(&target, &base, Duration::ZERO).unwrap(),
            Outcome::AlreadyBuilt
        );
    }

    #[test]
    fn leaves_existing_same_and_missing_dirs_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("main/target");
        build_dir(&base);
        let built = tmp.path().join("built");
        build_dir(&built);
        fs::write(built.join("debug/deps/mine"), "mine").unwrap();
        assert_eq!(
            seed_dir(&built, &base, Duration::ZERO).unwrap(),
            Outcome::AlreadyBuilt
        );
        assert_eq!(fs::read(built.join("debug/deps/mine")).unwrap(), b"mine");

        let occupied = tmp.path().join("occupied");
        fs::create_dir_all(&occupied).unwrap();
        fs::write(occupied.join("notes"), "x").unwrap();
        assert_eq!(
            seed_dir(&occupied, &base, Duration::ZERO).unwrap(),
            Outcome::Occupied
        );

        assert_eq!(
            seed_dir(&base, &base, Duration::ZERO).unwrap(),
            Outcome::Same
        );
        assert_eq!(
            seed_dir(&base.join("debug"), &base, Duration::ZERO).unwrap(),
            Outcome::Same
        );
        let new = tmp.path().join("new");
        assert_eq!(
            seed_dir(&new, &tmp.path().join("nothing"), Duration::ZERO).unwrap(),
            Outcome::NoBaseline
        );
        assert!(!new.exists());
    }

    #[test]
    fn an_empty_target_dir_is_seeded() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("main/target");
        build_dir(&base);
        let target = tmp.path().join("wt/target");
        fs::create_dir_all(&target).unwrap();
        let outcome = seed_dir(&target, &base, Duration::ZERO).unwrap();
        assert!(
            matches!(outcome, Outcome::Seeded(_) | Outcome::Unsupported),
            "{outcome:?}"
        );
        if matches!(outcome, Outcome::Seeded(_)) {
            assert!(target.join(".rustc_info.json").exists());
        }
    }

    #[test]
    fn skips_a_baseline_a_build_is_using() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("main/target");
        build_dir(&base);
        let held = fsutil::lock_profile(&base.join("debug"));
        assert!(matches!(held, Lock::Held(_)));
        let target = tmp.path().join("wt/target");
        assert_eq!(
            seed_dir(&target, &base, Duration::from_millis(250)).unwrap(),
            Outcome::Busy
        );
        assert!(!target.exists());
        drop(held);
        // Child processes other tests start can briefly hold a copy of the
        // lock, so wait as the command does.
        let outcome = seed_dir(&target, &base, LOCK_WAIT).unwrap();
        assert!(
            matches!(outcome, Outcome::Seeded(_) | Outcome::Unsupported),
            "{outcome:?}"
        );
    }

    #[test]
    fn reads_both_directories_from_metadata() {
        let both = br#"{"packages":[],"target_directory":"/w/target","build_directory":"/b/x"}"#;
        assert_eq!(
            parse_metadata(both),
            Some(Dirs {
                build: "/b/x".into(),
                target: "/w/target".into()
            })
        );
        let old = br#"{"packages":[],"target_directory":"/w/target"}"#;
        assert_eq!(parse_metadata(old), Some(Dirs::one(Path::new("/w/target"))));
        assert_eq!(parse_metadata(b"not json"), None);
    }

    /// Runs git with no user or system config, so tests see plain defaults.
    fn git_ok(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
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

    #[test]
    fn finds_the_main_worktree_of_a_linked_one() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let repo = root.join("repo");
        fs::create_dir_all(repo.join("crates/app")).unwrap();
        git_ok(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join("crates/app/file"), "x").unwrap();
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
        let wt = root.join("wt");
        git_ok(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        assert_eq!(main_worktree(&wt), Some(repo.clone()));
        assert_eq!(
            main_worktree(&wt.join("crates/app")),
            Some(repo.join("crates/app"))
        );
        assert_eq!(main_worktree(&repo), Some(repo.clone()));
    }

    #[test]
    fn describes_every_outcome() {
        let (t, b) = (Path::new("/w/target"), Path::new("/m/target"));
        let cases = [
            (
                Outcome::Seeded(Duration::from_millis(1250)),
                "Seeded /w/target from /m/target (1.25 s, copy-on-write)",
            ),
            (Outcome::AlreadyBuilt, "/w/target already holds a build"),
            (
                Outcome::Occupied,
                "Not seeding /w/target: it exists and isn't empty",
            ),
            (Outcome::NoBaseline, "No build to seed from at /m/target"),
            (
                Outcome::Same,
                "/w/target is the main build directory; nothing to seed",
            ),
            (
                Outcome::Busy,
                "Not seeding /w/target: a build is using /m/target",
            ),
            (
                Outcome::Unsupported,
                "Not seeding /w/target: its filesystem can't clone from /m/target",
            ),
        ];
        for (outcome, line) in cases {
            assert_eq!(describe(&outcome, t, b), line);
        }
    }

    #[test]
    fn plans_explicit_dirs_and_nothing_without_a_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        let args = SeedArgs {
            target_dir: Some("/w/target".into()),
            from: Some("/m/target".into()),
            ..SeedArgs::default()
        };
        assert_eq!(
            plan(&args, tmp.path()),
            [(PathBuf::from("/w/target"), PathBuf::from("/m/target"))]
        );
        // Outside git there's no main worktree to clone from.
        let args = SeedArgs {
            target_dir: Some("/w/target".into()),
            ..SeedArgs::default()
        };
        assert!(plan(&args, tmp.path()).is_empty());
        assert_eq!(main_worktree(tmp.path()), None);
    }

    #[test]
    fn handles_repositories_whose_git_dir_is_elsewhere() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let repo = root.join("repo");
        fs::create_dir_all(repo.join("sub")).unwrap();
        let gitdir = root.join("gitdir");
        git_ok(
            &repo,
            &[
                "init",
                "-q",
                "-b",
                "main",
                "--separate-git-dir",
                gitdir.to_str().unwrap(),
            ],
        );
        // Git doesn't record where this repository's worktree is, and lists
        // the git directory in its place: that's no worktree to seed from.
        assert_eq!(main_worktree(&repo.join("sub")), None);

        // Nor from a linked worktree of it.
        git_ok(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        let wt = root.join("wt");
        git_ok(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        assert_eq!(main_worktree(&wt), None);

        // A bare repository has no main worktree.
        let bare = root.join("bare.git");
        fs::create_dir_all(&bare).unwrap();
        git_ok(&bare, &["init", "-q", "--bare"]);
        assert_eq!(main_worktree(&bare), None);
    }

    #[test]
    fn plans_nothing_when_the_main_worktree_isnt_a_cargo_project() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().canonicalize().unwrap().join("repo");
        fs::create_dir_all(&repo).unwrap();
        git_ok(&repo, &["init", "-q", "-b", "main"]);
        let args = SeedArgs {
            target_dir: Some(repo.join("target")),
            ..SeedArgs::default()
        };
        assert_eq!(main_worktree(&repo), Some(repo.clone()));
        assert!(plan(&args, &repo).is_empty());
    }

    #[test]
    fn a_baseline_that_isnt_a_build_dir_is_no_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("notes");
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("readme"), "x").unwrap();
        let target = tmp.path().join("wt/target");
        assert_eq!(
            seed_dir(&target, &base, Duration::ZERO).unwrap(),
            Outcome::NoBaseline
        );
        assert!(!target.exists());
    }

    #[test]
    fn a_target_without_a_name_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("main/target");
        build_dir(&base);
        let other = tmp.path().join("other");
        fs::create_dir_all(&other).unwrap();
        // `other/missing/..` names no directory of its own to create.
        let target = other.join("missing/..");
        assert_eq!(
            seed_dir(&target, &base, Duration::ZERO).unwrap(),
            Outcome::Same
        );
        assert!(!other.join("missing").exists());
    }

    /// A target on another volume can't share the baseline's blocks.
    #[cfg(unix)]
    #[test]
    fn a_target_on_another_volume_is_unsupported() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("main/target");
        build_dir(&base);
        // Never created: the device check comes first.
        let target = Path::new("/dev/cargo-steward-seed-test");
        if fsutil::device(Path::new("/dev")) == fsutil::device(&base) {
            return;
        }
        assert_eq!(
            seed_dir(target, &base, Duration::ZERO).unwrap(),
            Outcome::Unsupported
        );
        assert!(!target.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_clone_that_fails_is_an_error_and_leaves_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("main/target");
        build_dir(&base);
        let parent = tmp.path().join("readonly");
        fs::create_dir_all(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();
        let result = seed_dir(&parent.join("target"), &base, Duration::ZERO);
        let names: Vec<String> = fsutil::entries(&parent).map(|(n, _)| n).collect();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        match result {
            Ok(outcome) => assert_eq!(outcome, Outcome::Unsupported),
            Err(e) => assert!(format!("{e:#}").contains("cloning"), "{e:#}"),
        }
        assert!(names.is_empty(), "{names:?}");
    }

    /// A dangling symlink looks missing, but the clone can't replace it.
    #[cfg(unix)]
    #[test]
    fn a_clone_that_cant_be_moved_into_place_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("main/target");
        build_dir(&base);
        let wt = tmp.path().join("wt");
        fs::create_dir_all(&wt).unwrap();
        let target = wt.join("target");
        std::os::unix::fs::symlink(tmp.path().join("nowhere"), &target).unwrap();
        match seed_dir(&target, &base, Duration::ZERO) {
            Ok(outcome) => assert_eq!(outcome, Outcome::Unsupported),
            Err(e) => assert!(format!("{e:#}").contains("moving the clone"), "{e:#}"),
        }
        // The staged clone is gone; the symlink is untouched.
        let names: Vec<String> = fsutil::entries(&wt).map(|(n, _)| n).collect();
        assert_eq!(names, ["target"]);
        assert!(fs::symlink_metadata(&target).unwrap().is_symlink());
    }

    /// With a separate build-dir, both it and the target dir are seeded,
    /// each from the main worktree's.
    #[test]
    fn plans_the_build_and_target_dirs_of_a_linked_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let repo = root.join("repo");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(repo.join(".cargo")).unwrap();
        fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(repo.join("src/lib.rs"), "").unwrap();
        fs::write(
            repo.join(".cargo/config.toml"),
            "[build]\nbuild-dir = \"bdir\"\n",
        )
        .unwrap();
        git_ok(&repo, &["init", "-q", "-b", "main"]);
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
        let wt = root.join("wt");
        git_ok(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        let dirs = cargo_dirs(&wt).expect("cargo metadata in the worktree");
        if dirs.build == dirs.target {
            // A Cargo too old to report build_directory.
            return;
        }
        let pairs = plan(&SeedArgs::default(), &wt);
        assert_eq!(pairs.len(), 2, "{pairs:?}");
        assert_eq!(pairs[0], (wt.join("bdir"), repo.join("bdir")));
        // The target dirs, unless CARGO_TARGET_DIR puts both elsewhere.
        assert_eq!(pairs[1].0, dirs.target);
    }
}
