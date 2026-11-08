//! `cargo steward ci trim`: leave the build directory holding what the job
//! used, within a size budget.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use super::marker::{self, Marker};
use super::units::{self, Kind, Unit};
use crate::fsutil::{self, Lock, human};
use crate::scan::{self, Profile};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Eviction {
    Lru,
    #[default]
    RebuildCost,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub root: PathBuf,
    /// Package names of the workspace members.
    pub members: BTreeSet<String>,
    pub budget: Option<u64>,
    pub keep_workspace: bool,
    pub keep_incremental: bool,
    pub keep_packages: BTreeSet<String>,
    pub eviction: Eviction,
    pub dry_run: bool,
    pub verbose: bool,
    /// `$GITHUB_STEP_SUMMARY`: a markdown file shown on the job's page.
    pub step_summary: Option<PathBuf>,
}

/// How `trim` tells what the job used.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Tracking {
    /// Fingerprints read or written since the start belong to used units.
    Reads { start: SystemTime },
    /// The filesystem doesn't record reads: budget only.
    Frozen,
    /// `ci start` didn't run here: budget only.
    NoMarker,
}

impl Tracking {
    fn from_marker(marker: Option<Marker>) -> Tracking {
        match marker {
            Some(m) if m.atime => Tracking::Reads {
                start: m.start_time(),
            },
            Some(_) => Tracking::Frozen,
            None => Tracking::NoMarker,
        }
    }

    fn describe(self) -> String {
        match self {
            Tracking::Reads { start } => format!(
                "kept what the job used since {}",
                humantime::format_rfc3339_seconds(start)
            ),
            Tracking::Frozen => {
                "this filesystem doesn't record reads (noatime), so only the budget applied".into()
            }
            Tracking::NoMarker => {
                "`ci start` left no mark here (it didn't run, or the job started \
                                   without a build directory), so only the budget applied"
                    .into()
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Reason {
    Unused,
    Workspace,
    Incremental,
    StaleObjects,
    Budget,
}

impl Reason {
    fn label(self, budget: Option<u64>) -> String {
        match self {
            Reason::Unused => "not used in this job".into(),
            Reason::Workspace => "workspace members".into(),
            Reason::Incremental => "incremental caches".into(),
            Reason::StaleObjects => "stale object files".into(),
            Reason::Budget => format!("over the {} budget", human(budget.unwrap_or_default())),
        }
    }
}

/// What a trim removed (or would), by reason, and what it kept.
#[derive(Debug, Default)]
struct Report {
    /// `(reason, items, bytes)`, in the order they happened.
    removed: Vec<(Reason, usize, u64)>,
    kept_units: usize,
    kept_caches: usize,
    kept_bytes: u64,
    before: u64,
    after: u64,
    /// What's left that trim doesn't remove, whatever the budget: profiles a
    /// build holds, `doc/`, final outputs cargo copies up, its own files.
    outside: u64,
}

impl Report {
    fn add(&mut self, reason: Reason, items: usize, bytes: u64) {
        match self.removed.iter_mut().find(|r| r.0 == reason) {
            Some(row) => {
                row.1 += items;
                row.2 += bytes;
            }
            None => self.removed.push((reason, items, bytes)),
        }
    }

    fn removed_bytes(&self) -> u64 {
        self.removed.iter().map(|r| r.2).sum()
    }
}

pub fn run(options: &Options) -> Result<()> {
    let root = &options.root;
    if !root.is_dir() {
        println!(
            "cargo-steward: nothing to trim, {} doesn't exist",
            root.display()
        );
        return Ok(());
    }
    let tracking = Tracking::from_marker(marker::read(root)?);
    let report = trim(options, tracking);
    if !options.dry_run {
        marker::remove(root)
            .with_context(|| format!("removing the marker in {}", root.display()))?;
    }

    print!("{}", render_text(options, tracking, &report));
    if let Some(path) = &options.step_summary {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| f.write_all(render_markdown(options, tracking, &report).as_bytes()))
            .with_context(|| format!("writing the step summary {}", path.display()))?;
    }
    Ok(())
}

/// Removes units and tallies them.
struct Remover {
    apply: bool,
    verbose: bool,
    report: Report,
    /// Paths removed (or that would be, in a dry run).
    gone: Vec<PathBuf>,
}

impl Remover {
    fn unit(&mut self, unit: Unit, bytes: u64, reason: Reason) {
        if self.verbose {
            let what = unit.paths.first().map(|p| p.display().to_string());
            self.log(
                bytes,
                &format!("{} {}", unit.noun(), what.unwrap_or_default()),
            );
        }
        if self.apply {
            for path in &unit.paths {
                if let Err(e) = fsutil::remove(path) {
                    eprintln!(
                        "warning: cargo-steward: failed to remove {}: {e}",
                        path.display()
                    );
                }
            }
        }
        self.report.add(reason, 1, bytes);
        self.gone.extend(unit.paths);
    }

    fn is_gone(&self, path: &Path) -> bool {
        self.gone.iter().any(|g| path.starts_with(g))
    }

    fn log(&self, bytes: u64, what: &str) {
        let verb = if self.apply {
            "Removed"
        } else {
            "Would remove"
        };
        println!("{verb:>12} {:>10}  {what}", human(bytes));
    }
}

fn trim(options: &Options, tracking: Tracking) -> Report {
    let root = &options.root;
    let apply = !options.dry_run;
    let mut out = Remover {
        apply,
        verbose: options.verbose,
        report: Report::default(),
        gone: Vec::new(),
    };

    // Cargo's locks on every profile, held to the end so the budget can
    // weigh units across all of them.
    let mut locks = Vec::new();
    let mut profiles: Vec<Profile> = Vec::new();
    for profile in scan::profiles(root) {
        if apply {
            match fsutil::lock_profile(&profile.path) {
                Lock::Busy => {
                    eprintln!(
                        "warning: cargo-steward: a build is running in {}; left as is",
                        profile.path.display()
                    );
                    continue;
                }
                held => locks.push(held),
            }
            sweep(&profile.path);
        }
        profiles.push(profile);
    }
    out.report.before = fsutil::tree_size(root);

    let members = &options.members;
    let mut member_crates: BTreeSet<String> = members.iter().map(|m| m.replace('-', "_")).collect();
    let mut selected_crates: BTreeSet<String> = options
        .keep_packages
        .iter()
        .map(|p| p.replace('-', "_"))
        .collect();
    let used_since = match tracking {
        Tracking::Reads { start } => Some(start),
        Tracking::Frozen | Tracking::NoMarker => None,
    };

    // Build units: the workspace's own, and those no build in the job used.
    let mut kept: Vec<Unit> = Vec::new();
    for unit in profiles.iter().flat_map(|p| {
        let mut units = units::legacy_units(&p.path);
        units.extend(units::v2_units(&p.path));
        units
    }) {
        // The package name need not be its [lib] or binary crate name.
        // Keep ownership before removing the unit and its dep-info files.
        if members.contains(&unit.package) {
            member_crates.extend(unit.crates.iter().cloned());
        }
        if options.keep_packages.contains(&unit.package) {
            selected_crates.extend(unit.crates.iter().cloned());
        }
        let reason = if !options.keep_workspace
            && !options.keep_packages.contains(&unit.package)
            && members.contains(&unit.package)
        {
            Some(Reason::Workspace)
        } else if let Some(start) = used_since
            // Outputs without a fingerprint can't be dated reliably; leave
            // them to the budget.
            && unit.fingerprint.is_some()
            && unit.last_used < start
        {
            Some(Reason::Unused)
        } else {
            None
        };
        match reason {
            Some(reason) => {
                let bytes = unit.bytes();
                out.unit(unit, bytes, reason);
            }
            None => kept.push(unit),
        }
    }

    // Incremental caches, which belong with their crate's units.
    let kept_crates: BTreeSet<&String> = kept.iter().flat_map(|u| &u.crates).collect();
    let mut kept_caches: Vec<Unit> = Vec::new();
    for cache in profiles
        .iter()
        .flat_map(|p| units::incremental_units(&p.path))
    {
        let reason = if !options.keep_incremental {
            Some(Reason::Incremental)
        } else if !options.keep_workspace
            && !selected_crates.contains(&cache.package)
            && member_crates.contains(&cache.package)
        {
            Some(Reason::Workspace)
        } else if used_since.is_some() && !kept_crates.contains(&cache.package) {
            Some(Reason::Unused)
        } else {
            None
        };
        match reason {
            Some(reason) => {
                let bytes = cache.bytes();
                out.unit(cache, bytes, reason);
            }
            None => kept_caches.push(cache),
        }
    }

    // Object files that later compiles replaced, in what's left.
    for profile in &profiles {
        for dir in scan::object_dirs(profile) {
            if out.is_gone(&dir) {
                continue;
            }
            // The job is over: nothing it built is still running and
            // needs old objects for backtraces, so no grace applies.
            let no_grace = SystemTime::now() + Duration::from_secs(86_400);
            let files: Vec<PathBuf> = scan::stale_objects_in(&dir, no_grace, None)
                .into_iter()
                .flat_map(|s| s.files)
                .filter(|f| !out.is_gone(f))
                .collect();
            if files.is_empty() {
                continue;
            }
            let bytes = fsutil::freed_by(&files);
            let mut count = files.len();
            if apply {
                count = files.iter().filter(|f| fs::remove_file(f).is_ok()).count();
            }
            if options.verbose {
                out.log(
                    bytes,
                    &format!("{count} stale object files in {}", dir.display()),
                );
            }
            out.report.add(Reason::StaleObjects, count, bytes);
        }
    }

    // Then the least recently used, until under the budget.
    let mut remaining: Vec<(Unit, u64)> = kept
        .into_iter()
        .chain(kept_caches)
        .map(|u| {
            let bytes = u.bytes();
            (u, bytes)
        })
        .collect();
    // The budget weighs only what trim could remove: evicting units can't
    // make up for bytes it leaves alone, such as a busy profile or `doc/`.
    // A dry run hasn't removed the stale objects counted in units' sizes.
    let stale_pending = if apply {
        0
    } else {
        out.report
            .removed
            .iter()
            .filter(|r| r.0 == Reason::StaleObjects)
            .map(|r| r.2)
            .sum()
    };
    let removable = remaining
        .iter()
        .map(|(_, b)| b)
        .sum::<u64>()
        .saturating_sub(stale_pending);
    if let Some(budget) = options.budget
        && removable > budget
    {
        let items: Vec<_> = remaining
            .iter()
            .map(|(u, b)| (u.last_used, *b, u.rebuild_ms()))
            .collect();
        let dependents = if options.eviction == Eviction::RebuildCost && items.len() <= 4096 {
            units::dependents(&remaining.iter().map(|(u, _)| u).collect::<Vec<_>>())
        } else {
            Vec::new()
        };
        let chosen: BTreeSet<usize> =
            evict_linked(&items, removable - budget, options.eviction, &dependents)
                .into_iter()
                .collect();
        let mut stay = Vec::new();
        for (i, (unit, bytes)) in remaining.into_iter().enumerate() {
            if chosen.contains(&i) {
                out.unit(unit, bytes, Reason::Budget);
            } else {
                stay.push((unit, bytes));
            }
        }
        remaining = stay;
    }

    // Kept fingerprints, and those in profiles a build is using, get back the
    // access times they had.
    if apply
        && used_since.is_some()
        && let Err(e) = marker::restore_all(root)
    {
        eprintln!(
            "warning: cargo-steward: failed to restore access times in {}: {e}",
            root.display()
        );
    }

    let mut report = out.report;
    for (unit, bytes) in &remaining {
        match unit.kind {
            Kind::Incremental => report.kept_caches += 1,
            Kind::Legacy | Kind::V2 => report.kept_units += 1,
        }
        report.kept_bytes += bytes;
    }
    report.after = if apply {
        fsutil::tree_size(root)
    } else {
        report.before.saturating_sub(report.removed_bytes())
    };
    report.outside = report.after.saturating_sub(report.kept_bytes);
    drop(locks);
    report
}

/// Indices of the items to remove, least recently used first, so that
/// removing them frees at least `excess` bytes (or all of them, if together
/// they come to less).
fn over_budget(items: &[(SystemTime, u64)], excess: u64) -> Vec<usize> {
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by_key(|&i| items[i].0);
    let mut freed = 0;
    let mut chosen = Vec::new();
    for i in order {
        if freed >= excess {
            break;
        }
        freed += items[i].1;
        chosen.push(i);
    }
    chosen
}

/// Prefer cheap-to-reconstruct bytes. Unknown units receive the median
/// observed cost density, avoiding a bias against unmeasured packages.
/// With no observations this is exactly LRU. Ratios use widened integers.
fn evict(items: &[(SystemTime, u64, Option<u64>)], excess: u64, policy: Eviction) -> Vec<usize> {
    let mut known: Vec<_> = items
        .iter()
        .filter_map(|(_, bytes, ms)| Some((ms.filter(|&n| n > 0)?, (*bytes).max(1))))
        .collect();
    if policy == Eviction::Lru || known.is_empty() {
        return over_budget(
            &items.iter().map(|(t, b, _)| (*t, *b)).collect::<Vec<_>>(),
            excess,
        );
    }
    let compare = |a: (u64, u64), b: (u64, u64)| {
        (u128::from(a.0) * u128::from(b.1)).cmp(&(u128::from(b.0) * u128::from(a.1)))
    };
    known.sort_by(|a, b| compare(*a, *b));
    let fallback = known[known.len() / 2];
    let value = |i: usize| {
        items[i]
            .2
            .filter(|&n| n > 0)
            .map(|n| (n, items[i].1.max(1)))
            .unwrap_or(fallback)
    };
    let mut order: Vec<_> = (0..items.len()).collect();
    order.sort_by(|&a, &b| compare(value(a), value(b)).then(items[a].0.cmp(&items[b].0)));
    let mut freed = 0u64;
    order
        .into_iter()
        .take_while(|&i| {
            if freed >= excess {
                return false;
            }
            freed = freed.saturating_add(items[i].1);
            true
        })
        .collect()
}

/// Charge an eviction for all compiler work it invalidates, including
/// retained dependents. Each invalidated unit is charged only once.
fn evict_linked(
    items: &[(SystemTime, u64, Option<u64>)],
    excess: u64,
    policy: Eviction,
    dependents: &[Vec<usize>],
) -> Vec<usize> {
    if policy == Eviction::Lru
        || dependents.is_empty()
        || items.iter().all(|(_, _, ms)| ms.is_none())
    {
        return evict(items, excess, policy);
    }
    let mut densities: Vec<_> = items
        .iter()
        .filter_map(|(_, b, c)| c.map(|c| (c, (*b).max(1))))
        .collect();
    densities.sort_by(|a, b| {
        (u128::from(a.0) * u128::from(b.1)).cmp(&(u128::from(b.0) * u128::from(a.1)))
    });
    let fallback = densities[densities.len() / 2];
    let costs: Vec<u64> = items
        .iter()
        .map(|(_, b, c)| {
            c.unwrap_or_else(|| {
                (u128::from(fallback.0) * u128::from(*b) / u128::from(fallback.1))
                    .min(u128::from(u64::MAX)) as u64
            })
        })
        .collect();
    let mut affected = vec![Vec::new(); items.len()];
    let closures: Vec<Vec<usize>> = (0..items.len())
        .map(|i| {
            let mut visited = BTreeSet::new();
            let mut stack = vec![i];
            while let Some(j) = stack.pop() {
                if j < items.len() && visited.insert(j) {
                    stack.extend(dependents.get(j).into_iter().flatten().copied());
                }
            }
            for &j in &visited {
                affected[j].push(i);
            }
            visited.into_iter().collect()
        })
        .collect();
    let mut damage: Vec<u128> = closures
        .iter()
        .map(|c| c.iter().map(|&i| u128::from(costs[i])).sum())
        .collect();
    let mut invalidated = vec![false; items.len()];
    let mut removed = vec![false; items.len()];
    let (mut freed, mut chosen) = (0u64, Vec::new());
    while freed < excess {
        let Some(i) = (0..items.len()).filter(|&i| !removed[i]).min_by(|&a, &b| {
            // Saturation matters only for corrupt, implausibly large hints.
            damage[a]
                .saturating_mul(u128::from(items[b].1.max(1)))
                .cmp(&damage[b].saturating_mul(u128::from(items[a].1.max(1))))
                .then(items[a].0.cmp(&items[b].0))
        }) else {
            break;
        };
        removed[i] = true;
        chosen.push(i);
        freed = freed.saturating_add(items[i].1);
        for &j in &closures[i] {
            if !invalidated[j] {
                invalidated[j] = true;
                for &candidate in &affected[j] {
                    damage[candidate] = damage[candidate].saturating_sub(u128::from(costs[j]));
                }
            }
        }
    }
    chosen
}

/// Finishes deletions that an interrupted trim left renamed aside.
fn sweep(profile: &Path) {
    for dir in [".fingerprint", "deps", "examples", "build", "incremental"] {
        fsutil::sweep_trash(&profile.join(dir));
    }
    for package in fsutil::subdirs(&profile.join("build")) {
        fsutil::sweep_trash(&package);
    }
}

fn kept_label(report: &Report) -> String {
    let count =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    format!(
        "{}, {}",
        count(report.kept_units, "unit", "units"),
        count(
            report.kept_caches,
            "incremental cache",
            "incremental caches"
        )
    )
}

fn render_text(options: &Options, tracking: Tracking, report: &Report) -> String {
    let verb = if options.dry_run {
        "Would remove"
    } else {
        "Removed"
    };
    let mut s = String::new();
    let _ = writeln!(
        s,
        "cargo-steward ci trim: {} ({})",
        options.root.display(),
        tracking.describe()
    );
    for (reason, items, bytes) in &report.removed {
        let _ = writeln!(
            s,
            "{verb:>12} {:>10}  {items} {}",
            human(*bytes),
            reason.label(options.budget)
        );
    }
    if report.removed.is_empty() {
        let _ = writeln!(s, "{:>12} {:>10}", "Removed", "nothing");
    }
    let _ = writeln!(
        s,
        "{:>12} {:>10}  {}",
        "Kept",
        human(report.kept_bytes),
        kept_label(report)
    );
    let _ = write!(
        s,
        "{:>12} {:>10}  (was {})",
        "Size",
        human(report.after),
        human(report.before)
    );
    if let Some(budget) = options.budget {
        let _ = write!(s, ", budget {}", human(budget));
        let _ = write!(s, ", eviction {:?}", options.eviction);
    }
    s.push('\n');
    if let Some(note) = budget_unmet(options, report) {
        let _ = writeln!(
            s,
            "{:>12} {:>10}  {note}",
            "Can't remove",
            human(report.outside)
        );
    }
    s
}

/// Why the build directory is still over the budget, when it is: what's
/// left of it that trim doesn't remove.
fn budget_unmet(options: &Options, report: &Report) -> Option<&'static str> {
    let budget = options.budget?;
    (report.after > budget).then_some(
        "outside what trim removes (profiles a build is using, doc/, final outputs), so \
         the build directory stays over the budget",
    )
}

fn render_markdown(options: &Options, tracking: Tracking, report: &Report) -> String {
    let verb = if options.dry_run {
        "Would remove"
    } else {
        "Removed"
    };
    let mut s = String::new();
    let _ = writeln!(s, "### cargo-steward ci trim\n");
    let _ = writeln!(
        s,
        "`{}`: {}.\n",
        options.root.display(),
        tracking.describe()
    );
    let _ = writeln!(s, "| | Items | Size |\n|---|--:|--:|");
    let _ = writeln!(
        s,
        "| Kept | {} | {} |",
        kept_label(report),
        human(report.kept_bytes)
    );
    for (reason, items, bytes) in &report.removed {
        let _ = writeln!(
            s,
            "| {verb}: {} | {items} | {} |",
            reason.label(options.budget),
            human(*bytes)
        );
    }
    let budget = options
        .budget
        .map(|b| format!(" (budget {})", human(b)))
        .unwrap_or_default();
    if let Some(note) = budget_unmet(options, report) {
        let _ = writeln!(s, "| Can't remove: {note} | | {} |", human(report.outside));
    }
    let _ = writeln!(
        s,
        "| **Build directory**{budget} | was {} | **{}** |\n",
        human(report.before),
        human(report.after)
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn a_budget_keeps_expensive_units_instead_of_merely_recent_ones() {
        let (_tmp, root) = fixture();
        let deps = root.join("debug/deps");
        for (name, hash, ms, age) in [
            ("used", "1111111111111111", 1000, 10),
            ("unused", "2222222222222222", 1, 20),
        ] {
            crate::costs::record(
                &deps.join(format!(".cargo-steward-{hash}.json")),
                name,
                &deps,
                Duration::from_millis(ms),
                Duration::ZERO,
                None,
                Some(ms),
            );
            let fingerprint = root.join(format!("debug/.fingerprint/{name}-{hash}/lib-{name}"));
            fs::File::open(&fingerprint)
                .unwrap()
                .set_times(
                    fs::FileTimes::new()
                        .set_accessed(at(age))
                        .set_modified(at(age)),
                )
                .unwrap();
            fs::File::open(fingerprint.parent().unwrap())
                .unwrap()
                .set_times(
                    fs::FileTimes::new()
                        .set_accessed(at(age))
                        .set_modified(at(age)),
                )
                .unwrap();
        }
        let budget = units::legacy_units(&root.join("debug"))
            .into_iter()
            .find(|u| u.package == "used")
            .unwrap()
            .bytes();
        let options = Options {
            budget: Some(budget),
            ..opts(&root)
        };
        trim(&options, Tracking::NoMarker);
        assert!(
            deps.join("libused-1111111111111111.rlib").exists(),
            "expensive old unit was evicted"
        );
        assert!(
            !deps.join("libunused-2222222222222222.rlib").exists(),
            "cheap recent unit was kept"
        );
    }

    #[test]
    fn compiler_cpu_cost_is_preferred_to_contended_wall_time() {
        let (_tmp, root) = fixture();
        let deps = root.join("debug/deps");
        let file = deps.join(".cargo-steward-1111111111111111.json");
        fs::write(
            &file,
            serde_json::to_vec(&serde_json::json!({
                "version":1,"output_dir":deps,"max_execution_ms":9000,
                "cpu_samples_ms":[100,110,90],"execution_samples_ms":[9000]
            }))
            .unwrap(),
        )
        .unwrap();
        let unit = units::legacy_units(&root.join("debug"))
            .into_iter()
            .find(|u| u.package == "used")
            .unwrap();
        assert_eq!(unit.rebuild_ms(), Some(100));
    }

    #[test]
    fn eviction_falls_back_to_lru_and_supports_an_explicit_lru_override() {
        let items = [(at(10), 100, Some(1000)), (at(20), 100, Some(1))];
        assert_eq!(evict(&items, 100, Eviction::Lru), [0]);
        assert_eq!(evict(&items, 100, Eviction::RebuildCost), [1]);
        let unknown = [(at(10), 100, None), (at(20), 100, None)];
        assert_eq!(evict(&unknown, 100, Eviction::RebuildCost), [0]);
    }

    #[test]
    fn eviction_accounts_for_rebuilds_caused_in_retained_dependents() {
        // A tiny, cheap dependency is required by a costly retained unit.
        // Throwing it away also invalidates that retained compiler work.
        let items = [
            (at(10), 10, Some(1)),
            (at(20), 100, Some(100)),
            (at(30), 10, Some(5)),
        ];
        let dependents = vec![vec![1], vec![], vec![]];
        assert_eq!(
            evict_linked(&items, 10, Eviction::RebuildCost, &dependents),
            [2]
        );
    }

    #[test]
    fn selected_workspace_packages_can_be_retained() {
        let (_tmp, root) = fixture();
        let options = Options {
            keep_packages: BTreeSet::from(["app".into()]),
            ..opts(&root)
        };
        trim(&options, Tracking::NoMarker);
        assert!(root.join("debug/deps/app-3333333333333333").exists());
    }

    #[test]
    fn custom_library_names_follow_their_workspace_packages_incremental_policy() {
        for keep in [false, true] {
            let (_tmp, root) = fixture();
            fs::write(root.join("debug/deps/custom_lib-3333333333333333.d"), "").unwrap();
            let cache = root.join("debug/incremental/custom_lib-1abc");
            fs::create_dir_all(cache.join("s-1")).unwrap();
            fs::write(cache.join("s-1/dep-graph.bin"), "x").unwrap();
            trim(
                &Options {
                    keep_incremental: true,
                    keep_packages: if keep {
                        BTreeSet::from(["app".into()])
                    } else {
                        BTreeSet::new()
                    },
                    ..opts(&root)
                },
                Tracking::NoMarker,
            );
            assert_eq!(
                cache.exists(),
                keep,
                "custom crate ignored its package's cache policy"
            );
        }
    }

    #[test]
    fn budget_removes_least_recently_used_first_until_under() {
        let items = [(at(30), 100), (at(10), 50), (at(20), 70), (at(40), 10)];
        assert_eq!(over_budget(&items, 0), Vec::<usize>::new());
        assert_eq!(over_budget(&items, 50), [1]);
        assert_eq!(over_budget(&items, 51), [1, 2]);
        assert_eq!(over_budget(&items, 121), [1, 2, 0]);
        // More than everything: all of it, oldest first.
        assert_eq!(over_budget(&items, 1000), [1, 2, 0, 3]);
        assert!(over_budget(&[], 10).is_empty());
    }

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("target");
        populate(&root.join("debug"));
        fs::write(root.join(".rustc_info.json"), "{}").unwrap();
        (tmp, root)
    }

    /// Three units (`used`, `unused`, the member `app`) and two incremental
    /// caches, of 8 KiB files: 64 KiB in all.
    fn populate(profile: &Path) {
        let used = "1111111111111111";
        let unused = "2222222222222222";
        let member = "3333333333333333";
        for path in [
            format!(".fingerprint/used-{used}/lib-used"),
            format!("deps/libused-{used}.rlib"),
            format!(".fingerprint/unused-{unused}/lib-unused"),
            format!("deps/libunused-{unused}.rlib"),
            format!(".fingerprint/app-{member}/bin-app"),
            format!("deps/app-{member}"),
            "incremental/used-1abc/s-1/dep-graph.bin".into(),
            "incremental/unused-2abc/s-1/dep-graph.bin".into(),
        ] {
            let path = profile.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, vec![1u8; 8192]).unwrap();
        }
    }

    fn opts(root: &Path) -> Options {
        Options {
            root: root.to_path_buf(),
            members: BTreeSet::from(["app".to_owned()]),
            budget: None,
            keep_workspace: false,
            keep_incremental: false,
            keep_packages: BTreeSet::new(),
            eviction: Eviction::default(),
            dry_run: false,
            verbose: false,
            step_summary: None,
        }
    }

    fn fingerprint_count(root: &Path) -> usize {
        fsutil::entries(&root.join("debug/.fingerprint")).count()
    }

    #[test]
    fn removes_units_the_job_did_not_read() {
        let (_tmp, root) = fixture();
        let profile = root.join("debug");
        marker::write(
            &root,
            &Marker {
                start: marker::now_secs(),
                atime: true,
            },
        )
        .unwrap();
        let fingerprint = |name: &str| {
            fsutil::subdirs(&profile.join(".fingerprint"))
                .find(|d| d.file_name().unwrap().to_string_lossy().starts_with(name))
                .unwrap()
        };
        // As `ci start` leaves them, then the job reads `used`'s fingerprint.
        let old = at(1_700_000_000);
        for dir in fsutil::subdirs(&profile.join(".fingerprint")) {
            for (_, file) in fsutil::entries(&dir) {
                fs::File::open(&file)
                    .unwrap()
                    .set_times(
                        fs::FileTimes::new()
                            .set_accessed(UNIX_EPOCH)
                            .set_modified(old),
                    )
                    .unwrap();
            }
            fs::File::open(&dir)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        let read = fingerprint("used").join("lib-used");
        fs::File::open(&read)
            .unwrap()
            .set_times(fs::FileTimes::new().set_accessed(SystemTime::now()))
            .unwrap();

        let dry = opts(&root);
        let dry = Options {
            dry_run: true,
            keep_incremental: true,
            ..dry
        };
        let report = trim(&dry, Tracking::from_marker(marker::read(&root).unwrap()));
        assert_eq!(fingerprint_count(&root), 3, "a dry run removes nothing");
        assert_eq!(
            report
                .removed
                .iter()
                .map(|r| (r.0, r.1))
                .collect::<Vec<_>>(),
            [(Reason::Unused, 2), (Reason::Workspace, 1)]
        );
        assert!(report.after < report.before);

        let summary = root.join("summary.md");
        let options = Options {
            keep_incremental: true,
            step_summary: Some(summary.clone()),
            ..opts(&root)
        };
        run(&options).unwrap();
        assert!(fingerprint("used").exists());
        assert!(profile.join("deps/libused-1111111111111111.rlib").exists());
        assert!(profile.join("incremental/used-1abc").exists());
        assert_eq!(fingerprint_count(&root), 1);
        assert!(
            !profile
                .join("deps/libunused-2222222222222222.rlib")
                .exists()
        );
        assert!(!profile.join("deps/app-3333333333333333").exists());
        assert!(!profile.join("incremental/unused-2abc").exists());
        // The kept unit's unread files have their access times back, and the
        // marker is gone.
        let meta = fs::metadata(&read).unwrap();
        assert!(meta.accessed().unwrap() > UNIX_EPOCH + Duration::from_secs(86_400));
        assert_eq!(marker::read(&root).unwrap(), None);
        let markdown = fs::read_to_string(&summary).unwrap();
        assert!(
            markdown.contains("| Removed: not used in this job | 2 |"),
            "{markdown}"
        );
        assert!(
            markdown.contains("| Kept | 1 unit, 1 incremental cache |"),
            "{markdown}"
        );
    }

    #[test]
    fn without_a_marker_only_the_budget_applies() {
        let (_tmp, root) = fixture();
        let options = Options {
            members: BTreeSet::new(),
            keep_incremental: true,
            budget: Some(1),
            ..opts(&root)
        };
        let report = trim(&options, Tracking::NoMarker);
        assert_eq!(
            report
                .removed
                .iter()
                .map(|r| (r.0, r.1))
                .collect::<Vec<_>>(),
            [(Reason::Budget, 5)]
        );
        assert_eq!(fingerprint_count(&root), 0);
        assert!(fsutil::entries(&root.join("debug/deps")).next().is_none());

        // Under budget: nothing at all.
        let (_tmp, root) = fixture();
        let options = Options {
            budget: Some(1 << 30),
            keep_workspace: true,
            keep_incremental: true,
            ..opts(&root)
        };
        let report = trim(&options, Tracking::Frozen);
        assert!(report.removed.is_empty(), "{report:?}");
        assert_eq!(report.kept_units, 3);
        assert_eq!(report.kept_caches, 2);
    }

    #[test]
    fn a_missing_build_directory_is_nothing_to_trim() {
        let tmp = tempfile::tempdir().unwrap();
        let options = opts(&tmp.path().join("absent"));
        run(&options).unwrap();
        assert!(!tmp.path().join("absent").exists());
    }

    #[test]
    fn a_noatime_marker_leaves_only_the_budget() {
        let tracking = Tracking::from_marker(Some(Marker {
            start: 1_700_000_000,
            atime: false,
        }));
        assert_eq!(tracking, Tracking::Frozen);
        assert!(tracking.describe().contains("noatime"));
        assert!(
            Tracking::NoMarker
                .describe()
                .contains("`ci start` left no mark")
        );
        let reads = Tracking::from_marker(Some(Marker {
            start: 1_700_000_000,
            atime: true,
        }));
        assert!(
            reads
                .describe()
                .starts_with("kept what the job used since 2023-11-14")
        );
        assert_eq!(
            Reason::Budget.label(Some(1 << 20)),
            "over the 1.0 MiB budget"
        );
    }

    #[test]
    fn a_member_cache_kept_with_incremental_still_goes_with_the_workspace() {
        let (_tmp, root) = fixture();
        let options = Options {
            members: BTreeSet::from(["used".to_owned()]),
            keep_incremental: true,
            dry_run: true,
            verbose: true,
            ..opts(&root)
        };
        let report = trim(&options, Tracking::NoMarker);
        // `used`'s unit and its incremental cache, as workspace members.
        assert_eq!(
            report
                .removed
                .iter()
                .map(|r| (r.0, r.1))
                .collect::<Vec<_>>(),
            [(Reason::Workspace, 2)]
        );
        assert!(
            root.join("debug/incremental/used-1abc").exists(),
            "a dry run"
        );
        let text = render_text(&options, Tracking::NoMarker, &report);
        assert!(text.contains("Would remove"), "{text}");
    }

    #[test]
    fn removes_objects_later_compiles_replaced() {
        let (_tmp, root) = fixture();
        let deps = root.join("debug/deps");
        let old = deps.join("used-1111111111111111.x.0aaa.rcgu.o");
        let new = deps.join("used-1111111111111111.x.0bbb.rcgu.o");
        fs::write(&old, vec![0u8; 4096]).unwrap();
        fs::write(&new, vec![0u8; 4096]).unwrap();
        fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(at(1_700_000_000))
            .unwrap();
        let options = Options {
            keep_workspace: true,
            keep_incremental: true,
            verbose: true,
            ..opts(&root)
        };
        let report = trim(&options, Tracking::NoMarker);
        assert_eq!(
            report
                .removed
                .iter()
                .map(|r| (r.0, r.1))
                .collect::<Vec<_>>(),
            [(Reason::StaleObjects, 1)]
        );
        assert!(!old.exists());
        assert!(new.exists());
        assert_eq!(report.kept_units, 3);
    }

    #[test]
    fn a_dry_run_counts_stale_objects_without_removing_them() {
        let (_tmp, root) = fixture();
        let deps = root.join("debug/deps");
        let old = deps.join("used-1111111111111111.x.0aaa.rcgu.o");
        fs::write(&old, vec![0u8; 4096]).unwrap();
        fs::write(
            deps.join("used-1111111111111111.x.0bbb.rcgu.o"),
            vec![0u8; 4096],
        )
        .unwrap();
        fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(at(1_700_000_000))
            .unwrap();
        let options = Options {
            keep_workspace: true,
            keep_incremental: true,
            dry_run: true,
            ..opts(&root)
        };
        let report = trim(&options, Tracking::NoMarker);
        assert_eq!(
            report
                .removed
                .iter()
                .map(|r| (r.0, r.1))
                .collect::<Vec<_>>(),
            [(Reason::StaleObjects, 1)]
        );
        assert!(old.exists(), "a dry run");
        let text = render_text(&options, Tracking::NoMarker, &report);
        assert!(text.contains("1 stale object files"), "{text}");
    }

    #[test]
    fn the_budget_keeps_the_most_recently_used() {
        let (_tmp, root) = fixture();
        let all = Options {
            keep_workspace: true,
            keep_incremental: true,
            dry_run: true,
            ..opts(&root)
        };
        // What trim could remove, all kept; one byte over the budget: only
        // the least recently used item goes.
        let removable = trim(&all, Tracking::NoMarker).kept_bytes;
        let options = Options {
            budget: Some(removable - 1),
            ..all
        };
        let report = trim(&options, Tracking::NoMarker);
        assert_eq!(
            report
                .removed
                .iter()
                .map(|r| (r.0, r.1))
                .collect::<Vec<_>>(),
            [(Reason::Budget, 1)]
        );
        assert_eq!(report.kept_units + report.kept_caches, 4, "{report:?}");
    }

    #[test]
    fn a_unit_that_cant_be_removed_is_a_warning() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, root) = fixture();
        let deps = root.join("debug/deps");
        fs::set_permissions(&deps, fs::Permissions::from_mode(0o555)).unwrap();
        let options = Options {
            budget: Some(1),
            keep_workspace: true,
            keep_incremental: true,
            ..opts(&root)
        };
        let report = trim(&options, Tracking::NoMarker);
        fs::set_permissions(&deps, fs::Permissions::from_mode(0o755)).unwrap();
        // Counted as trimmed, but what couldn't go is still there and the
        // trim carried on with the rest.
        assert_eq!(
            report
                .removed
                .iter()
                .map(|r| (r.0, r.1))
                .collect::<Vec<_>>(),
            [(Reason::Budget, 5)]
        );
        assert_eq!(fingerprint_count(&root), 0);
        assert!(deps.join("libunused-2222222222222222.rlib").exists());
    }

    #[test]
    fn leaves_a_profile_a_build_is_using() {
        let (_tmp, root) = fixture();
        let lock = fsutil::try_lock_file(&root.join("debug/.cargo-lock"))
            .unwrap()
            .unwrap();
        let options = Options {
            budget: Some(1),
            ..opts(&root)
        };
        let report = trim(&options, Tracking::NoMarker);
        assert!(report.removed.is_empty(), "{report:?}");
        assert_eq!(fingerprint_count(&root), 3);
        drop(lock);
    }

    #[test]
    fn the_budget_weighs_only_what_trim_can_remove() {
        let removed = |report: &Report| {
            report
                .removed
                .iter()
                .map(|r| (r.0, r.1))
                .collect::<Vec<_>>()
        };
        let options = |root: &Path, budget: u64| Options {
            members: BTreeSet::new(),
            budget: Some(budget),
            keep_incremental: true,
            ..opts(root)
        };

        // A busy `release` as big as the idle `debug`. Each case starts
        // afresh: a process another test forks can briefly share the locks
        // a trim took.
        let busy_release = || {
            let (tmp, root) = fixture();
            populate(&root.join("release"));
            let lock = fsutil::try_lock_file(&root.join("release/.cargo-lock"))
                .unwrap()
                .unwrap();
            (tmp, root, lock)
        };

        // A budget that holds `debug` keeps all of it, though the directory
        // is over.
        let (_tmp, root, lock) = busy_release();
        let report = trim(&options(&root, 72 << 10), Tracking::NoMarker);
        assert!(report.removed.is_empty(), "{report:?}");
        assert_eq!(fingerprint_count(&root), 3);
        assert!(report.after > 72 << 10, "{report:?}");
        let text = render_text(&options(&root, 72 << 10), Tracking::NoMarker, &report);
        assert!(text.contains("Can't remove"), "{text}");
        assert!(text.contains("stays over the budget"), "{text}");
        drop(lock);
        // A budget below `debug`'s units removes from them only.
        let (_tmp, root, lock) = busy_release();
        let report = trim(&options(&root, 40 << 10), Tracking::NoMarker);
        assert!(
            matches!(removed(&report).as_slice(), [(Reason::Budget, n)] if *n < 5),
            "{report:?}"
        );
        assert!(report.kept_bytes <= 40 << 10, "{report:?}");
        assert_eq!(
            fsutil::entries(&root.join("release/.fingerprint")).count(),
            3
        );
        drop(lock);

        // `doc/` alone over the budget: no unit goes to make up for it.
        let (_tmp, root) = fixture();
        let doc = root.join("doc/app");
        fs::create_dir_all(&doc).unwrap();
        fs::write(doc.join("index.html"), vec![1u8; 512 << 10]).unwrap();
        let report = trim(&options(&root, 400 << 10), Tracking::NoMarker);
        assert!(report.removed.is_empty(), "{report:?}");
        assert_eq!(fingerprint_count(&root), 3);
        assert!(report.outside >= 512 << 10, "{report:?}");
        let markdown = render_markdown(&options(&root, 400 << 10), Tracking::NoMarker, &report);
        assert!(markdown.contains("| Can't remove: "), "{markdown}");
        // Under the budget, no note.
        let text = render_text(&options(&root, 1 << 30), Tracking::NoMarker, &report);
        assert!(!text.contains("Can't remove"), "{text}");
    }

    #[test]
    fn an_unwritable_step_summary_is_an_error() {
        let (_tmp, root) = fixture();
        let options = Options {
            dry_run: true,
            step_summary: Some(root.join("missing-dir/summary.md")),
            ..opts(&root)
        };
        let err = run(&options).unwrap_err();
        assert!(format!("{err:#}").contains("step summary"), "{err:#}");
    }
}
