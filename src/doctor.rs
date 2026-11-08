//! `cargo steward doctor`: checks the setup and reports what to change.
//!
//! The slow parts (external commands such as `rustup`, `sccache` and
//! `tmutil`, and measuring toolchains) run in parallel threads while the
//! local checks run; the report is printed in a fixed order once they finish.

mod backup;
mod binary;
mod cmd;
mod editors;
mod profile;
mod sccache;
mod toolchains;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::{self, Paths};
use crate::fsutil::{self, human};
use crate::{json, ledger, scan, wrapper};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Level {
    Ok,
    Warn,
    Info,
}

/// One thing a check found: its text as printed, where a line starting
/// `Fix:` (or `Fix (when):`) begins what to do about it.
#[derive(Debug)]
struct Finding {
    level: Level,
    text: String,
}

/// A titled group of findings.
#[derive(Debug)]
struct Section {
    title: &'static str,
    findings: Vec<Finding>,
}

/// What every check found, in order. Printed as text or as one JSON
/// document, so both say the same.
#[derive(Debug, Default)]
struct Report {
    sections: Vec<Section>,
}

impl Report {
    /// Starts a section: what's said next belongs to it.
    fn section(&mut self, title: &'static str) {
        self.sections.push(Section {
            title,
            findings: Vec::new(),
        });
    }

    fn say(&mut self, level: Level, message: impl AsRef<str>) {
        if self.sections.is_empty() {
            self.section("");
        }
        if let Some(section) = self.sections.last_mut() {
            section.findings.push(Finding {
                level,
                text: message.as_ref().to_owned(),
            });
        }
    }

    fn text(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for (i, section) in self.sections.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            let _ = writeln!(out, "{}", section.title);
            for finding in &section.findings {
                let mark = match finding.level {
                    Level::Ok => "  ok ",
                    Level::Warn => "  !! ",
                    Level::Info => "  -- ",
                };
                let mut lines = finding.text.lines();
                if let Some(first) = lines.next() {
                    let _ = writeln!(out, "{mark}{first}");
                }
                for line in lines {
                    let _ = writeln!(out, "     {line}");
                }
            }
        }
        out
    }

    fn document(&self) -> Document {
        let mut counts = Counts::default();
        let sections = self
            .sections
            .iter()
            .map(|section| SectionDoc {
                id: section.title.to_lowercase().replace(' ', "_"),
                title: section.title,
                checks: section
                    .findings
                    .iter()
                    .map(|finding| {
                        match finding.level {
                            Level::Ok => counts.ok += 1,
                            Level::Warn => counts.warn += 1,
                            Level::Info => counts.info += 1,
                        }
                        Check::of(finding)
                    })
                    .collect(),
            })
            .collect();
        Document {
            version: json::VERSION,
            counts,
            sections,
        }
    }
}

/// `doctor --json`.
#[derive(Debug, Serialize)]
struct Document {
    version: u32,
    /// How many checks have each level.
    counts: Counts,
    sections: Vec<SectionDoc>,
}

#[derive(Debug, Default, Serialize)]
struct Counts {
    ok: usize,
    warn: usize,
    info: usize,
}

#[derive(Debug, Serialize)]
struct SectionDoc {
    /// `settings`, `wrapper`, `toolchains`, `projects`, `compiler_cache`,
    /// `editors`, `disks` or `cleanup`.
    id: String,
    title: &'static str,
    checks: Vec<Check>,
}

#[derive(Debug, PartialEq, Serialize)]
struct Check {
    level: Level,
    /// What was found, without the fix.
    message: String,
    /// What to do about it: commands or a change to make.
    #[serde(skip_serializing_if = "Option::is_none")]
    fix: Option<String>,
    /// When the fix applies, if only sometimes (`volume roots`).
    #[serde(skip_serializing_if = "Option::is_none")]
    fix_note: Option<String>,
}

impl Check {
    /// Splits a finding's text at its `Fix:` line.
    fn of(finding: &Finding) -> Check {
        let lines: Vec<&str> = finding.text.lines().collect();
        let at = lines
            .iter()
            .position(|l| l.starts_with("Fix:") || l.starts_with("Fix ("));
        let Some(at) = at else {
            return Check {
                level: finding.level,
                message: finding.text.trim_end().to_owned(),
                fix: None,
                fix_note: None,
            };
        };
        let (label, rest) = lines[at].split_once(':').unwrap_or((lines[at], ""));
        let fix_note = label
            .strip_prefix("Fix (")
            .and_then(|l| l.strip_suffix(')'))
            .map(str::to_owned);
        let fix: Vec<&str> = std::iter::once(rest.trim())
            .filter(|first| !first.is_empty())
            .chain(lines[at + 1..].iter().copied())
            .collect();
        Check {
            level: finding.level,
            message: lines[..at].join("\n"),
            fix: Some(fix.join("\n")),
            fix_note,
        }
    }
}

/// How deep under a scan root build directories are searched for, as
/// cleanup does.
const SCAN_DEPTH: usize = 6;

/// A finished thread's result, or the default if it panicked.
fn joined<T: Default>(handle: thread::ScopedJoinHandle<'_, T>) -> T {
    handle.join().unwrap_or_default()
}

pub fn run(as_json: bool) -> Result<()> {
    let report = collect()?;
    if as_json {
        json::print(&report.document())
    } else {
        print!("{}", report.text());
        Ok(())
    }
}

fn collect() -> Result<Report> {
    let paths = Paths::resolve().context("cannot locate the home directory")?;
    let (settings, problems) = config::load_lenient(&paths);
    let policy = settings.policy()?;
    let configured = configured_wrapper();
    let home = env::var_os("HOME").map(PathBuf::from);

    let mut report = Report::default();
    let out = &mut report;
    thread::scope(|s| -> Result<()> {
        let toolchain_probe = s.spawn(toolchains::probe);
        let sccache_probe = s.spawn(sccache::probe);
        let binary_probe = s.spawn(|| binary::probe(configured.as_deref()));
        let root_probe = s.spawn(|| backup::probe_roots(&settings.scan_roots));

        // Read only: doctor reports, and agents run it in sandboxes where
        // the steward home can't be written.
        let registry = ledger::peek(&paths);
        // Build directories on the system disk share it with Time Machine's
        // local snapshots, which keep deleted files around for about a day.
        let home_device = home.as_deref().and_then(fsutil::device);
        let on_home = system_disk_build_dirs(&registry, &settings.scan_roots, home_device);
        let tm_probe = {
            let dirs = on_home.clone();
            s.spawn(move || backup::isexcluded(&dirs))
        };

        let packages: BTreeSet<PathBuf> = registry
            .roots
            .values()
            .flat_map(|info| info.workspaces.iter().cloned())
            .filter(|w| w.exists())
            .collect();
        let workspace_roots = profile::workspace_roots(&packages);
        let pins = toolchain_pins(&packages);

        out.section("Settings");
        check_settings(out, &paths, &problems);

        out.section("Wrapper");
        binary::report(out, &joined(binary_probe));
        check_wrapper(out, &settings, configured.as_deref());

        out.section("Toolchains");
        toolchains::report(
            out,
            joined(toolchain_probe).as_ref(),
            &pins,
            workspace_roots.len(),
        );

        out.section("Projects");
        check_projects(out, &packages, &pins);
        profile::report(out, &workspace_roots);

        out.section("Compiler cache");
        sccache::report(out, &joined(sccache_probe), &packages);

        out.section("Editors");
        editors::report(out, home.as_deref(), &workspace_roots);

        out.section("Disks");
        check_disks(
            out,
            &settings.scan_roots,
            &settings,
            &policy,
            &joined(root_probe),
        );
        backup::report_build_dirs(out, on_home.len(), &joined(tm_probe));

        out.section("Cleanup");
        check_cleanup(out, &paths, &settings);
        Ok(())
    })?;
    Ok(report)
}

/// Build directories on the device `home_device`: recorded ones, and any
/// under scan roots on that device. Unlike [`crate::gc::inventory`] this neither
/// searches scan roots on other volumes nor inspects each directory's
/// contents, which on a big scratch volume takes a minute.
fn system_disk_build_dirs(
    registry: &ledger::Registry,
    scan_roots: &[PathBuf],
    home_device: Option<u64>,
) -> Vec<PathBuf> {
    let Some(home_device) = home_device else {
        return Vec::new();
    };
    let on_home = |p: &Path| fsutil::device(p) == Some(home_device);
    let mut roots: Vec<PathBuf> = registry
        .roots
        .keys()
        .filter_map(|r| scan::find_root(r))
        .filter(|r| on_home(r))
        .collect();
    let local_roots: Vec<PathBuf> = scan_roots.iter().filter(|r| on_home(r)).cloned().collect();
    roots.extend(scan::discover(&local_roots, SCAN_DEPTH));
    scan::outermost(roots)
}

/// `build.rustc-wrapper` in the user-wide Cargo config.
fn configured_wrapper() -> Option<String> {
    let home = config::cargo_home()?;
    let text = fs::read_to_string(config::user_cargo_config(&home)).ok()?;
    let doc = text.parse::<toml_edit::DocumentMut>().ok()?;
    doc.get("build")?
        .get("rustc-wrapper")?
        .as_str()
        .map(str::to_owned)
}

fn check_wrapper(out: &mut Report, settings: &config::Config, configured: Option<&str>) {
    match configured {
        Some(w) if w.contains("cargo-steward") => out.say(
            Level::Ok,
            format!("Cargo's rustc-wrapper is cargo-steward ({w})"),
        ),
        Some(w) => out.say(
            Level::Warn,
            format!(
                "Cargo's rustc-wrapper is {w}, so builds bypass cargo-steward.\nFix: cargo steward setup"
            ),
        ),
        None => out.say(
            Level::Warn,
            "No rustc-wrapper is set in Cargo's user config.\nFix: cargo steward setup",
        ),
    }
    for var in ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"] {
        if let Some(value) = env::var_os(var).filter(|v| !v.is_empty()) {
            let value = value.to_string_lossy();
            if value.contains("cargo-steward") {
                out.say(Level::Ok, format!("{var} points at cargo-steward"));
            } else {
                out.say(
                    Level::Warn,
                    format!(
                        "{var}={value} overrides Cargo's config in this environment, bypassing cargo-steward.\n\
                         Fix: set {var} to cargo-steward and CARGO_STEWARD_INNER={value}"
                    ),
                );
            }
        }
    }
    let inner = env::var("CARGO_STEWARD_INNER")
        .ok()
        .or_else(|| settings.inner_wrapper.clone());
    match inner.filter(|i| !i.is_empty()) {
        Some(inner) => match wrapper::find_program(inner.as_ref()) {
            Some(path) => out.say(
                Level::Ok,
                format!("Inner wrapper {inner} found at {}", path.display()),
            ),
            None => out.say(
                Level::Warn,
                format!(
                    "Inner wrapper {inner} isn't installed; builds run rustc directly until it is"
                ),
            ),
        },
        None => out.say(
            Level::Info,
            "No inner wrapper (such as sccache) is configured",
        ),
    }
    let from = if env::var_os(wrapper::MAX_COMPILES_VAR).is_some() {
        wrapper::MAX_COMPILES_VAR
    } else {
        "max-compiles"
    };
    match wrapper::compile_limit(settings) {
        Some(limit) => out.say(
            Level::Ok,
            format!("At most {limit} compile(s) run at once, machine-wide ({from})"),
        ),
        None => out.say(
            Level::Info,
            format!("Compiles run as many at once as Cargo starts ({from} is unset)"),
        ),
    }
    match wrapper::memory_budget(settings) {
        Some(budget) => out.say(Level::Info, format!(
            "Compile memory reservations: {}; unknown compilers reserve {}. Estimates are not an RSS cap; inner cache wrappers use the fallback estimate",
            crate::fsutil::human(budget), settings.compile_memory_estimate
        )),
        None => out.say(Level::Info, "Compile memory admission is disabled"),
    }
}

/// The toolchain each recorded package's project pins, and the projects
/// pinning it.
fn toolchain_pins(packages: &BTreeSet<PathBuf>) -> BTreeMap<String, BTreeSet<PathBuf>> {
    let mut pins: BTreeMap<String, BTreeSet<PathBuf>> = BTreeMap::new();
    for package in packages {
        if let Some((file, channel)) = toolchain_pin(package) {
            pins.entry(channel)
                .or_default()
                .insert(file.parent().unwrap_or(&file).to_path_buf());
        }
    }
    pins
}

fn check_projects(
    out: &mut Report,
    workspaces: &BTreeSet<PathBuf>,
    pins: &BTreeMap<String, BTreeSet<PathBuf>>,
) {
    if workspaces.is_empty() {
        out.say(
            Level::Info,
            "No builds recorded yet; projects are checked once they have built through cargo-steward",
        );
        return;
    }
    let bypass: BTreeSet<PathBuf> = workspaces
        .iter()
        .filter_map(|w| wrapper_override(w))
        .collect();
    for (channel, projects) in pins {
        let leaks = channel
            .strip_prefix("1.")
            .and_then(|rest| rest.split('.').next())
            .and_then(|m| m.parse::<u32>().ok())
            .is_some_and(|minor| (88..=100).contains(&minor));
        let list: Vec<String> = projects.iter().map(|p| p.display().to_string()).collect();
        out.say(
            if leaks { Level::Info } else { Level::Ok },
            format!(
                "{} project(s) pin {channel}{}:\n{}",
                projects.len(),
                if leaks {
                    " (leaks without cargo-steward; fixed from 1.101)"
                } else {
                    ""
                },
                list.join("\n")
            ),
        );
    }
    for config in &bypass {
        out.say(
            Level::Warn,
            format!(
                "{} sets build.rustc-wrapper, which outranks the user config: builds there bypass cargo-steward",
                config.display()
            ),
        );
    }
    if bypass.is_empty() {
        out.say(
            Level::Ok,
            format!(
                "{} recorded project(s) build through cargo-steward",
                workspaces.len()
            ),
        );
    }
}

/// The nearest `rust-toolchain.toml` or `rust-toolchain` above `dir`, and
/// the channel it pins.
fn toolchain_pin(dir: &Path) -> Option<(PathBuf, String)> {
    for ancestor in dir.ancestors() {
        for name in ["rust-toolchain.toml", "rust-toolchain"] {
            let file = ancestor.join(name);
            let Ok(text) = fs::read_to_string(&file) else {
                continue;
            };
            let channel = text
                .parse::<toml_edit::DocumentMut>()
                .ok()
                .and_then(|doc| {
                    doc.get("toolchain")?
                        .get("channel")?
                        .as_str()
                        .map(str::to_owned)
                })
                .or_else(|| text.lines().next().map(|l| l.trim().to_owned()))?;
            return Some((file, channel));
        }
    }
    None
}

/// A project-level `.cargo/config.toml` above `dir` that sets its own
/// `rustc-wrapper`. `$CARGO_HOME`'s config (`~/.cargo` above projects in the
/// home directory) is the user-wide one, not a project's.
fn wrapper_override(dir: &Path) -> Option<PathBuf> {
    let cargo_home = config::cargo_home();
    for ancestor in dir.ancestors() {
        let dot_cargo = ancestor.join(".cargo");
        if cargo_home
            .as_deref()
            .is_some_and(|h| fsutil::same_file(&dot_cargo, h))
        {
            continue;
        }
        for name in ["config.toml", "config"] {
            let file = dot_cargo.join(name);
            let Ok(text) = fs::read_to_string(&file) else {
                continue;
            };
            let sets = text
                .parse::<toml_edit::DocumentMut>()
                .ok()
                .and_then(|doc| doc.get("build")?.get("rustc-wrapper").map(|_| ()))
                .is_some();
            if sets {
                return Some(file);
            }
        }
    }
    None
}

fn check_disks(
    out: &mut Report,
    scan_roots: &[PathBuf],
    settings: &config::Config,
    policy: &config::Policy,
    statuses: &[backup::RootStatus],
) {
    if scan_roots.is_empty() {
        out.say(
            Level::Info,
            "No scan roots: only build directories the wrapper has seen are managed.\n\
             Add a scratch volume or projects folder with `cargo steward setup --scan-root DIR`",
        );
    }
    for root in scan_roots {
        if !root.exists() {
            out.say(
                Level::Warn,
                format!(
                    "Scan root {} doesn't exist (is the volume mounted?)",
                    root.display()
                ),
            );
            continue;
        }
        let free = fsutil::free_space(root).unwrap_or(0);
        let level = if free < policy.min_free {
            Level::Warn
        } else {
            Level::Ok
        };
        out.say(
            level,
            format!(
                "{}: {} free (cleanup frees space below {})",
                root.display(),
                human(free),
                settings.min_free
            ),
        );
        if let Some(status) = statuses.iter().find(|s| &s.root == root) {
            backup::report_root(out, status);
        }
    }
}

/// What in the settings file can't be used, and so has its default.
fn check_settings(out: &mut Report, paths: &Paths, problems: &[String]) {
    if problems.is_empty() {
        let file = paths.config();
        if file.exists() {
            out.say(Level::Ok, format!("{} reads cleanly", file.display()));
        } else {
            out.say(
                Level::Info,
                format!("No {}: the defaults apply", file.display()),
            );
        }
        return;
    }
    for problem in problems {
        out.say(
            Level::Warn,
            format!(
                "Settings: {problem}\nFix: edit {}",
                paths.config().display()
            ),
        );
    }
}

fn check_cleanup(out: &mut Report, paths: &Paths, settings: &config::Config) {
    if settings.auto_gc {
        out.say(
            Level::Ok,
            format!(
                "Automatic cleanup is on, at most every {}",
                settings.gc_interval
            ),
        );
    } else {
        out.say(
            Level::Warn,
            "Automatic cleanup is off.\nFix: cargo steward setup (or set auto-gc = true)",
        );
    }
    match fs::metadata(paths.stamp()).and_then(|m| m.modified()) {
        Ok(t) => out.say(
            Level::Info,
            format!(
                "Last cleanup started {} ago; log: {}",
                fsutil::ago(t),
                paths.log().display()
            ),
        ),
        Err(_) => out.say(
            Level::Info,
            "No cleanup has run yet. Preview one with `cargo steward gc`",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(level: Level, text: &str) -> Check {
        Check::of(&Finding {
            level,
            text: text.to_owned(),
        })
    }

    #[test]
    fn splits_each_finding_at_its_fix() {
        assert_eq!(
            check(Level::Ok, "All good"),
            Check {
                level: Level::Ok,
                message: "All good".into(),
                fix: None,
                fix_note: None,
            }
        );
        let found = check(
            Level::Warn,
            "No rustc-wrapper is set.\nMore detail.\nFix: cargo steward setup",
        );
        assert_eq!(found.message, "No rustc-wrapper is set.\nMore detail.");
        assert_eq!(found.fix.as_deref(), Some("cargo steward setup"));
        assert_eq!(found.fix_note, None);
        // A fix running over several lines, and one that only sometimes applies.
        let found = check(
            Level::Warn,
            "Two are backed up:\n/a\n/b\nFix (a sticky exclusion; no sudo needed):\ntmutil addexclusion /a\ntmutil addexclusion /b",
        );
        assert_eq!(found.message, "Two are backed up:\n/a\n/b");
        assert_eq!(
            found.fix.as_deref(),
            Some("tmutil addexclusion /a\ntmutil addexclusion /b")
        );
        assert_eq!(
            found.fix_note.as_deref(),
            Some("a sticky exclusion; no sudo needed")
        );
        let found = check(
            Level::Warn,
            "Indexed.\nFix (volume roots): sudo mdutil -i off /V",
        );
        assert_eq!(found.fix.as_deref(), Some("sudo mdutil -i off /V"));
        assert_eq!(found.fix_note.as_deref(), Some("volume roots"));
    }

    #[test]
    fn text_and_document_come_from_the_same_findings() {
        let mut report = Report::default();
        report.section("Settings");
        report.say(Level::Ok, "Reads cleanly");
        report.section("Compiler cache");
        report.say(Level::Warn, "Split caches:\n/a\nFix: set it once");
        report.say(Level::Info, "Note");
        assert_eq!(
            report.text(),
            "Settings\n  ok Reads cleanly\n\nCompiler cache\n  !! Split caches:\n     /a\n     Fix: set it once\n  -- Note\n"
        );
        let doc = serde_json::to_value(report.document()).unwrap();
        assert_eq!(doc["version"], 1);
        assert_eq!(
            doc["counts"],
            serde_json::json!({"ok": 1, "warn": 1, "info": 1})
        );
        assert_eq!(doc["sections"][1]["id"], "compiler_cache");
        assert_eq!(doc["sections"][1]["title"], "Compiler cache");
        let warn = &doc["sections"][1]["checks"][0];
        assert_eq!(warn["level"], "warn");
        assert_eq!(warn["message"], "Split caches:\n/a");
        assert_eq!(warn["fix"], "set it once");
        // No fix: no field.
        assert!(doc["sections"][0]["checks"][0].get("fix").is_none());
    }

    #[test]
    fn finds_toolchain_pins_and_wrapper_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("p");
        fs::create_dir_all(project.join("crates/a")).unwrap();
        fs::write(
            project.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.95.0\"\n",
        )
        .unwrap();
        fs::create_dir_all(project.join(".cargo")).unwrap();
        fs::write(
            project.join(".cargo/config.toml"),
            "[build]\nrustc-wrapper = \"sccache\"\n",
        )
        .unwrap();
        let (file, channel) = toolchain_pin(&project.join("crates/a")).unwrap();
        assert_eq!(
            (file, channel.as_str()),
            (project.join("rust-toolchain.toml"), "1.95.0")
        );
        assert_eq!(
            wrapper_override(&project.join("crates/a")),
            Some(project.join(".cargo/config.toml"))
        );

        let legacy = tmp.path().join("q");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("rust-toolchain"), "nightly-2026-01-01\n").unwrap();
        assert_eq!(toolchain_pin(&legacy).unwrap().1, "nightly-2026-01-01");
    }
}
