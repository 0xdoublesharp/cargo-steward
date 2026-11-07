//! The dev profile's debug info: full debug info is most of what a debug
//! build writes, and the object files the leak keeps.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use super::{Level, say};
use crate::config;

/// How much debug info a `[profile.dev]` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Debug {
    /// Not set: the dev profile's default, full debug info.
    Unset,
    Full,
    Reduced,
}

/// Reads `debug` from `[profile.dev]` in a `Cargo.toml` or Cargo config.
pub fn dev_debug(text: &str) -> Debug {
    let Ok(doc) = text.parse::<toml::Table>() else {
        return Debug::Unset;
    };
    let Some(value) = doc
        .get("profile")
        .and_then(|p| p.get("dev"))
        .and_then(|d| d.get("debug"))
    else {
        return Debug::Unset;
    };
    classify(value)
}

fn classify(value: &toml::Value) -> Debug {
    match value {
        toml::Value::Boolean(true) => Debug::Full,
        toml::Value::Boolean(false) => Debug::Reduced,
        toml::Value::Integer(2) => Debug::Full,
        toml::Value::Integer(_) => Debug::Reduced,
        toml::Value::String(s) if s == "full" => Debug::Full,
        toml::Value::String(_) => Debug::Reduced,
        _ => Debug::Unset,
    }
}

/// `CARGO_PROFILE_DEV_DEBUG` as cargo reads it.
fn env_debug(value: &str) -> Debug {
    let value = value.trim();
    match value {
        "true" | "2" | "full" => Debug::Full,
        "" => Debug::Unset,
        _ => Debug::Reduced,
    }
}

/// The workspace a package directory belongs to: the nearest directory at
/// or above it whose `Cargo.toml` has a `[workspace]` table, else the
/// package itself.
pub fn workspace_root(package: &Path) -> Option<PathBuf> {
    let mut first = None;
    for dir in package.ancestors() {
        let Ok(text) = fs::read_to_string(dir.join("Cargo.toml")) else {
            continue;
        };
        if first.is_none() {
            first = Some(dir.to_path_buf());
        }
        if text
            .parse::<toml::Table>()
            .is_ok_and(|doc| doc.contains_key("workspace"))
        {
            return Some(dir.to_path_buf());
        }
    }
    first
}

/// Workspace roots of the recorded package directories.
pub fn workspace_roots(packages: &BTreeSet<PathBuf>) -> BTreeSet<PathBuf> {
    packages.iter().filter_map(|p| workspace_root(p)).collect()
}

/// Mentions `debug = "line-tables-only"` for workspaces whose dev profile
/// keeps full debug info, unless the environment or the user-wide Cargo
/// config already reduces it everywhere.
pub fn report(roots: &BTreeSet<PathBuf>) {
    if roots.is_empty() {
        return;
    }
    let from_env = env::var("CARGO_PROFILE_DEV_DEBUG")
        .map(|v| env_debug(&v))
        .unwrap_or(Debug::Unset);
    let from_user = config::cargo_home()
        .and_then(|h| fs::read_to_string(h.join("config.toml")).ok())
        .map_or(Debug::Unset, |t| dev_debug(&t));
    if from_env == Debug::Reduced || (from_env == Debug::Unset && from_user == Debug::Reduced) {
        say(
            Level::Ok,
            "Debug info in dev builds is reduced for every project (environment or ~/.cargo/config.toml)",
        );
        return;
    }
    let full: Vec<String> = roots
        .iter()
        .filter(|root| {
            fs::read_to_string(root.join("Cargo.toml"))
                .is_ok_and(|text| dev_debug(&text) != Debug::Reduced)
        })
        .map(|root| root.display().to_string())
        .collect();
    if full.is_empty() {
        say(
            Level::Ok,
            "Every recorded workspace reduces debug info in dev builds",
        );
        return;
    }
    const SHOWN: usize = 8;
    let mut list = full.iter().take(SHOWN).cloned().collect::<Vec<_>>();
    if full.len() > SHOWN {
        list.push(format!("and {} more", full.len() - SHOWN));
    }
    say(
        Level::Info,
        format!(
            "{} of {} recorded workspaces build with full debug info in the dev profile:\n{}\n\
             `debug = \"line-tables-only\"` under [profile.dev] (in each Cargo.toml, or once in\n\
             ~/.cargo/config.toml) makes debug builds smaller and faster; backtraces keep file and\n\
             line numbers, debuggers lose local variables. Cargo plans to make it the dev default\n\
             (rust-lang/cargo#17518).",
            full.len(),
            roots.len(),
            list.join("\n")
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_dev_debug_settings() {
        assert_eq!(dev_debug("[package]\nname = \"a\"\n"), Debug::Unset);
        assert_eq!(dev_debug("[profile.dev]\nopt-level = 1\n"), Debug::Unset);
        assert_eq!(dev_debug("[profile.dev]\ndebug = true\n"), Debug::Full);
        assert_eq!(dev_debug("[profile.dev]\ndebug = 2\n"), Debug::Full);
        assert_eq!(dev_debug("[profile.dev]\ndebug = \"full\"\n"), Debug::Full);
        assert_eq!(dev_debug("[profile.dev]\ndebug = 1\n"), Debug::Reduced);
        assert_eq!(dev_debug("[profile.dev]\ndebug = false\n"), Debug::Reduced);
        assert_eq!(
            dev_debug("[profile.dev]\ndebug = \"line-tables-only\"\n"),
            Debug::Reduced
        );
        // Malformed TOML, or a value cargo would reject: the default.
        assert_eq!(dev_debug("[profile.dev\n"), Debug::Unset);
        assert_eq!(dev_debug("[profile.dev]\ndebug = 1.5\n"), Debug::Unset);
        assert_eq!(env_debug("line-tables-only"), Debug::Reduced);
        assert_eq!(env_debug("true"), Debug::Full);
    }

    #[test]
    fn finds_the_workspace_a_package_belongs_to() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(ws.join("crates/a")).unwrap();
        fs::write(
            ws.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .unwrap();
        fs::write(ws.join("crates/a/Cargo.toml"), "[package]\nname = \"a\"\n").unwrap();
        assert_eq!(workspace_root(&ws.join("crates/a")), Some(ws.clone()));

        let solo = tmp.path().join("solo");
        fs::create_dir_all(&solo).unwrap();
        fs::write(solo.join("Cargo.toml"), "[package]\nname = \"solo\"\n").unwrap();
        assert_eq!(workspace_root(&solo), Some(solo.clone()));
    }
}
