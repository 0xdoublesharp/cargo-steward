//! Warm starts for new worktrees: `cargo steward seed` and the git hooks
//! that run it.
//!
//! A new worktree normally starts with an empty build directory and compiles
//! every dependency from scratch. Seeding starts it instead as a
//! copy-on-write clone of another worktree's build directory (the main
//! checkout's when a build isn't using it, else the most recently used
//! other one): the clone takes a fraction of a second and no space, and
//! since it keeps every file's modification time, cargo finds the
//! dependencies fresh and only rebuilds crates whose sources are newer than
//! their outputs.

pub(crate) mod baseline;
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
use crate::{gc, ledger, scan};

pub use hooks::{HooksArgs, run_hooks};

use baseline::Candidate;
use clone::Cloned;

/// How long to wait in all for builds to release a build directory to seed
/// from before giving up.
pub(crate) const LOCK_WAIT: Duration = Duration::from_secs(5);

/// How long to wait for a build to release one candidate before trying the
/// next.
const EACH_WAIT: Duration = Duration::from_millis(500);

/// `cargo steward seed`. Its flags are a contract: the `cargo` proxy runs
/// `cargo-steward seed --target-dir <new> --worktree <dir> --quiet`.
#[derive(Debug, Default, clap::Args)]
pub struct SeedArgs {
    /// The target directory to start [default: the worktree's, as `cargo
    /// metadata` reports it]. With `build.build-dir` set, the worktree's
    /// build directory is started too.
    #[arg(long, value_name = "DIR")]
    pub target_dir: Option<PathBuf>,
    /// A build directory to clone, instead of looking for one; repeat it to
    /// name several. The first is used unless a build is using it, then the
    /// most recently used of the others [default: the main checkout's, else
    /// the most recently used of the repository's other worktrees']
    #[arg(long, value_name = "DIR")]
    pub from: Vec<PathBuf>,
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
    /// Builds kept every baseline locked.
    Busy,
    /// The filesystem can't clone, or the two are on different volumes.
    Unsupported,
}

/// What seeding a directory from the best of several candidates did.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Seeding {
    pub outcome: Outcome,
    /// The candidate it was cloned from (or would have been, had the clone
    /// worked).
    pub from: Option<usize>,
    /// The candidates holding a build it could use, in the order tried.
    pub tried: Vec<usize>,
}

/// `cargo steward seed`.
pub fn run_seed(args: &SeedArgs) -> Result<()> {
    let paths = Paths::resolve().context("cannot locate the home directory")?;
    let config = config::load_warning(&paths);
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

    let registry = ledger::peek(&paths);
    let Some(job) = plan(args, &worktree, &registry) else {
        say(format!(
            "Nothing to seed: no cargo workspace found at {}",
            worktree.display()
        ));
        return Ok(());
    };
    let seeding = seed_best(&job.mine.build, &job.candidates, EACH_WAIT, LOCK_WAIT)?;
    say(describe(&seeding, &job.mine.build, &job.candidates));
    // With a separate build-dir, final artifacts live in the target dir:
    // start it from the same worktree's.
    if !same_path(&job.mine.build, &job.mine.target)
        && matches!(seeding.outcome, Outcome::Seeded(_))
        && let Some(i) = seeding.from
        && let Some(theirs) = &job.candidates[i].target
    {
        let from = Candidate {
            dir: theirs.clone(),
            target: None,
            ..job.candidates[i].clone()
        };
        let again = seed_best(
            &job.mine.target,
            std::slice::from_ref(&from),
            LOCK_WAIT,
            LOCK_WAIT,
        )?;
        say(describe(&again, &job.mine.target, &[from]));
    }
    Ok(())
}

/// The line `seed` prints for seeding `target` from one of `candidates`.
fn describe(seeding: &Seeding, target: &Path, candidates: &[Candidate]) -> String {
    let t = target.display();
    let dirs = |which: &[usize]| {
        let names: Vec<String> = which
            .iter()
            .map(|&i| candidates[i].dir.display().to_string())
            .collect();
        match names.as_slice() {
            [] => String::new(),
            [one] => one.clone(),
            [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
        }
    };
    let all: Vec<usize> = (0..candidates.len()).collect();
    match &seeding.outcome {
        Outcome::Seeded(took) => {
            let Some(from) = seeding.from.map(|i| &candidates[i]) else {
                return format!("Seeded {t}");
            };
            let mut notes = Vec::new();
            let about = from.about();
            if !about.is_empty() {
                notes.push(about);
            }
            let busy: Vec<String> = seeding
                .tried
                .iter()
                .take_while(|&&i| Some(i) != seeding.from)
                .map(|&i| match candidates[i].label.as_str() {
                    "" => candidates[i].dir.display().to_string(),
                    label => label.to_owned(),
                })
                .collect();
            if !busy.is_empty() {
                notes.push(format!("busy: {}", busy.join(", ")));
            }
            notes.push(format!("{:.2} s, copy-on-write", took.as_secs_f64()));
            format!(
                "Seeded {t} from {} ({})",
                from.dir.display(),
                notes.join("; ")
            )
        }
        Outcome::AlreadyBuilt => format!("{t} already holds a build"),
        Outcome::Occupied => format!("Not seeding {t}: it exists and isn't empty"),
        Outcome::NoBaseline if candidates.is_empty() => {
            format!("No build to seed {t} from: the repository has no other worktree")
        }
        Outcome::NoBaseline => format!("No build to seed from at {}", dirs(&all)),
        Outcome::Same => {
            format!("{t} is the build directory it would be seeded from; nothing to seed")
        }
        Outcome::Busy if seeding.tried.len() > 1 => {
            format!("Not seeding {t}: builds are using {}", dirs(&seeding.tried))
        }
        Outcome::Busy => format!("Not seeding {t}: a build is using {}", dirs(&seeding.tried)),
        Outcome::Unsupported => format!(
            "Not seeding {t}: its filesystem can't clone from {}",
            dirs(&seeding.tried)
        ),
    }
}

/// A workspace's directories: where intermediate build output goes (the
/// build directory) and where final artifacts go (the target directory).
/// They're the same unless `build.build-dir` is set.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Dirs {
    pub build: PathBuf,
    pub target: PathBuf,
}

impl Dirs {
    pub(crate) fn one(dir: &Path) -> Dirs {
        Dirs {
            build: dir.to_path_buf(),
            target: dir.to_path_buf(),
        }
    }
}

/// What to seed: this worktree's directories, and the candidates to seed
/// its build directory from, ranked.
#[derive(Debug)]
struct Job {
    mine: Dirs,
    candidates: Vec<Candidate>,
}

/// What to seed for `args` in `worktree`. `None` outside a cargo workspace,
/// when there's no directory to seed.
fn plan(args: &SeedArgs, worktree: &Path, registry: &ledger::Registry) -> Option<Job> {
    if !args.from.is_empty() {
        let build = match &args.target_dir {
            Some(dir) => dir.clone(),
            None => cargo_dirs(worktree, None)?.build,
        };
        let candidates = args
            .from
            .iter()
            .enumerate()
            .map(|(i, dir)| Candidate::given(dir, i == 0, registry))
            .collect();
        return Some(Job {
            mine: Dirs::one(&build),
            candidates: baseline::rank(candidates),
        });
    }
    let mine = match &args.target_dir {
        // Cargo reports the build-dir that goes with this target dir.
        Some(target) => match cargo_dirs(worktree, Some(target)) {
            Some(dirs) if !same_path(&dirs.build, target) => Dirs {
                build: dirs.build,
                target: target.clone(),
            },
            _ => Dirs::one(target),
        },
        None => cargo_dirs(worktree, None)?,
    };
    let candidates = baseline::discover(worktree, &mine, registry);
    Some(Job { mine, candidates })
}

/// Whether two paths name the same directory, existing or not.
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    a == b || crate::proxy::route::resolve(a) == crate::proxy::route::resolve(b)
}

/// The workspace directories of the cargo project at `dir`, as `cargo
/// metadata` reports them, so the project's config and the environment are
/// respected; with `target_dir`, as they are for a build into it. `None` if
/// `dir` isn't in a cargo project.
pub(crate) fn cargo_dirs(dir: &Path, target_dir: Option<&Path>) -> Option<Dirs> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut command = Command::new(cargo);
    command
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    if let Some(target) = target_dir {
        command.env("CARGO_TARGET_DIR", target);
    }
    let output = command.output().ok()?;
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

/// Variables with which git picks a repository other than the one its
/// working directory is in. Git sets them for the hooks and `rebase --exec`
/// commands it runs, so anything those start would otherwise act on the
/// repository that ran them.
pub(crate) const GIT_REPO_VARS: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_PREFIX",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
];

/// `git`, finding its repository from the directory it runs in alone
/// ([`GIT_REPO_VARS`] removed).
pub(crate) fn git_command() -> Command {
    let mut command = Command::new("git");
    for var in GIT_REPO_VARS {
        command.env_remove(var);
    }
    command
}

/// Runs git in `dir` and returns its output without the final newline, or
/// `None` if it fails or prints nothing. Variables a hook inherits from git
/// are cleared, so `dir` alone decides which repository this is.
pub(crate) fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = git_command()
        .arg("-C")
        .arg(dir)
        .args(args)
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

/// Starts `target` as a clone of the build directory `baseline`, waiting
/// up to `wait` for a build using it to finish.
#[cfg(test)]
pub(crate) fn seed_dir(target: &Path, baseline: &Path, wait: Duration) -> Result<Outcome> {
    let from = Candidate::given(baseline, true, &ledger::Registry::default());
    Ok(seed_best(target, &[from], wait, wait)?.outcome)
}

/// Starts `target` as a clone of the first of the ranked `candidates` that
/// holds a build no build is using.
///
/// Each candidate is tried in turn, waiting up to `each` for a build using
/// it to finish; if builds are using them all, they're tried again in turn
/// until `total` has passed. The clone is made under a temporary name next
/// to `target` while holding cargo's locks on every profile of the
/// candidate, then renamed into place, so no build ever sees a half made
/// directory. A pin is not inherited.
pub(crate) fn seed_best(
    target: &Path,
    candidates: &[Candidate],
    each: Duration,
    total: Duration,
) -> Result<Seeding> {
    let done = |outcome, from, tried| {
        Ok(Seeding {
            outcome,
            from,
            tried,
        })
    };
    let target = std::path::absolute(target).unwrap_or_else(|_| target.to_path_buf());
    let resolved = target.canonicalize().unwrap_or_else(|_| target.clone());
    let mut usable: Vec<(usize, PathBuf)> = Vec::new();
    let mut same = false;
    for (i, candidate) in candidates.iter().enumerate() {
        let Ok(dir) = candidate.dir.canonicalize() else {
            continue;
        };
        if !holds_build(&dir) {
            continue;
        }
        if resolved.starts_with(&dir) || dir.starts_with(&resolved) {
            same = true;
            continue;
        }
        usable.push((i, dir));
    }
    if usable.is_empty() {
        let outcome = if same {
            Outcome::Same
        } else {
            Outcome::NoBaseline
        };
        return done(outcome, None, Vec::new());
    }
    let tried = |usable: &[(usize, PathBuf)]| usable.iter().map(|(i, _)| *i).collect();
    if holds_build(&target) {
        return done(Outcome::AlreadyBuilt, None, tried(&usable));
    }
    if !is_empty_or_missing(&target) {
        return done(Outcome::Occupied, None, tried(&usable));
    }
    let (Some(parent), Some(name)) = (target.parent(), target.file_name()) else {
        return done(Outcome::Same, None, tried(&usable));
    };
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let device = fsutil::device(parent);
    let all = tried(&usable);
    usable.retain(|(_, dir)| fsutil::device(dir) == device);
    if usable.is_empty() {
        return done(Outcome::Unsupported, None, all);
    }

    let dirs: Vec<&Path> = usable.iter().map(|(_, d)| d.as_path()).collect();
    let tried = tried(&usable);
    let Some((k, locks)) = lock_first(&dirs, each, total) else {
        return done(Outcome::Busy, None, tried);
    };
    let (from, baseline) = (usable[k].0, &usable[k].1);
    let started = Instant::now();
    let staging = parent.join(format!(
        ".{}.steward-seed-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    let _ = fsutil::remove(&staging);
    let cloned = clone::clone_tree(baseline, &staging);
    drop(locks);
    match cloned {
        Ok(Cloned::Done) => {}
        Ok(Cloned::Unsupported) => {
            let _ = fsutil::remove(&staging);
            return done(Outcome::Unsupported, Some(from), vec![from]);
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
            let outcome = if holds_build(&target) {
                Outcome::AlreadyBuilt
            } else {
                Outcome::Occupied
            };
            return done(outcome, None, tried);
        }
        return Err(e).with_context(|| format!("moving the clone to {}", target.display()));
    }
    done(Outcome::Seeded(started.elapsed()), Some(from), tried)
}

/// Whether a build has started in `dir`: it's tagged as cargo's, or holds
/// build profiles (a directory made before cargo ran, as the proxy makes
/// target directories, has no tag).
pub(crate) fn holds_build(dir: &Path) -> bool {
    scan::is_cargo_root(dir) || !scan::profiles(dir).is_empty()
}

fn is_empty_or_missing(dir: &Path) -> bool {
    match fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_none(),
        Err(e) => e.kind() == io::ErrorKind::NotFound,
    }
}

/// Cargo's locks on every profile of the first of the build directories
/// `dirs` that no build is using, and which one that is. Each is tried in
/// turn for up to `each`, then all of them in turn until `total` has passed.
fn lock_first(dirs: &[&Path], each: Duration, total: Duration) -> Option<(usize, Vec<Lock>)> {
    let deadline = Instant::now() + total;
    for (i, dir) in dirs.iter().enumerate() {
        let wait = each.min(deadline.saturating_duration_since(Instant::now()));
        if let Some(locks) = lock_all(dir, wait) {
            return Some((i, locks));
        }
    }
    while Instant::now() < deadline {
        sleep(Duration::from_millis(100));
        for (i, dir) in dirs.iter().enumerate() {
            if let Some(locks) = lock_all(dir, Duration::ZERO) {
                return Some((i, locks));
            }
        }
    }
    None
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

    fn worktree_candidate(dir: &Path, label: &str, main: bool, age: u64) -> Candidate {
        Candidate {
            dir: dir.to_path_buf(),
            target: None,
            label: label.into(),
            main,
            last_used: SystemTime::now() - Duration::from_secs(age),
        }
    }

    /// The main checkout's build is busy: the most recently used idle
    /// worktree's is used, without waiting out the whole budget.
    #[test]
    fn a_busy_main_falls_back_to_the_most_recent_idle_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main/target");
        let older = tmp.path().join("older/target");
        let newer = tmp.path().join("newer/target");
        let busy = tmp.path().join("busy/target");
        for dir in [&main, &older, &newer, &busy] {
            build_dir(dir);
        }
        fs::write(newer.join("debug/deps/newer"), "x").unwrap();
        let candidates = baseline::rank(vec![
            worktree_candidate(&older, "worktree older", false, 600),
            worktree_candidate(&busy, "worktree busy", false, 1),
            worktree_candidate(&main, "the main checkout", true, 3600),
            worktree_candidate(&newer, "worktree newer", false, 60),
        ]);
        let order: Vec<&str> = candidates.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(
            order,
            [
                "the main checkout",
                "worktree busy",
                "worktree newer",
                "worktree older"
            ]
        );

        let held_main = fsutil::lock_profile(&main.join("debug"));
        let held_busy = fsutil::lock_profile(&busy.join("debug"));
        let target = tmp.path().join("wt/target");
        let started = Instant::now();
        let seeding = seed_best(
            &target,
            &candidates,
            Duration::from_millis(100),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "waited too long"
        );
        assert_eq!(seeding.tried, [0, 1, 2, 3]);
        if seeding.outcome == Outcome::Unsupported {
            return;
        }
        assert!(matches!(seeding.outcome, Outcome::Seeded(_)), "{seeding:?}");
        assert_eq!(seeding.from, Some(2));
        assert_eq!(fs::read(target.join("debug/deps/newer")).unwrap(), b"x");
        let line = describe(&seeding, &target, &candidates);
        assert!(
            line.contains(
                "(worktree newer, last used 1m ago; busy: the main checkout, worktree busy;"
            ),
            "{line}"
        );

        // Once main is idle, it's preferred again.
        drop(held_main);
        let other = tmp.path().join("wt2/target");
        let seeding = seed_best(&other, &candidates, LOCK_WAIT, LOCK_WAIT).unwrap();
        assert_eq!(seeding.from, Some(0), "{seeding:?}");
        drop(held_busy);
    }

    /// Nothing built anywhere, or only the target itself: a cold build.
    #[test]
    fn nothing_usable_means_a_cold_build() {
        let tmp = tempfile::tempdir().unwrap();
        let empty = tmp.path().join("empty/target");
        fs::create_dir_all(&empty).unwrap();
        let notes = tmp.path().join("notes");
        fs::create_dir_all(&notes).unwrap();
        fs::write(notes.join("readme"), "x").unwrap();
        let target = tmp.path().join("wt/target");
        let candidates = [
            worktree_candidate(&tmp.path().join("missing"), "the main checkout", true, 0),
            worktree_candidate(&empty, "worktree empty", false, 0),
            worktree_candidate(&notes, "worktree notes", false, 0),
        ];
        let seeding = seed_best(&target, &candidates, Duration::ZERO, Duration::ZERO).unwrap();
        assert_eq!(seeding.outcome, Outcome::NoBaseline);
        assert!(!target.exists());
        let seeding = seed_best(&target, &[], Duration::ZERO, Duration::ZERO).unwrap();
        assert_eq!(seeding.outcome, Outcome::NoBaseline);

        // The target is the only build there is.
        build_dir(&target);
        let me = [worktree_candidate(&target, "worktree wt", false, 0)];
        let seeding = seed_best(&target, &me, Duration::ZERO, Duration::ZERO).unwrap();
        assert_eq!(seeding.outcome, Outcome::Same);
    }

    /// A build directory made before cargo ran (as the proxy makes target
    /// directories) has neither of cargo's markers, only its profiles.
    #[test]
    fn a_build_dir_without_cargos_markers_is_a_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("routed/main");
        build_dir(&base);
        fs::remove_file(base.join(".rustc_info.json")).unwrap();
        fs::remove_file(base.join("CACHEDIR.TAG")).unwrap();
        assert!(!scan::is_cargo_root(&base));
        let target = tmp.path().join("routed/wt");
        let outcome = seed_dir(&target, &base, Duration::ZERO).unwrap();
        assert!(
            matches!(outcome, Outcome::Seeded(_) | Outcome::Unsupported),
            "{outcome:?}"
        );
    }

    /// Builds are using every candidate: after the wait, a cold build.
    #[test]
    fn every_candidate_busy_means_a_cold_build() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a/target");
        let b = tmp.path().join("b/target");
        build_dir(&a);
        build_dir(&b);
        let held = [
            fsutil::lock_profile(&a.join("debug")),
            fsutil::lock_profile(&b.join("debug")),
        ];
        let candidates = [
            worktree_candidate(&a, "the main checkout", true, 0),
            worktree_candidate(&b, "worktree b", false, 0),
        ];
        let target = tmp.path().join("wt/target");
        let started = Instant::now();
        let seeding = seed_best(
            &target,
            &candidates,
            Duration::from_millis(100),
            Duration::from_millis(400),
        )
        .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(400));
        assert_eq!(seeding.outcome, Outcome::Busy);
        assert_eq!(seeding.tried, [0, 1]);
        assert!(!target.exists());
        drop(held);
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
        let out = git_command()
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
        let main = |dir: &Path| {
            baseline::worktrees(dir)
                .into_iter()
                .find(|w| w.main)
                .map(|w| w.path)
        };
        assert_eq!(main(&wt), Some(repo.clone()));
        assert_eq!(main(&wt.join("crates/app")), Some(repo.clone()));
        assert_eq!(main(&repo), Some(repo.clone()));
        assert_eq!(baseline::worktrees(&wt).len(), 2);
    }

    #[test]
    fn describes_every_outcome() {
        let t = Path::new("/w/target");
        let given = [Candidate::given(
            Path::new("/m/target"),
            true,
            &ledger::Registry::default(),
        )];
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
                "/w/target is the build directory it would be seeded from; nothing to seed",
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
            let seeding = Seeding {
                outcome,
                from: Some(0),
                tried: vec![0],
            };
            assert_eq!(describe(&seeding, t, &given), line);
        }

        // Several candidates: which one, whose, and which were busy.
        let mut worktrees = vec![
            Candidate {
                label: "the main checkout".into(),
                ..given[0].clone()
            },
            Candidate {
                dir: "/a/target".into(),
                label: "worktree a".into(),
                main: false,
                ..given[0].clone()
            },
            Candidate {
                dir: "/b/target".into(),
                label: "worktree b".into(),
                main: false,
                ..given[0].clone()
            },
        ];
        let seeded = Seeding {
            outcome: Outcome::Seeded(Duration::from_millis(500)),
            from: Some(1),
            tried: vec![0, 1, 2],
        };
        assert_eq!(
            describe(&seeded, t, &worktrees),
            "Seeded /w/target from /a/target (worktree a; busy: the main checkout; 0.50 s, copy-on-write)"
        );
        let busy = Seeding {
            outcome: Outcome::Busy,
            from: None,
            tried: vec![0, 1, 2],
        };
        assert_eq!(
            describe(&busy, t, &worktrees),
            "Not seeding /w/target: builds are using /m/target, /a/target or /b/target"
        );
        let none = Seeding {
            outcome: Outcome::NoBaseline,
            from: None,
            tried: vec![],
        };
        assert_eq!(
            describe(&none, t, &worktrees[1..]),
            "No build to seed from at /a/target or /b/target"
        );
        worktrees.clear();
        assert_eq!(
            describe(&none, t, &worktrees),
            "No build to seed /w/target from: the repository has no other worktree"
        );
    }

    #[test]
    fn plans_explicit_dirs_and_nothing_without_a_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = ledger::Registry::default();
        let args = SeedArgs {
            target_dir: Some("/w/target".into()),
            from: vec!["/m/target".into(), "/n/target".into()],
            ..SeedArgs::default()
        };
        let job = plan(&args, tmp.path(), &registry).unwrap();
        assert_eq!(job.mine, Dirs::one(Path::new("/w/target")));
        let from: Vec<&Path> = job.candidates.iter().map(|c| c.dir.as_path()).collect();
        assert_eq!(from, [Path::new("/m/target"), Path::new("/n/target")]);
        assert!(job.candidates[0].main && !job.candidates[1].main);
        // Outside git there's no other worktree to clone from.
        let args = SeedArgs {
            target_dir: Some("/w/target".into()),
            ..SeedArgs::default()
        };
        let job = plan(&args, tmp.path(), &registry).unwrap();
        assert!(job.candidates.is_empty());
        assert!(baseline::worktrees(tmp.path()).is_empty());
        // Nor anything to seed outside a cargo workspace.
        assert!(plan(&SeedArgs::default(), tmp.path(), &registry).is_none());
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
        assert!(baseline::worktrees(&repo.join("sub")).is_empty());

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
        let found = baseline::worktrees(&wt);
        assert!(found.iter().all(|w| !w.main), "{found:?}");
        assert_eq!(found.len(), 1);

        // A bare repository has no main worktree.
        let bare = root.join("bare.git");
        fs::create_dir_all(&bare).unwrap();
        git_ok(&bare, &["init", "-q", "--bare"]);
        assert!(baseline::worktrees(&bare).is_empty());
    }

    #[test]
    fn plans_nothing_to_seed_from_in_a_lone_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().canonicalize().unwrap().join("repo");
        fs::create_dir_all(&repo).unwrap();
        git_ok(&repo, &["init", "-q", "-b", "main"]);
        let args = SeedArgs {
            target_dir: Some(repo.join("target")),
            ..SeedArgs::default()
        };
        let job = plan(&args, &repo, &ledger::Registry::default()).unwrap();
        assert!(job.candidates.is_empty(), "{job:?}");
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
        let dirs = cargo_dirs(&wt, None).expect("cargo metadata in the worktree");
        if dirs.build == dirs.target {
            // A Cargo too old to report build_directory.
            return;
        }
        let registry = ledger::Registry::default();
        let job = plan(&SeedArgs::default(), &wt, &registry).unwrap();
        assert_eq!(job.mine.build, wt.join("bdir"));
        assert_eq!(job.candidates.len(), 1, "{job:?}");
        assert_eq!(job.candidates[0].dir, repo.join("bdir"));
        assert!(job.candidates[0].main);
        // The target dirs, unless CARGO_TARGET_DIR puts both elsewhere.
        assert_eq!(job.mine.target, dirs.target);
        if dirs.target.starts_with(&wt) {
            assert_eq!(job.candidates[0].target, Some(repo.join("target")));
        }

        // A target dir given on the command line comes with the build-dir
        // cargo uses with it.
        let args = SeedArgs {
            target_dir: Some(root.join("routed/wt")),
            ..SeedArgs::default()
        };
        let job = plan(&args, &wt, &registry).unwrap();
        assert_eq!(job.mine.build, wt.join("bdir"));
        assert_eq!(job.mine.target, root.join("routed/wt"));
    }

    #[test]
    fn git_finds_its_repository_from_its_directory_alone() {
        // As in a hook or `git rebase --exec`, where git exports where its
        // own repository is.
        let command = git_command();
        let removed: Vec<&std::ffi::OsStr> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name)
            .collect();
        for var in GIT_REPO_VARS {
            assert!(removed.contains(&std::ffi::OsStr::new(var)), "{var}");
        }
        // And it works: in a fresh repository, git sees that one.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().canonicalize().unwrap();
        git_ok(&repo, &["init", "-q"]);
        assert_eq!(
            git(&repo, &["rev-parse", "--show-toplevel"]).map(PathBuf::from),
            Some(repo)
        );
    }
}
