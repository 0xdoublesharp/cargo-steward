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

use crate::config::{self, Paths};
use crate::fsutil::{self, human};
use crate::{ledger, scan, wrapper};

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

/// How deep under a scan root build directories are searched for, as
/// cleanup does.
const SCAN_DEPTH: usize = 6;

/// A finished thread's result, or the default if it panicked.
fn joined<T: Default>(handle: thread::ScopedJoinHandle<'_, T>) -> T {
    handle.join().unwrap_or_default()
}

pub fn run() -> Result<()> {
    let paths = Paths::resolve().context("cannot locate the home directory")?;
    let (settings, problems) = config::load_lenient(&paths);
    let policy = settings.policy()?;
    let configured = configured_wrapper();
    let home = env::var_os("HOME").map(PathBuf::from);

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

        println!("Settings");
        check_settings(&paths, &problems);

        println!("\nWrapper");
        binary::report(&joined(binary_probe));
        check_wrapper(&settings, configured.as_deref());

        println!("\nToolchains");
        toolchains::report(
            joined(toolchain_probe).as_ref(),
            &pins,
            workspace_roots.len(),
        );

        println!("\nProjects");
        check_projects(&packages, &pins);
        profile::report(&workspace_roots);

        println!("\nCompiler cache");
        sccache::report(&joined(sccache_probe), &packages);

        println!("\nEditors");
        editors::report(home.as_deref(), &workspace_roots);

        println!("\nDisks");
        check_disks(
            &settings.scan_roots,
            &settings,
            &policy,
            &joined(root_probe),
        );
        backup::report_build_dirs(on_home.len(), &joined(tm_probe));

        println!("\nCleanup");
        check_cleanup(&paths, &settings);
        Ok(())
    })
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
    let text = fs::read_to_string(home.join("config.toml")).ok()?;
    let doc = text.parse::<toml_edit::DocumentMut>().ok()?;
    doc.get("build")?
        .get("rustc-wrapper")?
        .as_str()
        .map(str::to_owned)
}

fn check_wrapper(settings: &config::Config, configured: Option<&str>) {
    match configured {
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

fn check_projects(workspaces: &BTreeSet<PathBuf>, pins: &BTreeMap<String, BTreeSet<PathBuf>>) {
    if workspaces.is_empty() {
        say(
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
    settings: &config::Config,
    policy: &config::Policy,
    statuses: &[backup::RootStatus],
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
        if let Some(status) = statuses.iter().find(|s| &s.root == root) {
            backup::report_root(status);
        }
    }
}

/// What in the settings file can't be used, and so has its default.
fn check_settings(paths: &Paths, problems: &[String]) {
    if problems.is_empty() {
        let file = paths.config();
        if file.exists() {
            say(Level::Ok, format!("{} reads cleanly", file.display()));
        } else {
            say(
                Level::Info,
                format!("No {}: the defaults apply", file.display()),
            );
        }
        return;
    }
    for problem in problems {
        say(
            Level::Warn,
            format!(
                "Settings: {problem}\nFix: edit {}",
                paths.config().display()
            ),
        );
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
