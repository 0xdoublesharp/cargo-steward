//! Cleanup. Finds build directories, decides what no longer earns its space,
//! and removes it, cheapest-to-rebuild first:
//!
//! 1. stale codegen-unit objects (never used again);
//! 2. old-layout files of directories now built with the new layout;
//! 3. incremental caches and new-layout units no build has used lately;
//! 4. whole directories whose workspaces were all deleted, or that no build
//!    has used lately;
//! 5. when a disk runs low, whole directories, least recently used first.
//!
//! When applying, each profile directory is examined and cleaned while
//! holding cargo's own locks, so a running build is never touched and the
//! decision can't go stale between looking and deleting.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

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
    LegacyLayout,
    IdleIncremental,
    IdleUnit,
    Orphaned,
    Idle,
    LowSpace,
}

impl Kind {
    pub fn describe(self) -> &'static str {
        match self {
            Kind::StaleObjects => "stale object files",
            Kind::LegacyLayout => "old-layout build files",
            Kind::IdleIncremental => "idle incremental caches",
            Kind::IdleUnit => "idle build units",
            Kind::Orphaned => "build dirs of deleted workspaces",
            Kind::Idle => "idle build dirs",
            Kind::LowSpace => "build dirs removed for free space",
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

/// A build directory and what is known about its use.
#[derive(Debug, Clone)]
pub struct BuildDir {
    pub root: PathBuf,
    pub profiles: Vec<Profile>,
    pub last_used: SystemTime,
    pub workspaces: BTreeSet<PathBuf>,
    pub pinned: bool,
    pub device: Option<u64>,
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
    let dirs = roots
        .iter()
        .map(|root| BuildDir::inspect(root, registry.roots.get(root)))
        .collect();
    Ok((registry, dirs))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Remove things; otherwise only report.
    pub apply: bool,
    /// Started by the wrapper: quiet, low priority, one at a time.
    pub auto: bool,
    pub verbose: bool,
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
    let now = SystemTime::now();
    let actions = clean(&dirs, &policy, now, options.apply);

    for action in actions.iter().filter(|a| a.kind >= Kind::Orphaned) {
        registry.roots.remove(&action.path);
    }
    if options.apply {
        ledger::save(&paths, &registry)?;
    }
    report(&actions, options);
    Ok(())
}

/// Plans, and when `apply`, carries out the cleanup of `dirs`.
pub fn clean(dirs: &[BuildDir], policy: &Policy, now: SystemTime, apply: bool) -> Vec<Action> {
    let mut actions = Vec::new();
    let mut removed: BTreeSet<PathBuf> = BTreeSet::new();

    for dir in dirs {
        fsutil::sweep_trash(&dir.root);
        if let Some(parent) = dir.root.parent() {
            fsutil::sweep_trash(parent);
        }
        if !dir.pinned {
            let idle = dir.idle_for(now);
            let whole = if dir.orphaned() && idle >= policy.orphan_grace {
                Some((
                    Kind::Orphaned,
                    format!(
                        "workspaces deleted, unused for {}",
                        fsutil::ago(dir.last_used)
                    ),
                ))
            } else if idle >= policy.dir_idle {
                Some((
                    Kind::Idle,
                    format!("unused for {}", fsutil::ago(dir.last_used)),
                ))
            } else {
                None
            };
            if let Some((kind, note)) = whole {
                if let Some(action) = remove_build_dir(dir, kind, note, apply) {
                    removed.insert(dir.root.clone());
                    actions.push(action);
                }
                continue;
            }
        }
        for profile in &dir.profiles {
            actions.extend(clean_profile(profile, dir.pinned, policy, now, apply));
        }
    }

    actions.extend(free_space(dirs, &removed, &actions, policy, now, apply));
    actions
}

fn clean_profile(
    profile: &Profile,
    pinned: bool,
    policy: &Policy,
    now: SystemTime,
    apply: bool,
) -> Vec<Action> {
    let _lock = if apply {
        match fsutil::lock_profile(&profile.path) {
            Lock::Busy => return Vec::new(),
            held => Some(held),
        }
    } else {
        None
    };
    let mut actions = Vec::new();
    let idle = |t: SystemTime, limit: Duration| now.duration_since(t).unwrap_or_default() >= limit;

    for dir in scan::object_dirs(profile) {
        let stale = scan::stale_objects_in(&dir);
        if stale.is_empty() {
            continue;
        }
        let files: Vec<&PathBuf> = stale.iter().flat_map(|s| &s.files).collect();
        let mut gone = 0;
        if apply {
            gone = files.iter().filter(|f| fs::remove_file(f).is_ok()).count();
        }
        actions.push(Action {
            kind: Kind::StaleObjects,
            path: dir,
            files: if apply { gone } else { files.len() },
            bytes: stale.iter().map(|s| s.bytes).sum(),
            note: match stale.as_slice() {
                [one] => one.unit.clone(),
                many => format!("{} units", many.len()),
            },
        });
    }
    if pinned {
        return actions;
    }

    // Old-layout files, once the new layout is in use here and no old-layout
    // build (an older pinned toolchain) has run for a while.
    if profile.legacy
        && profile.v2
        && scan::legacy_last_used(&profile.path).is_none_or(|t| idle(t, policy.legacy_idle))
    {
        for path in scan::legacy_paths(&profile.path) {
            actions.extend(remove_dir(Kind::LegacyLayout, &path, String::new(), apply));
        }
    }
    for unit in scan::v2_units(&profile.path) {
        if idle(unit.last_used, policy.unit_idle) {
            actions.extend(remove_dated(Kind::IdleUnit, &unit, apply));
        }
    }
    for cache in scan::incremental_dirs(&profile.path) {
        if idle(cache.last_used, policy.incremental_idle) {
            actions.extend(remove_dated(Kind::IdleIncremental, &cache, apply));
        }
    }
    actions
}

fn remove_dated(kind: Kind, item: &Dated, apply: bool) -> Option<Action> {
    remove_dir(
        kind,
        &item.path,
        format!("unused for {}", fsutil::ago(item.last_used)),
        apply,
    )
}

fn remove_dir(kind: Kind, path: &Path, note: String, apply: bool) -> Option<Action> {
    let bytes = fsutil::tree_size(path);
    if apply && let Err(e) = fsutil::remove(path) {
        eprintln!(
            "warning: cargo-steward: failed to remove {}: {e}",
            path.display()
        );
        return None;
    }
    Some(Action {
        kind,
        path: path.to_path_buf(),
        files: 0,
        bytes,
        note,
    })
}

/// Removes a whole build directory, holding every profile's locks while it is
/// renamed aside.
fn remove_build_dir(dir: &BuildDir, kind: Kind, note: String, apply: bool) -> Option<Action> {
    if !apply {
        return remove_dir(kind, &dir.root, note, false);
    }
    let mut locks = Vec::new();
    for profile in &dir.profiles {
        match fsutil::lock_profile(&profile.path) {
            Lock::Busy => return None,
            held => locks.push(held),
        }
    }
    let action = remove_dir(kind, &dir.root, note, true);
    drop(locks);
    action
}

/// Removes least recently used build directories from each disk that is
/// below `min-free`, until it has `target-free` again.
fn free_space(
    dirs: &[BuildDir],
    removed: &BTreeSet<PathBuf>,
    done: &[Action],
    policy: &Policy,
    now: SystemTime,
    apply: bool,
) -> Vec<Action> {
    let mut by_device: BTreeMap<u64, Vec<&BuildDir>> = BTreeMap::new();
    for dir in dirs {
        if let Some(device) = dir.device
            && !removed.contains(&dir.root)
        {
            by_device.entry(device).or_default().push(dir);
        }
    }
    let mut actions = Vec::new();
    for (_, mut candidates) in by_device {
        let Some(free) = fsutil::free_space(&candidates[0].root) else {
            continue;
        };
        // A dry run hasn't freed what it reported; assume it would have.
        let mut free = if apply {
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
            !d.pinned && now.duration_since(d.last_used).unwrap_or_default() >= policy.recent_guard
        });
        candidates.sort_by_key(|d| d.last_used);
        for dir in candidates {
            if free >= policy.target_free {
                break;
            }
            let note = format!("unused for {}", fsutil::ago(dir.last_used));
            if let Some(action) = remove_build_dir(dir, Kind::LowSpace, note, apply) {
                free = if apply {
                    dir.root
                        .parent()
                        .and_then(fsutil::free_space)
                        .unwrap_or(free + action.bytes)
                } else {
                    free + action.bytes
                };
                actions.push(action);
            }
        }
    }
    actions
}

fn report(actions: &[Action], options: Options) {
    let verb = if options.apply {
        "Removed"
    } else {
        "Would remove"
    };
    let stamp = humantime::format_rfc3339_seconds(SystemTime::now());
    if actions.is_empty() {
        if !options.auto {
            println!("Nothing to clean up.");
        }
        return;
    }
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
            let prefix = if options.auto {
                format!("{stamp} ")
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
    if options.auto {
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
    println!(
        "\nSizes count blocks shared with APFS or btrfs clones in full; the disk's free space\nis what actually changes."
    );
    if !options.apply {
        println!("Run `cargo steward gc --apply` to remove these.");
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
        let actions = clean(&dirs, &policy(), SystemTime::now(), true);
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
        let actions = clean(&dirs, &policy(), SystemTime::now(), true);
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
        assert!(clean(&dirs, &policy(), SystemTime::now(), true).is_empty());
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
        let actions = clean(&dirs, &policy(), SystemTime::now(), true);
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
    fn a_dry_run_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let idle = build_dir(tmp.path(), "idle");
        make(&idle.join("debug/.fingerprint/a-0123456789abcdef/lib-a"));
        age(&idle, 40 * DAY);
        let dirs = vec![BuildDir::inspect(&idle, None)];
        let actions = clean(&dirs, &policy(), SystemTime::now(), false);
        assert_eq!(actions.len(), 1);
        assert!(idle.exists());
    }
}
