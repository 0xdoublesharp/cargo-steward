//! Cleanup. Finds build directories, decides what no longer earns its space,
//! and removes it, cheapest-to-rebuild first:
//!
//! 1. stale codegen-unit objects (never used again);
//! 2. old-layout files of directories now built with the new layout;
//! 3. incremental caches and new-layout units no build has used lately;
//! 4. whole directories whose workspaces were all deleted, or that no build
//!    has used lately;
//! 5. when a disk runs low, whole directories, those whose bytes cost least
//!    to rebuild first.
//!
//! When applying, each profile directory is examined and cleaned while
//! holding cargo's own locks, so a running build is never touched and the
//! decision can't go stale between looking and deleting. Nothing a running
//! program was started from is removed ([`guard`]). Build directories are
//! worked on in parallel, each by one thread.

mod guard;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

pub use guard::Running;

use crate::config::{self, Paths, Policy};
use crate::fsutil::{self, Lock, human};
use crate::ledger::{self, Registry, RootInfo};
use crate::scan::{self, Dated, Profile};

/// A file a pinned build directory carries: only stale objects are removed
/// from it.
pub const PIN_FILE: &str = ".cargo-steward-keep";

/// How deep under a scan root build directories are searched for.
const SCAN_DEPTH: usize = 6;

/// Automatic cleanups truncate their log past this size.
const MAX_LOG: u64 = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    StaleObjects,
    OldSessions,
    LegacyLayout,
    IdleIncremental,
    IdleUnit,
    Orphaned,
    Idle,
    LowSpace,
    Requested,
}

impl Kind {
    pub fn describe(self) -> &'static str {
        match self {
            Kind::StaleObjects => "stale object files",
            Kind::OldSessions => "superseded incremental sessions",
            Kind::LegacyLayout => "old-layout build files",
            Kind::IdleIncremental => "idle incremental caches",
            Kind::IdleUnit => "idle build units",
            Kind::Orphaned => "build dirs of deleted workspaces",
            Kind::Idle => "idle build dirs",
            Kind::LowSpace => "build dirs removed for free space",
            Kind::Requested => "build dirs removed on request",
        }
    }
}

/// Something removed, or that would be.
#[derive(Debug, Clone)]
pub struct Action {
    pub kind: Kind,
    /// What was removed: a directory, or for stale objects the directory
    /// holding them.
    pub path: PathBuf,
    pub files: usize,
    pub bytes: u64,
    pub note: String,
}

/// Something cleanup would have removed but left in place, and why.
#[derive(Debug, Clone)]
pub struct Skipped {
    pub kind: Kind,
    pub path: PathBuf,
    pub reason: String,
}

/// What a cleanup removed (or would), and what it kept back.
#[derive(Debug, Default)]
pub struct Cleanup {
    pub actions: Vec<Action>,
    pub skipped: Vec<Skipped>,
}

impl Cleanup {
    fn absorb(&mut self, other: Cleanup) {
        self.actions.extend(other.actions);
        self.skipped.extend(other.skipped);
    }
}

/// A build directory and what is known about its use.
#[derive(Debug, Clone)]
pub struct BuildDir {
    pub root: PathBuf,
    pub profiles: Vec<Profile>,
    pub last_used: SystemTime,
    pub workspaces: BTreeSet<PathBuf>,
    pub pinned: bool,
    pub device: Option<u64>,
    /// Compile time the wrapper has recorded here, in milliseconds: a rough
    /// measure of what rebuilding it costs. 0 when none is recorded.
    pub compile_ms: u64,
}

impl BuildDir {
    fn inspect(root: &Path, info: Option<&RootInfo>) -> BuildDir {
        let profiles = scan::profiles(root);
        let mut last_used = info.map_or(UNIX_EPOCH, |i| {
            UNIX_EPOCH + Duration::from_secs(i.last_used)
        });
        for profile in &profiles {
            let times = scan::legacy_last_used(&profile.path)
                .into_iter()
                .chain(
                    scan::v2_units(&profile.path)
                        .into_iter()
                        .map(|u| u.last_used),
                )
                .chain(
                    scan::incremental_dirs(&profile.path)
                        .into_iter()
                        .map(|d| d.last_used),
                );
            for t in times {
                last_used = last_used.max(t);
            }
        }
        if last_used == UNIX_EPOCH {
            // Never built anything we can date: go by the directory itself.
            last_used = fs::metadata(root)
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);
        }
        BuildDir {
            root: root.to_path_buf(),
            profiles,
            last_used,
            workspaces: info.map(|i| i.workspaces.clone()).unwrap_or_default(),
            pinned: root.join(PIN_FILE).exists(),
            device: fsutil::device(root),
            compile_ms: info.map_or(0, |i| i.compile_ms),
        }
    }

    /// Every workspace that built here is gone. Directories never seen by the
    /// wrapper have no recorded workspaces and are never orphans.
    pub fn orphaned(&self) -> bool {
        !self.workspaces.is_empty() && self.workspaces.iter().all(|w| !w.exists())
    }

    fn idle_for(&self, now: SystemTime) -> Duration {
        now.duration_since(self.last_used).unwrap_or_default()
    }
}

/// Every known build directory: those the wrapper recorded and those found
/// under the scan roots. Registry entries are re-resolved to the build
/// directory's root (merging any recorded against a subdirectory), and
/// dropped once the directory is gone.
pub fn inventory(paths: &Paths, scan_roots: &[PathBuf]) -> Result<(Registry, Vec<BuildDir>)> {
    let compacted = ledger::compact(paths)?;
    let mut registry = Registry::default();
    for (root, info) in &compacted.roots {
        if let Some(root) = scan::find_root(root) {
            registry.roots.entry(root).or_default().absorb(info);
        }
    }
    if registry != compacted {
        ledger::save(paths, &registry)?;
    }
    let mut roots: Vec<PathBuf> = registry.roots.keys().cloned().collect();
    roots.extend(scan::discover(scan_roots, SCAN_DEPTH));
    let roots = scan::outermost(roots);
    let dirs = fsutil::parallel_map(&roots, |root| {
        BuildDir::inspect(root, registry.roots.get(root))
    });
    Ok((registry, dirs))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Remove things; otherwise only report.
    pub apply: bool,
    /// Started by the wrapper: quiet, low priority, one at a time.
    pub auto: bool,
    pub verbose: bool,
    /// The command that produced the report, for its closing hint.
    pub command: &'static str,
}

pub fn run(options: Options) -> Result<()> {
    let paths = Paths::resolve().context("cannot locate the home directory")?;
    fs::create_dir_all(&paths.home)
        .with_context(|| format!("creating {}", paths.home.display()))?;
    let config = config::load(&paths)?;
    let policy = config.policy()?;

    let _singleton = if options.apply {
        match fsutil::try_lock_file(&paths.gc_lock())? {
            Some(lock) => Some(lock),
            None => {
                if !options.auto {
                    eprintln!("another cleanup is already running");
                }
                return Ok(());
            }
        }
    } else {
        None
    };
    if options.apply {
        touch(&paths.stamp());
    }

    let (mut registry, dirs) = inventory(&paths, &config.scan_roots)?;
    let running = if config.process_guard {
        Running::collect()
    } else {
        Running::default()
    };
    let volumes = if options.apply {
        Volume::measure(&dirs)
    } else {
        Vec::new()
    };
    let cleanup = clean(
        &dirs,
        Sweep {
            policy: &policy,
            running: &running,
            now: SystemTime::now(),
            apply: options.apply,
        },
    );
    let freed: Vec<(PathBuf, i128)> = volumes
        .iter()
        .filter(|v| {
            cleanup.actions.iter().any(|a| {
                dirs.iter()
                    .any(|d| d.device == Some(v.device) && a.path.starts_with(&d.root))
            })
        })
        .filter_map(|v| Some((v.probe.clone(), v.freed()?)))
        .collect();

    for action in cleanup.actions.iter().filter(|a| a.kind >= Kind::Orphaned) {
        registry.roots.remove(&action.path);
    }
    if options.apply {
        ledger::save(&paths, &registry)?;
    }
    report(&cleanup, &freed, options);

    // Share identical files across what's left. Only where the filesystem
    // can tell files that already share blocks apart (APFS clone IDs), so
    // repeated cleanups don't re-read and re-clone the same duplicates.
    if options.apply && config.dedup && cfg!(target_os = "macos") {
        let removed: BTreeSet<&PathBuf> = cleanup
            .actions
            .iter()
            .filter(|a| a.kind >= Kind::Orphaned)
            .map(|a| &a.path)
            .collect();
        let remaining: Vec<BuildDir> = dirs
            .iter()
            .filter(|d| !removed.contains(&d.root))
            .cloned()
            .collect();
        match crate::dedup::dedup(&remaining, true) {
            Ok(summary) if summary.files > 0 => println!(
                "{:>12} {:>10}  identical files now shared across build dirs ({} files)",
                "Shared",
                human(summary.bytes),
                summary.files
            ),
            Ok(_) => {}
            Err(e) => eprintln!("warning: cargo-steward: dedup: {e:#}"),
        }
    }
    Ok(())
}

/// A volume holding build directories, and its free space before cleanup.
struct Volume {
    device: u64,
    /// A path on the volume that outlives the cleanup: the parent of one of
    /// its build directories.
    probe: PathBuf,
    before: u64,
}

impl Volume {
    fn measure(dirs: &[BuildDir]) -> Vec<Volume> {
        let mut volumes: Vec<Volume> = Vec::new();
        for dir in dirs {
            let Some(device) = dir.device else {
                continue;
            };
            if volumes.iter().any(|v| v.device == device) {
                continue;
            }
            let probe = dir
                .root
                .parent()
                .filter(|p| fsutil::device(p) == Some(device))
                .unwrap_or(&dir.root)
                .to_path_buf();
            if let Some(before) = fsutil::free_space(&probe) {
                volumes.push(Volume {
                    device,
                    probe,
                    before,
                });
            }
        }
        volumes
    }

    /// How much the volume's free space rose since [`Volume::measure`]. Other
    /// writes meanwhile move it too, so it can be negative.
    fn freed(&self) -> Option<i128> {
        let after = fsutil::free_space_near(&self.probe)?;
        Some(i128::from(after) - i128::from(self.before))
    }
}

/// How one cleanup works: its thresholds, the programs running when it
/// started, its notion of now, and whether it removes anything.
#[derive(Clone, Copy)]
pub struct Sweep<'a> {
    pub policy: &'a Policy,
    pub running: &'a Running,
    pub now: SystemTime,
    pub apply: bool,
}

/// Plans, and when applying, carries out the cleanup of `dirs`. Each build
/// directory is handled by one thread, several at a time.
pub fn clean(dirs: &[BuildDir], sweep: Sweep) -> Cleanup {
    // Before any thread renames something aside into the same parent.
    for dir in dirs {
        fsutil::sweep_trash(&dir.root);
        if let Some(parent) = dir.root.parent() {
            fsutil::sweep_trash(parent);
        }
    }
    let results = fsutil::parallel_map(dirs, |dir| clean_dir(dir, sweep));
    let mut cleanup = Cleanup::default();
    let mut removed: BTreeSet<PathBuf> = BTreeSet::new();
    for (dir, (result, whole)) in dirs.iter().zip(results) {
        if whole {
            removed.insert(dir.root.clone());
        }
        cleanup.absorb(result);
    }
    let low = free_space(dirs, &removed, &cleanup.actions, sweep);
    cleanup.absorb(low);
    cleanup
}

/// Cleans one build directory: removes it whole if it's orphaned or idle,
/// else cleans each profile. The flag says it was removed whole.
fn clean_dir(dir: &BuildDir, sweep: Sweep) -> (Cleanup, bool) {
    let mut out = Cleanup::default();
    if !dir.pinned {
        let idle = dir.idle_for(sweep.now);
        let whole = if dir.orphaned() && idle >= sweep.policy.orphan_grace {
            Some((
                Kind::Orphaned,
                format!(
                    "workspaces deleted, unused for {}",
                    fsutil::ago(dir.last_used)
                ),
            ))
        } else if idle >= sweep.policy.dir_idle {
            Some((
                Kind::Idle,
                format!("unused for {}", fsutil::ago(dir.last_used)),
            ))
        } else {
            None
        };
        if let Some((kind, note)) = whole {
            match remove_build_dir(dir, kind, note, None, sweep, &mut out) {
                Removal::Done => return (out, true),
                Removal::Busy | Removal::Failed => return (out, false),
                // A program runs from it: clean inside it instead.
                Removal::Guarded => {}
            }
        }
    }
    for profile in &dir.profiles {
        out.absorb(clean_profile(profile, dir.pinned, sweep));
    }
    (out, false)
}

fn clean_profile(profile: &Profile, pinned: bool, sweep: Sweep) -> Cleanup {
    let _lock = if sweep.apply {
        match fsutil::lock_profile(&profile.path) {
            Lock::Busy => return Cleanup::default(),
            held => Some(held),
        }
    } else {
        None
    };
    let mut out = Cleanup::default();
    let policy = sweep.policy;
    let now = sweep.now;
    let idle = |t: SystemTime, limit: Duration| now.duration_since(t).unwrap_or_default() >= limit;
    let keep_after = now.checked_sub(policy.object_grace).unwrap_or(UNIX_EPOCH);

    for dir in scan::object_dirs(profile) {
        let stale = scan::stale_objects_in(&dir, keep_after);
        if stale.is_empty() {
            continue;
        }
        let files: Vec<&PathBuf> = stale.iter().flat_map(|s| &s.files).collect();
        let mut gone = 0;
        if sweep.apply {
            gone = files.iter().filter(|f| fs::remove_file(f).is_ok()).count();
        }
        out.actions.push(Action {
            kind: Kind::StaleObjects,
            path: dir,
            files: if sweep.apply { gone } else { files.len() },
            bytes: stale.iter().map(|s| s.bytes).sum(),
            note: match stale.as_slice() {
                [one] => one.unit.clone(),
                many => format!("{} units", many.len()),
            },
        });
    }
    if pinned {
        return out;
    }

    // Old-layout files, once the new layout is in use here and no old-layout
    // build (an older pinned toolchain) has run for a while.
    if profile.legacy
        && profile.v2
        && scan::legacy_last_used(&profile.path).is_none_or(|t| idle(t, policy.legacy_idle))
    {
        for path in scan::legacy_paths(&profile.path) {
            remove_dir(
                Kind::LegacyLayout,
                &path,
                String::new(),
                None,
                sweep,
                &mut out,
            );
        }
    }
    for unit in scan::v2_units(&profile.path) {
        if idle(unit.last_used, policy.unit_idle) {
            remove_dated(Kind::IdleUnit, &unit, sweep, &mut out);
        }
    }
    let mut sessions: Vec<PathBuf> = Vec::new();
    for cache in scan::incremental_dirs(&profile.path) {
        if idle(cache.last_used, policy.incremental_idle) {
            remove_dated(Kind::IdleIncremental, &cache, sweep, &mut out);
        } else {
            sessions.extend(scan::superseded_sessions(&cache.path));
        }
    }
    if !sessions.is_empty() {
        // Each session directory, and the lock file next to it.
        let session_dirs: Vec<&PathBuf> = sessions.iter().filter(|p| p.is_dir()).collect();
        let count = session_dirs.len();
        let bytes: u64 = session_dirs.iter().map(|d| fsutil::tree_size(d)).sum();
        if sweep.apply {
            for path in &sessions {
                if let Err(e) = fsutil::remove(path) {
                    eprintln!(
                        "warning: cargo-steward: failed to remove {}: {e}",
                        path.display()
                    );
                }
            }
        }
        out.actions.push(Action {
            kind: Kind::OldSessions,
            path: profile.path.join("incremental"),
            files: 0,
            bytes,
            note: format!("{count} sessions"),
        });
    }
    out
}

/// How a removal went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Removal {
    Done,
    /// A running program was started from inside it.
    Guarded,
    /// A build holds its locks.
    Busy,
    Failed,
}

fn remove_dated(kind: Kind, item: &Dated, sweep: Sweep, out: &mut Cleanup) -> Removal {
    let note = format!("unused for {}", fsutil::ago(item.last_used));
    remove_dir(kind, &item.path, note, None, sweep, out)
}

/// Removes `path` (when applying) and records it, unless a running program
/// was started from inside it, which is recorded instead. `bytes` is its
/// size if already measured.
fn remove_dir(
    kind: Kind,
    path: &Path,
    note: String,
    bytes: Option<u64>,
    sweep: Sweep,
    out: &mut Cleanup,
) -> Removal {
    if let Some(process) = sweep.running.inside(path) {
        out.skipped.push(Skipped {
            kind,
            path: path.to_path_buf(),
            reason: format!(
                "{} is running from it (pid {})",
                process.exe.display(),
                process.pid
            ),
        });
        return Removal::Guarded;
    }
    let bytes = bytes.unwrap_or_else(|| fsutil::tree_size(path));
    if sweep.apply
        && let Err(e) = fsutil::remove(path)
    {
        eprintln!(
            "warning: cargo-steward: failed to remove {}: {e}",
            path.display()
        );
        return Removal::Failed;
    }
    out.actions.push(Action {
        kind,
        path: path.to_path_buf(),
        files: 0,
        bytes,
        note,
    });
    Removal::Done
}

/// Removes a whole build directory, holding every profile's locks while it is
/// renamed aside.
fn remove_build_dir(
    dir: &BuildDir,
    kind: Kind,
    note: String,
    bytes: Option<u64>,
    sweep: Sweep,
    out: &mut Cleanup,
) -> Removal {
    if !sweep.apply {
        return remove_dir(kind, &dir.root, note, bytes, sweep, out);
    }
    let mut locks = Vec::new();
    for profile in &dir.profiles {
        match fsutil::lock_profile(&profile.path) {
            Lock::Busy => return Removal::Busy,
            held => locks.push(held),
        }
    }
    let removal = remove_dir(kind, &dir.root, note, bytes, sweep, out);
    drop(locks);
    removal
}

/// Removes whole build directories on request (`cargo steward remove`), the
/// way cleanup removes them: while holding cargo's locks, not while a running
/// program was started from inside, and renamed aside first. Anything that
/// isn't a cargo build directory is refused, and pinned ones need `force`.
pub fn remove_requested(roots: &[PathBuf], apply: bool, force: bool) -> Result<()> {
    let paths = Paths::resolve().context("cannot locate the home directory")?;
    let config = config::load(&paths)?;
    let policy = config.policy()?;
    let running = if config.process_guard {
        Running::collect()
    } else {
        Running::default()
    };
    let mut registry = ledger::compact(&paths)?;
    let sweep = Sweep {
        policy: &policy,
        running: &running,
        now: SystemTime::now(),
        apply,
    };
    let mut out = Cleanup::default();
    let mut refused: Vec<(PathBuf, String)> = Vec::new();
    for root in roots {
        let root = match root.canonicalize() {
            Ok(root) => root,
            Err(err) => {
                refused.push((root.clone(), format!("it doesn't exist ({err})")));
                continue;
            }
        };
        if !scan::is_cargo_root(&root) {
            refused.push((
                root,
                "not the root of a cargo build directory (no .rustc_info.json, or cargo \
                 CACHEDIR.TAG with build profiles)"
                    .into(),
            ));
            continue;
        }
        let dir = BuildDir::inspect(&root, registry.roots.get(&root));
        if dir.pinned && !force {
            out.skipped.push(Skipped {
                kind: Kind::Requested,
                path: root,
                reason: "pinned (use --force to remove it anyway)".into(),
            });
            continue;
        }
        let note = format!("last used {} ago", fsutil::ago(dir.last_used));
        match remove_build_dir(&dir, Kind::Requested, note, None, sweep, &mut out) {
            Removal::Busy => out.skipped.push(Skipped {
                kind: Kind::Requested,
                path: root.clone(),
                reason: "a cargo build is using it; try again when it finishes".into(),
            }),
            Removal::Done if apply => {
                registry.roots.remove(&root);
            }
            _ => {}
        }
    }
    if apply {
        ledger::save(&paths, &registry)?;
    }
    for (path, reason) in &refused {
        eprintln!("refusing to remove {}: {reason}", path.display());
    }
    if refused.len() == roots.len() {
        anyhow::bail!("nothing to remove");
    }
    report(
        &out,
        &[],
        Options {
            apply,
            auto: false,
            verbose: true,
            command: "remove",
        },
    );
    Ok(())
}

/// The build directory a worktree builds into: where the `cargo` proxy routes
/// it, else what `cargo metadata` reports there (ignoring a target dir
/// exported for some other checkout).
pub fn build_dir_of(worktree: &Path) -> Result<PathBuf> {
    let worktree = worktree
        .canonicalize()
        .with_context(|| format!("{} doesn't exist", worktree.display()))?;
    let config = Paths::resolve()
        .and_then(|p| config::load(&p).ok())
        .unwrap_or_default();
    if let Ok(plan) = crate::proxy::Plan::new(&config, &worktree, &[])
        && plan.skip.is_none()
        && let Some(target) = plan.target
    {
        return Ok(target);
    }
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = std::process::Command::new(cargo)
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(&worktree)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .output()
        .context("running cargo metadata")?;
    if !output.status.success() {
        anyhow::bail!(
            "cargo metadata failed in {}: {}",
            worktree.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let meta: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let dir = meta["build_directory"]
        .as_str()
        .or_else(|| meta["target_directory"].as_str())
        .context("cargo metadata reported no target directory")?;
    Ok(PathBuf::from(dir))
}

/// Removes build directories from each disk that is below `min-free`, until
/// it has `target-free` again, those whose bytes cost least to rebuild first
/// ([`eviction_order`]).
fn free_space(
    dirs: &[BuildDir],
    removed: &BTreeSet<PathBuf>,
    done: &[Action],
    sweep: Sweep,
) -> Cleanup {
    let policy = sweep.policy;
    let mut by_device: BTreeMap<u64, Vec<&BuildDir>> = BTreeMap::new();
    for dir in dirs {
        if let Some(device) = dir.device
            && !removed.contains(&dir.root)
        {
            by_device.entry(device).or_default().push(dir);
        }
    }
    let mut out = Cleanup::default();
    for (_, mut candidates) in by_device {
        let Some(free) = fsutil::free_space(&candidates[0].root) else {
            continue;
        };
        // A dry run hasn't freed what it reported; assume it would have.
        let mut free = if sweep.apply {
            free
        } else {
            let device = candidates[0].device;
            free + done
                .iter()
                .filter(|a| fsutil::device(&a.path).is_none() || fsutil::device(&a.path) == device)
                .map(|a| a.bytes)
                .sum::<u64>()
        };
        if free >= policy.min_free {
            continue;
        }
        candidates.retain(|d| {
            !d.pinned
                && sweep.now.duration_since(d.last_used).unwrap_or_default() >= policy.recent_guard
        });
        let sizes = fsutil::parallel_map(&candidates, |d| fsutil::tree_size(&d.root));
        let ordered = eviction_order(candidates.into_iter().zip(sizes).collect(), sweep.now);
        for (dir, bytes) in ordered {
            if free >= policy.target_free {
                break;
            }
            let note = format!("unused for {}", fsutil::ago(dir.last_used));
            if remove_build_dir(dir, Kind::LowSpace, note, Some(bytes), sweep, &mut out)
                == Removal::Done
            {
                free = if sweep.apply {
                    dir.root
                        .parent()
                        .and_then(fsutil::free_space)
                        .unwrap_or(free + bytes)
                } else {
                    free + bytes
                };
            }
        }
    }
    out
}

/// Orders build directories, with their sizes, for removal when space runs
/// low: the bytes that cost least to rebuild and are least likely to be
/// needed go first. Each scores idle time × size ÷ (compile time + 1 s),
/// highest first, so a big directory nothing has used in weeks goes before
/// a small one used yesterday, and one that took an hour to build stays
/// longer than one as big that took a minute.
///
/// A directory without recorded compile time (the wrapper never saw it
/// build) is assumed to cost what the others do per byte (their median);
/// when none has any, size and idle time alone decide.
fn eviction_order(mut candidates: Vec<(&BuildDir, u64)>, now: SystemTime) -> Vec<(&BuildDir, u64)> {
    let mut rates: Vec<f64> = candidates
        .iter()
        .filter(|(d, bytes)| d.compile_ms > 0 && *bytes > 0)
        .map(|(d, bytes)| d.compile_ms as f64 / *bytes as f64)
        .collect();
    rates.sort_by(f64::total_cmp);
    let median_rate = match rates.len() {
        0 => None,
        n if n % 2 == 1 => Some(rates[n / 2]),
        n => Some((rates[n / 2 - 1] + rates[n / 2]) / 2.0),
    };
    let score = |dir: &BuildDir, bytes: u64| {
        let idle = now
            .duration_since(dir.last_used)
            .unwrap_or_default()
            .as_secs_f64();
        let compile_ms = if dir.compile_ms > 0 {
            dir.compile_ms as f64
        } else {
            median_rate.map_or(0.0, |rate| rate * bytes as f64)
        };
        idle * bytes as f64 / (compile_ms / 1000.0 + 1.0)
    };
    candidates.sort_by(|(a, a_bytes), (b, b_bytes)| {
        score(b, *b_bytes)
            .total_cmp(&score(a, *a_bytes))
            .then_with(|| a.last_used.cmp(&b.last_used))
            .then_with(|| a.root.cmp(&b.root))
    });
    candidates
}

fn report(cleanup: &Cleanup, freed: &[(PathBuf, i128)], options: Options) {
    let verb = if options.apply {
        "Removed"
    } else {
        "Would remove"
    };
    let stamp = humantime::format_rfc3339_seconds(SystemTime::now());
    let prefix = if options.auto {
        format!("{stamp} ")
    } else {
        String::new()
    };
    let actions = &cleanup.actions;
    let mut totals: BTreeMap<Kind, (usize, u64)> = BTreeMap::new();
    for action in actions {
        let total = totals.entry(action.kind).or_default();
        total.0 += 1;
        total.1 += action.bytes;
        if options.verbose || options.auto || action.kind >= Kind::Orphaned {
            let files = if action.files > 0 {
                format!("{} files, ", action.files)
            } else {
                String::new()
            };
            println!(
                "{prefix}{verb:>12} {:>10}  {}  ({files}{}) {}",
                human(action.bytes),
                action.kind.describe(),
                action.note,
                action.path.display()
            );
        }
    }
    // Always shown: they explain why something due for removal is still here.
    for skip in &cleanup.skipped {
        println!(
            "{prefix}{:>12} {:>10}  {}  ({}) {}",
            "Kept",
            "",
            skip.kind.describe(),
            skip.reason,
            skip.path.display()
        );
    }
    if options.auto {
        for (probe, delta) in freed {
            println!(
                "{prefix}{:>12} {:>10}  on the disk holding {}",
                "Freed",
                signed(*delta),
                probe.display()
            );
        }
        return;
    }
    if actions.is_empty() {
        println!("Nothing to clean up.");
        return;
    }
    println!();
    let mut sum = 0;
    for (kind, (count, bytes)) in &totals {
        sum += bytes;
        println!(
            "{verb:>12} {:>10}  {} ({count})",
            human(*bytes),
            kind.describe()
        );
    }
    println!("{:>12} {:>10}", "Total", human(sum));
    for (probe, delta) in freed {
        println!(
            "{:>12} {:>10}  Freed on disk: the change in free space on the disk holding {}",
            "",
            signed(*delta),
            probe.display()
        );
    }
    if options.apply {
        println!(
            "\nSizes count blocks shared with APFS or btrfs clones in full. \"Freed on disk\" is what\nthe disk's free space actually gained, which other writes during cleanup also move."
        );
    } else {
        println!(
            "\nSizes count blocks shared with APFS or btrfs clones in full; the disk's free space\nis what actually changes."
        );
        println!(
            "Run `cargo steward {} --apply` to remove these.",
            options.command
        );
    }
}

/// A size that may be negative: `-1.5 GiB`.
fn signed(bytes: i128) -> String {
    let magnitude = u64::try_from(bytes.unsigned_abs()).unwrap_or(u64::MAX);
    if bytes < 0 {
        format!("-{}", human(magnitude))
    } else {
        human(magnitude)
    }
}

/// Starts `cargo-steward gc --auto` detached from the build: its own session,
/// output to the log, so cargo never waits on it.
pub fn spawn_background(paths: &Paths) {
    touch(&paths.stamp());
    let Ok(exe) = env::current_exe() else {
        return;
    };
    let log = paths.log();
    if fs::metadata(&log).is_ok_and(|m| m.len() > MAX_LOG) {
        let _ = fs::remove_file(&log);
    }
    let mut command = Command::new(exe);
    command.args(["gc", "--auto"]).stdin(Stdio::null());
    match OpenOptions::new().create(true).append(true).open(&log) {
        Ok(file) => {
            let err = file.try_clone().map_or(Stdio::null(), Stdio::from);
            command.stdout(file).stderr(err);
        }
        Err(_) => {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    // Cargo's jobserver belongs to the build, not to the cleanup.
    for var in ["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS"] {
        command.env_remove(var);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and touches no Rust state.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let _ = command.spawn();
}

/// Lowers this process to background priority, and closes descriptors
/// inherited from the build (such as cargo's jobserver pipe).
pub fn become_background() {
    #[cfg(unix)]
    {
        for fd in 3..256 {
            // SAFETY: called first thing in an automatic cleanup, before this
            // process has opened anything, so no Rust object owns these.
            unsafe {
                libc::close(fd);
            }
        }
        #[cfg(target_os = "macos")]
        // SAFETY: plain syscall on this process; lowers CPU and I/O priority.
        unsafe {
            libc::setpriority(libc::PRIO_DARWIN_PROCESS, 0, libc::PRIO_DARWIN_BG);
        }
        #[cfg(not(target_os = "macos"))]
        // SAFETY: plain syscall on this process.
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        }
    }
}

fn touch(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
    {
        let _ = writeln!(file, "{}", ledger::now_secs());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, FileTimes};

    const DAY: Duration = Duration::from_secs(86_400);

    fn policy() -> Policy {
        config::Config::default().policy().unwrap()
    }

    /// Cleans `dirs` with the default settings and no running programs.
    fn run_clean(dirs: &[BuildDir], apply: bool) -> Vec<Action> {
        let policy = policy();
        let running = Running::default();
        clean(
            dirs,
            Sweep {
                policy: &policy,
                running: &running,
                now: SystemTime::now(),
                apply,
            },
        )
        .actions
    }

    fn make(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![7u8; 8192]).unwrap();
    }

    /// Backdates everything under `path`.
    fn age(path: &Path, by: Duration) {
        let when = SystemTime::now() - by;
        let times = FileTimes::new().set_accessed(when).set_modified(when);
        let mut stack = vec![path.to_path_buf()];
        let mut all = Vec::new();
        while let Some(p) = stack.pop() {
            if p.is_dir() {
                stack.extend(fsutil::entries(&p).map(|(_, c)| c));
            }
            all.push(p);
        }
        // Files first: setting a directory's time doesn't change its files'.
        for p in all.iter().rev().chain(all.iter()) {
            if let Ok(file) = File::open(p) {
                let _ = file.set_times(times);
            }
        }
    }

    fn build_dir(parent: &Path, name: &str) -> PathBuf {
        let root = parent.join(name);
        make(&root.join("CACHEDIR.TAG"));
        fs::write(
            root.join("CACHEDIR.TAG"),
            "Signature: x\n# created by cargo.\n",
        )
        .unwrap();
        root
    }

    #[test]
    fn removes_idle_units_and_caches_but_keeps_recent_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let root = build_dir(tmp.path(), "target");
        let debug = root.join("debug");
        make(&debug.join("build/old/0123456789abcdef/fingerprint/lib-old"));
        make(&debug.join("build/old/0123456789abcdef/out/libold.rlib"));
        make(&debug.join("build/new/fedcba9876543210/fingerprint/lib-new"));
        make(&debug.join("incremental/old-1a2b/s-abc/dep-graph.bin"));
        make(&debug.join("incremental/new-3c4d/s-def/dep-graph.bin"));
        age(&debug.join("build/old"), 30 * DAY);
        age(&debug.join("incremental/old-1a2b"), 30 * DAY);

        let dirs = vec![BuildDir::inspect(&root, None)];
        let actions = run_clean(&dirs, true);
        let kinds: Vec<Kind> = actions.iter().map(|a| a.kind).collect();
        assert_eq!(kinds, [Kind::IdleUnit, Kind::IdleIncremental]);
        assert!(!debug.join("build/old/0123456789abcdef").exists());
        assert!(debug.join("build/new/fedcba9876543210").exists());
        assert!(!debug.join("incremental/old-1a2b").exists());
        assert!(debug.join("incremental/new-3c4d").exists());
    }

    #[test]
    fn removes_old_layout_files_once_the_new_layout_takes_over() {
        let tmp = tempfile::tempdir().unwrap();
        let root = build_dir(tmp.path(), "target");
        let debug = root.join("debug");
        make(&debug.join(".fingerprint/app-0123456789abcdef/bin-app"));
        make(&debug.join("deps/app-0123456789abcdef"));
        make(&debug.join("build/serde-0123456789abcdef/build-script-build"));
        age(&debug.join(".fingerprint"), 5 * DAY);
        make(&debug.join("build/app/fedcba9876543210/fingerprint/bin-app"));
        make(&debug.join("app"));

        let dirs = vec![BuildDir::inspect(&root, None)];
        let actions = run_clean(&dirs, true);
        assert!(
            actions.iter().all(|a| a.kind == Kind::LegacyLayout),
            "{actions:?}"
        );
        assert_eq!(actions.len(), 3);
        assert!(!debug.join("deps").exists());
        assert!(!debug.join(".fingerprint").exists());
        assert!(!debug.join("build/serde-0123456789abcdef").exists());
        assert!(debug.join("build/app/fedcba9876543210").exists());
        assert!(debug.join("app").exists());
    }

    #[test]
    fn keeps_old_layout_files_an_older_toolchain_still_uses() {
        let tmp = tempfile::tempdir().unwrap();
        let root = build_dir(tmp.path(), "target");
        let debug = root.join("debug");
        make(&debug.join(".fingerprint/app-0123456789abcdef/bin-app"));
        make(&debug.join("deps/app-0123456789abcdef"));
        make(&debug.join("build/app/fedcba9876543210/fingerprint/bin-app"));
        let dirs = vec![BuildDir::inspect(&root, None)];
        assert!(run_clean(&dirs, true).is_empty());
        assert!(debug.join("deps").exists());
    }

    #[test]
    fn removes_orphaned_and_idle_build_dirs_but_not_pinned_or_busy_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let orphan = build_dir(tmp.path(), "orphan");
        make(&orphan.join("debug/.fingerprint/a-0123456789abcdef/lib-a"));
        let idle = build_dir(tmp.path(), "idle");
        make(&idle.join("debug/.fingerprint/a-0123456789abcdef/lib-a"));
        let pinned = build_dir(tmp.path(), "pinned");
        make(&pinned.join("debug/.fingerprint/a-0123456789abcdef/lib-a"));
        make(&pinned.join(PIN_FILE));
        let busy = build_dir(tmp.path(), "busy");
        make(&busy.join("debug/.fingerprint/a-0123456789abcdef/lib-a"));
        age(&orphan, 3 * DAY);
        age(&idle, 40 * DAY);
        age(&pinned, 40 * DAY);
        age(&busy, 40 * DAY);
        let held = fsutil::lock_profile(&busy.join("debug"));
        assert!(matches!(held, Lock::Held(_)));

        let info = RootInfo {
            last_used: ledger::now_secs() - 3 * 86_400,
            workspaces: BTreeSet::from([tmp.path().join("deleted-worktree")]),
            compile_ms: 0,
        };
        let dirs = vec![
            BuildDir::inspect(&orphan, Some(&info)),
            BuildDir::inspect(&idle, None),
            BuildDir::inspect(&pinned, None),
            BuildDir::inspect(&busy, None),
        ];
        let actions = run_clean(&dirs, true);
        let kinds: Vec<(Kind, PathBuf)> =
            actions.iter().map(|a| (a.kind, a.path.clone())).collect();
        assert_eq!(
            kinds,
            [(Kind::Orphaned, orphan.clone()), (Kind::Idle, idle.clone())]
        );
        assert!(!orphan.exists() && !idle.exists());
        assert!(pinned.exists() && busy.exists());
        assert_eq!(
            fsutil::entries(tmp.path())
                .filter(|(n, _)| n.starts_with(fsutil::TRASH_PREFIX))
                .count(),
            0
        );
    }

    #[test]
    fn removes_superseded_incremental_sessions_and_keeps_the_live_one() {
        let tmp = tempfile::tempdir().unwrap();
        let root = build_dir(tmp.path(), "target");
        let cache = root.join("debug/incremental/app-1a2b");
        make(&root.join("debug/.fingerprint/app-0123456789abcdef/bin-app"));
        make(&cache.join("s-h1aaaa-xyz1-3abc/dep-graph.bin"));
        make(&cache.join("s-h1aaaa-xyz1.lock"));
        make(&cache.join("s-h2bbbb-xyz2-4def/dep-graph.bin"));
        make(&cache.join("s-h2bbbb-xyz2.lock"));
        age(&cache.join("s-h1aaaa-xyz1-3abc"), Duration::from_secs(3600));

        let dirs = vec![BuildDir::inspect(&root, None)];
        let actions = run_clean(&dirs, true);
        let kinds: Vec<Kind> = actions.iter().map(|a| a.kind).collect();
        assert_eq!(kinds, [Kind::OldSessions]);
        assert!(!cache.join("s-h1aaaa-xyz1-3abc").exists());
        assert!(!cache.join("s-h1aaaa-xyz1.lock").exists());
        assert!(cache.join("s-h2bbbb-xyz2-4def/dep-graph.bin").exists());
        assert!(cache.join("s-h2bbbb-xyz2.lock").exists());
    }

    #[test]
    fn a_dry_run_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let idle = build_dir(tmp.path(), "idle");
        make(&idle.join("debug/.fingerprint/a-0123456789abcdef/lib-a"));
        age(&idle, 40 * DAY);
        let dirs = vec![BuildDir::inspect(&idle, None)];
        let actions = run_clean(&dirs, false);
        assert_eq!(actions.len(), 1);
        assert!(idle.exists());
    }

    fn sweep<'a>(policy: &'a Policy, running: &'a Running, apply: bool) -> Sweep<'a> {
        Sweep {
            policy,
            running,
            now: SystemTime::now(),
            apply,
        }
    }

    fn set_modified(path: &Path, age: Duration) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(SystemTime::now() - age))
            .unwrap();
    }

    #[test]
    fn keeps_stale_objects_within_the_object_grace() {
        let tmp = tempfile::tempdir().unwrap();
        let root = build_dir(tmp.path(), "target");
        let out = root.join("debug/build/app/0123456789abcdef/out");
        make(&root.join("debug/build/app/0123456789abcdef/fingerprint/bin-app"));
        let minute = Duration::from_secs(60);
        for (invocation, age) in [
            ("0hour", 60 * minute),
            ("0five", 5 * minute),
            ("0now", minute / 60),
        ] {
            let path = out.join(format!("app.x.{invocation}.rcgu.o"));
            make(&path);
            set_modified(&path, age);
        }
        let dirs = vec![BuildDir::inspect(&root, None)];
        let actions = run_clean(&dirs, true);
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].files, 1);
        assert!(!out.join("app.x.0hour.rcgu.o").exists());
        assert!(out.join("app.x.0five.rcgu.o").exists());

        // With no grace, only the newest compile's objects stay.
        let mut policy = policy();
        policy.object_grace = Duration::ZERO;
        let running = Running::default();
        let cleanup = clean(&dirs, sweep(&policy, &running, true));
        assert_eq!(cleanup.actions.len(), 1);
        assert!(!out.join("app.x.0five.rcgu.o").exists());
        assert!(out.join("app.x.0now.rcgu.o").exists());
    }

    #[test]
    fn keeps_units_caches_and_old_layout_files_programs_run_from() {
        let tmp = tempfile::tempdir().unwrap();
        let root = build_dir(tmp.path(), "target");
        let debug = root.join("debug");
        make(&debug.join(".fingerprint/app-0123456789abcdef/bin-app"));
        make(&debug.join("deps/app-0123456789abcdef"));
        age(&debug.join(".fingerprint"), 5 * DAY);
        make(&debug.join("build/old/0123456789abcdef/fingerprint/lib-old"));
        make(&debug.join("build/old/0123456789abcdef/out/old-tool"));
        make(&debug.join("build/gone/fedcba9876543210/fingerprint/lib-gone"));
        age(&debug.join("build/old"), 30 * DAY);
        age(&debug.join("build/gone"), 30 * DAY);
        make(&debug.join("incremental/old-1a2b/s-abc/dep-graph.bin"));
        age(&debug.join("incremental"), 30 * DAY);

        let running = Running::of(vec![
            guard::Process {
                pid: 7,
                exe: debug.join("deps/app-0123456789abcdef"),
            },
            guard::Process {
                pid: 8,
                exe: debug.join("build/old/0123456789abcdef/out/old-tool"),
            },
        ]);
        let dirs = vec![BuildDir::inspect(&root, None)];
        let policy = policy();
        let cleanup = clean(&dirs, sweep(&policy, &running, true));
        let mut kept: Vec<(Kind, PathBuf)> = cleanup
            .skipped
            .iter()
            .map(|s| (s.kind, s.path.clone()))
            .collect();
        kept.sort();
        assert_eq!(
            kept,
            [
                (Kind::LegacyLayout, debug.join("deps")),
                (Kind::IdleUnit, debug.join("build/old/0123456789abcdef")),
            ]
        );
        assert!(debug.join("deps").exists());
        assert!(debug.join("build/old/0123456789abcdef").exists());
        // What no program runs from goes as before.
        assert!(!debug.join(".fingerprint").exists());
        assert!(!debug.join("build/gone/fedcba9876543210").exists());
        assert!(!debug.join("incremental/old-1a2b").exists());
    }

    fn dated(root: &str, idle_days: u32, compile_ms: u64) -> BuildDir {
        BuildDir {
            root: PathBuf::from(root),
            profiles: Vec::new(),
            last_used: SystemTime::now() - idle_days * DAY,
            workspaces: BTreeSet::new(),
            pinned: false,
            device: None,
            compile_ms,
        }
    }

    fn order(candidates: &[(BuildDir, u64)]) -> Vec<&str> {
        let refs = candidates.iter().map(|(d, b)| (d, *b)).collect();
        eviction_order(refs, SystemTime::now())
            .into_iter()
            .map(|(d, _)| d.root.to_str().unwrap())
            .collect()
    }

    #[test]
    fn evicts_the_cheapest_bytes_to_rebuild_first() {
        const GIB: u64 = 1 << 30;
        let minute = 60_000;
        // Alike but for idle time: least recently used first.
        assert_eq!(
            order(&[
                (dated("/recent", 2, 10 * minute), 10 * GIB),
                (dated("/old", 9, 10 * minute), 10 * GIB),
            ]),
            ["/old", "/recent"]
        );
        // Alike but for size: the bigger one frees more per rebuild.
        assert_eq!(
            order(&[
                (dated("/small", 5, 10 * minute), GIB),
                (dated("/big", 5, 10 * minute), 40 * GIB),
            ]),
            ["/big", "/small"]
        );
        // An hour of compiling outweighs being a little older.
        assert_eq!(
            order(&[
                (dated("/costly", 6, 60 * minute), 10 * GIB),
                (dated("/cheap", 4, minute), 10 * GIB),
            ]),
            ["/cheap", "/costly"]
        );
        // Without any compile times, idle time × size decides.
        assert_eq!(
            order(&[
                (dated("/a", 3, 0), 10 * GIB),
                (dated("/b", 10, 0), 2 * GIB),
                (dated("/c", 4, 0), 10 * GIB),
            ]),
            ["/c", "/a", "/b"]
        );
    }

    #[test]
    fn a_dir_without_compile_times_costs_what_others_do_per_byte() {
        const GIB: u64 = 1 << 30;
        let minute = 60_000;
        // Without the estimate the unknown one would score as free to
        // rebuild and go first; at the others' rate it sits between them.
        assert_eq!(
            order(&[
                (dated("/known-cheap", 5, minute), 10 * GIB),
                (dated("/unknown", 5, 0), 10 * GIB),
                (dated("/known-costly", 5, 30 * minute), 10 * GIB),
            ]),
            ["/known-cheap", "/unknown", "/known-costly"]
        );
    }

    #[test]
    fn low_space_eviction_keeps_pins_recent_dirs_and_running_programs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut dirs = Vec::new();
        let mut info = BTreeMap::new();
        for (name, idle_days, compile_ms) in [
            ("costly", 5, 3_600_000),
            ("cheap", 3, 1_000),
            ("running", 6, 1_000),
            ("recent", 0, 1_000),
            ("pinned", 8, 1_000),
        ] {
            let root = build_dir(tmp.path(), name);
            make(&root.join("debug/.fingerprint/a-0123456789abcdef/lib-a"));
            make(&root.join("debug/deps/liba-0123456789abcdef.rlib"));
            if name == "pinned" {
                make(&root.join(PIN_FILE));
            }
            if idle_days > 0 {
                age(&root, idle_days * DAY);
            }
            info.insert(
                name,
                RootInfo {
                    last_used: ledger::now_secs() - u64::from(idle_days) * 86_400,
                    workspaces: BTreeSet::new(),
                    compile_ms,
                },
            );
            dirs.push(BuildDir::inspect(&root, info.get(name)));
        }
        let mut policy = policy();
        policy.min_free = u64::MAX / 2;
        policy.target_free = u64::MAX / 2;
        let running = Running::of(vec![guard::Process {
            pid: 9,
            exe: tmp.path().join("running/debug/app"),
        }]);
        let cleanup = clean(&dirs, sweep(&policy, &running, false));
        let removed: Vec<(Kind, PathBuf)> = cleanup
            .actions
            .iter()
            .map(|a| (a.kind, a.path.clone()))
            .collect();
        assert_eq!(
            removed,
            [
                (Kind::LowSpace, tmp.path().join("cheap")),
                (Kind::LowSpace, tmp.path().join("costly")),
            ]
        );
        let kept: Vec<(Kind, PathBuf)> = cleanup
            .skipped
            .iter()
            .map(|s| (s.kind, s.path.clone()))
            .collect();
        assert_eq!(kept, [(Kind::LowSpace, tmp.path().join("running"))]);
    }
}
