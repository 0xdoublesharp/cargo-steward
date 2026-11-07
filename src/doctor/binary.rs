//! Binary health: the wrapper cargo is configured to run exists, can run,
//! and is this version; and `cargo steward` can find the binary on PATH.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{Level, cmd, say};
use crate::wrapper;

/// This binary's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What was found about the configured wrapper binary.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Health {
    /// No wrapper is configured, or it isn't cargo-steward.
    #[default]
    NotConfigured,
    Missing(String),
    NotExecutable(PathBuf),
    /// Ran it; its reported version (`None` if it printed nothing usable).
    Version(PathBuf, Option<String>),
    /// It is this binary.
    Running(PathBuf),
}

/// Parses `cargo steward 0.1.0` (clap's `--version` output).
pub fn parse_version(text: &str) -> Option<String> {
    let word = text.lines().next()?.split_whitespace().last()?;
    word.chars()
        .next()
        .is_some_and(|c| c.is_ascii_digit())
        .then(|| word.to_owned())
}

/// Checks the configured wrapper, running it for its version unless it is
/// this binary.
pub fn probe(configured: Option<&str>) -> Health {
    let Some(configured) = configured.filter(|w| w.contains("cargo-steward")) else {
        return Health::NotConfigured;
    };
    let Some(path) = wrapper::find_program(configured.as_ref()) else {
        return Health::Missing(configured.to_owned());
    };
    if !is_executable(&path) {
        return Health::NotExecutable(path);
    }
    let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
    let running = env::current_exe().ok().and_then(|e| e.canonicalize().ok());
    if running.as_ref() == Some(&canonical) {
        return Health::Running(path);
    }
    let version = cmd::stdout(Command::new(&path).arg("--version").env_remove("CARGO"))
        .and_then(|t| parse_version(&t));
    Health::Version(path, version)
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

pub fn report(health: &Health) {
    match health {
        Health::NotConfigured => {}
        Health::Missing(configured) => say(
            Level::Warn,
            format!(
                "The configured wrapper {configured} doesn't exist, so every build fails.\n\
                 Fix: reinstall cargo-steward, or run `cargo steward setup --undo`"
            ),
        ),
        Health::NotExecutable(path) => say(
            Level::Warn,
            format!(
                "The configured wrapper {} isn't executable, so every build fails.\nFix: chmod +x {}",
                path.display(),
                path.display()
            ),
        ),
        Health::Running(path) => say(
            Level::Ok,
            format!("The wrapper is this binary ({}), {VERSION}", path.display()),
        ),
        Health::Version(path, Some(v)) if v == VERSION => say(
            Level::Ok,
            format!(
                "The wrapper {} is {v}, the same as this one",
                path.display()
            ),
        ),
        Health::Version(path, other) => say(
            Level::Warn,
            format!(
                "The wrapper {} is {}, but this binary is {VERSION}.\n\
                 Fix: reinstall (scripts/install.sh, or `cargo install --locked cargo-steward`), then `cargo steward setup`",
                path.display(),
                other.as_deref().unwrap_or("an unknown version"),
            ),
        ),
    }
    // `cargo steward` runs `cargo-steward` from PATH.
    if wrapper::find_program("cargo-steward".as_ref()).is_none() {
        say(
            Level::Warn,
            "cargo-steward isn't on PATH, so `cargo steward` can't find it.\n\
             Fix: add ~/.cargo/bin (or wherever it is installed) to PATH",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_output() {
        assert_eq!(
            parse_version("cargo steward 0.1.0\n").as_deref(),
            Some("0.1.0")
        );
        assert_eq!(
            parse_version("cargo-steward 1.2.3-beta.1").as_deref(),
            Some("1.2.3-beta.1")
        );
        assert_eq!(parse_version("error: unexpected argument"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn reports_a_missing_or_foreign_wrapper() {
        assert_eq!(probe(None), Health::NotConfigured);
        assert_eq!(probe(Some("sccache")), Health::NotConfigured);
        assert_eq!(
            probe(Some("/nonexistent/bin/cargo-steward")),
            Health::Missing("/nonexistent/bin/cargo-steward".into())
        );
    }

    #[cfg(unix)]
    #[test]
    fn notices_a_wrapper_that_cant_run() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("cargo-steward");
        std::fs::write(&exe, "#!/bin/sh\necho 'cargo steward 0.0.1'\n").unwrap();
        let configured = exe.to_str().unwrap();
        assert_eq!(probe(Some(configured)), Health::NotExecutable(exe.clone()));
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A script just written can briefly be "text file busy" on Linux
        // while another test's fork still holds it open.
        let mut health = probe(Some(configured));
        for _ in 0..20 {
            if health != Health::Version(exe.clone(), None) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            health = probe(Some(configured));
        }
        assert_eq!(health, Health::Version(exe.clone(), Some("0.0.1".into())));
    }
}
