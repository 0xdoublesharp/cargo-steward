//! `cargo steward doctor`: checks the setup and reports what to change.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::config::{self, Paths};
use crate::fsutil::{self, human};
use crate::{gc, wrapper};

#[derive(Clone, Copy)]
enum Level {
    Ok,
    Warn,
    Info,
}

fn say(level: Level, message: impl AsRef<str>) {
    let mark = match level {
        Level::Ok => "  ok ",
        Level::Warn => "  !! ",
        Level::Info => "  -- ",
    };
    let mut lines = message.as_ref().lines();
    if let Some(first) = lines.next() {
        println!("{mark}{first}");
    }
    for line in lines {
        println!("     {line}");
    }
}

pub fn run() -> Result<()> {
    let paths = Paths::resolve().context("cannot locate the home directory")?;
    let settings = config::load(&paths)?;
    let policy = settings.policy()?;

    println!("Wrapper");
    check_wrapper(&settings);

    println!("\nToolchains");
    check_toolchains();

    println!("\nProjects");
    let (registry, dirs) = gc::inventory(&paths, &settings.scan_roots)?;
    let workspaces: BTreeSet<PathBuf> = registry
        .roots
        .values()
        .flat_map(|info| info.workspaces.iter().cloned())
        .filter(|w| w.exists())
        .collect();
    check_projects(&workspaces);

    println!("\nDisks");
    check_disks(&settings.scan_roots, &dirs, &settings, &policy);

    println!("\nCleanup");
    check_cleanup(&paths, &settings);
    Ok(())
}

fn check_wrapper(settings: &config::Config) {
    let cargo_home = config::cargo_home();
    let configured = cargo_home
        .as_ref()
        .and_then(|home| fs::read_to_string(home.join("config.toml")).ok())
        .and_then(|text| text.parse::<toml_edit::DocumentMut>().ok())
        .and_then(|doc| {
            doc.get("build")?
                .get("rustc-wrapper")?
                .as_str()
                .map(str::to_owned)
        });
    match &configured {
        Some(w) if w.contains("cargo-steward") => say(
            Level::Ok,
            format!("Cargo's rustc-wrapper is cargo-steward ({w})"),
        ),
        Some(w) => say(
            Level::Warn,
            format!(
                "Cargo's rustc-wrapper is {w}, so builds bypass cargo-steward.\nFix: cargo steward setup"
            ),
        ),
        None => say(
            Level::Warn,
            "No rustc-wrapper is set in Cargo's user config.\nFix: cargo steward setup",
        ),
    }
    for var in ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"] {
        if let Some(value) = env::var_os(var).filter(|v| !v.is_empty()) {
            let value = value.to_string_lossy();
            if value.contains("cargo-steward") {
                say(Level::Ok, format!("{var} points at cargo-steward"));
            } else {
                say(
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
            Some(path) => say(
                Level::Ok,
                format!("Inner wrapper {inner} found at {}", path.display()),
            ),
            None => say(
                Level::Warn,
                format!(
                    "Inner wrapper {inner} isn't installed; builds run rustc directly until it is"
                ),
            ),
        },
        None => say(
            Level::Info,
            "No inner wrapper (such as sccache) is configured",
        ),
    }
}

/// A toolchain's rustc version: minor version, channel, and commit date.
#[derive(Debug, Clone, PartialEq)]
struct RustcVersion {
    minor: u32,
    nightly: bool,
    date: Option<String>,
}

impl RustcVersion {
    /// Parses `rustc 1.101.0-nightly (c980177d8 2026-11-06)`.
    fn parse(text: &str) -> Option<RustcVersion> {
        let version = text.split_whitespace().nth(1)?;
        let minor = version.split('.').nth(1)?.parse().ok()?;
        let date = text
            .rsplit(' ')
            .next()
            .map(|d| d.trim_end_matches(')').to_owned())
            .filter(|d| d.len() == 10 && d.as_bytes()[4] == b'-');
        Some(RustcVersion {
            minor,
            nightly: version.contains("nightly") || version.contains("dev"),
            date,
        })
    }

    /// Whether this toolchain leaves stale object files behind on its own.
    /// The leak arrived in 1.88; Cargo 1.101 removes each unit's previous
    /// output before compiling it (rust-lang/cargo#17675, merged 2026-11-03).
    fn leaks(&self) -> bool {
        match self.minor {
            ..88 => false,
            88..=100 => true,
            101 if self.nightly => self.date.as_deref().is_none_or(|d| d < "2026-11-04"),
            _ => false,
        }
    }
}

fn rustc_version(toolchain: &str) -> Option<RustcVersion> {
    let output = Command::new("rustup")
        .args(["run", toolchain, "rustc", "-V"])
        .output()
        .ok()?;
    RustcVersion::parse(String::from_utf8_lossy(&output.stdout).trim())
}

fn check_toolchains() {
    let Ok(output) = Command::new("rustup").args(["toolchain", "list"]).output() else {
        say(
            Level::Info,
            "rustup isn't installed; skipping the toolchain check",
        );
        return;
    };
    let names: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next().map(str::to_owned))
        .collect();
    let mut leaking = Vec::new();
    let mut fixed = Vec::new();
    for name in &names {
        match rustc_version(name) {
            Some(v) if v.leaks() => leaking.push(name.clone()),
            Some(_) => fixed.push(name.clone()),
            None => {}
        }
    }
    if !leaking.is_empty() {
        say(
            Level::Info,
            format!(
                "{} of {} installed toolchains leave stale object files behind (Rust 1.88 to 1.100):\n{}\n\
                 cargo-steward's wrapper removes them as builds run.",
                leaking.len(),
                names.len(),
                leaking.join(", ")
            ),
        );
    }
    if !fixed.is_empty() {
        say(Level::Ok, format!("Not affected: {}", fixed.join(", ")));
    }
}

fn check_projects(workspaces: &BTreeSet<PathBuf>) {
    if workspaces.is_empty() {
        say(
            Level::Info,
            "No builds recorded yet; projects are checked once they have built through cargo-steward",
        );
        return;
    }
    let mut pins: BTreeMap<String, BTreeSet<PathBuf>> = BTreeMap::new();
    let mut bypass: BTreeSet<PathBuf> = BTreeSet::new();
    for workspace in workspaces {
        if let Some((file, channel)) = toolchain_pin(workspace) {
            pins.entry(channel)
                .or_default()
                .insert(file.parent().unwrap_or(&file).to_path_buf());
        }
        if let Some(config) = wrapper_override(workspace) {
            bypass.insert(config);
        }
    }
    for (channel, projects) in &pins {
        let leaks = channel
            .strip_prefix("1.")
            .and_then(|rest| rest.split('.').next())
            .and_then(|m| m.parse::<u32>().ok())
            .is_some_and(|minor| (88..=100).contains(&minor));
        let list: Vec<String> = projects.iter().map(|p| p.display().to_string()).collect();
        say(
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
        say(
            Level::Warn,
            format!(
                "{} sets build.rustc-wrapper, which outranks the user config: builds there bypass cargo-steward",
                config.display()
            ),
        );
    }
    if bypass.is_empty() {
        say(
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
/// `rustc-wrapper`.
fn wrapper_override(dir: &Path) -> Option<PathBuf> {
    let user = config::cargo_home().map(|h| h.join("config.toml"));
    for ancestor in dir.ancestors() {
        for name in ["config.toml", "config"] {
            let file = ancestor.join(".cargo").join(name);
            if Some(&file) == user.as_ref() {
                continue;
            }
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
    scan_roots: &[PathBuf],
    dirs: &[gc::BuildDir],
    settings: &config::Config,
    policy: &config::Policy,
) {
    if scan_roots.is_empty() {
        say(
            Level::Info,
            "No scan roots: only build directories the wrapper has seen are managed.\n\
             Add a scratch volume or projects folder with `cargo steward setup --scan-root DIR`",
        );
    }
    for root in scan_roots {
        if !root.exists() {
            say(
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
        say(
            level,
            format!(
                "{}: {} free (cleanup frees space below {})",
                root.display(),
                human(free),
                settings.min_free
            ),
        );
        macos_checks(root);
    }
    // Build directories on the system disk share it with Time Machine's local
    // snapshots, which keep deleted files around for about a day.
    let home_device = env::var_os("HOME").and_then(|h| fsutil::device(Path::new(&h)));
    let on_home: Vec<&gc::BuildDir> = dirs
        .iter()
        .filter(|d| d.device.is_some() && d.device == home_device)
        .collect();
    if !on_home.is_empty() && cfg!(target_os = "macos") {
        say(
            Level::Info,
            format!(
                "{} build directories are on the system disk, where local snapshots keep deleted files\n\
                 for about a day. A separate APFS volume avoids that: see `README.md`, \"Build volume\".",
                on_home.len()
            ),
        );
    }
}

fn macos_checks(root: &Path) {
    if !cfg!(target_os = "macos") {
        return;
    }
    if let Ok(output) = Command::new("tmutil").arg("isexcluded").arg(root).output() {
        let text = String::from_utf8_lossy(&output.stdout);
        if text.contains("[Excluded]") {
            say(
                Level::Ok,
                format!("{} is excluded from Time Machine", root.display()),
            );
        } else if text.contains("[Included]") {
            say(
                Level::Warn,
                format!(
                    "{} is backed up by Time Machine.\nFix: tmutil addexclusion {}",
                    root.display(),
                    root.display()
                ),
            );
        }
    }
    if let Ok(output) = Command::new("mdutil").arg("-s").arg(root).output() {
        let text = String::from_utf8_lossy(&output.stdout);
        if text.contains("Indexing enabled") {
            say(
                Level::Warn,
                format!(
                    "Spotlight indexes {}, which costs CPU on every build.\nFix (volume roots): sudo mdutil -i off {}",
                    root.display(),
                    root.display()
                ),
            );
        } else if text.contains("disabled") {
            say(
                Level::Ok,
                format!("Spotlight doesn't index {}", root.display()),
            );
        }
    }
}

fn check_cleanup(paths: &Paths, settings: &config::Config) {
    if settings.auto_gc {
        say(
            Level::Ok,
            format!(
                "Automatic cleanup is on, at most every {}",
                settings.gc_interval
            ),
        );
    } else {
        say(
            Level::Warn,
            "Automatic cleanup is off.\nFix: cargo steward setup (or set auto-gc = true)",
        );
    }
    match fs::metadata(paths.stamp()).and_then(|m| m.modified()) {
        Ok(t) => say(
            Level::Info,
            format!(
                "Last cleanup started {} ago; log: {}",
                fsutil::ago(t),
                paths.log().display()
            ),
        ),
        Err(_) => say(
            Level::Info,
            "No cleanup has run yet. Preview one with `cargo steward gc`",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_which_toolchains_leak() {
        let v = |s: &str| RustcVersion::parse(s).unwrap();
        assert!(!v("rustc 1.87.0 (17067e9ac 2025-05-09)").leaks());
        assert!(v("rustc 1.95.0 (59807616e 2026-04-14)").leaks());
        assert!(v("rustc 1.100.0-beta.12 (b0aaa0fca 2026-11-06)").leaks());
        assert!(v("rustc 1.101.0-nightly (1a2b3c4d5 2026-10-30)").leaks());
        assert!(!v("rustc 1.101.0-nightly (c980177d8 2026-11-06)").leaks());
        assert!(!v("rustc 1.101.0 (aaaaaaaaa 2026-12-20)").leaks());
        assert!(!v("rustc 1.102.0-nightly (bbbbbbbbb 2026-11-20)").leaks());
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
