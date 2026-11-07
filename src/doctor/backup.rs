//! macOS only: Time Machine and Spotlight on build output.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::{Level, cmd, say};
use crate::setup::shell_quote;

/// One line of `tmutil isexcluded`: `[Excluded]  /path` or `[Included]  /path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exclusion {
    pub path: PathBuf,
    pub excluded: bool,
}

/// Parses `tmutil isexcluded`'s output. Lines it can't decide on
/// (`[UNKNOWN]`, a missing path) are left out.
pub fn parse_isexcluded(text: &str) -> Vec<Exclusion> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let (excluded, rest) = match line.strip_prefix("[Excluded]") {
                Some(rest) => (true, rest),
                None => (false, line.strip_prefix("[Included]")?),
            };
            Some(Exclusion {
                path: PathBuf::from(rest.trim()),
                excluded,
            })
        })
        .collect()
}

/// Asks Time Machine about all of `dirs` in one call. Empty off macOS.
pub fn isexcluded(dirs: &[PathBuf]) -> Vec<Exclusion> {
    if !cfg!(target_os = "macos") || dirs.is_empty() {
        return Vec::new();
    }
    cmd::stdout(Command::new("tmutil").arg("isexcluded").args(dirs))
        .map(|t| parse_isexcluded(&t))
        .unwrap_or_default()
}

/// What `mdutil -s` says about a volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indexing {
    On,
    Off,
    Unknown,
}

/// Time Machine and Spotlight status of a scan root.
#[derive(Debug, Clone)]
pub struct RootStatus {
    pub root: PathBuf,
    pub excluded: Option<bool>,
    pub indexing: Indexing,
}

/// Checks each scan root. Empty off macOS.
pub fn probe_roots(roots: &[PathBuf]) -> Vec<RootStatus> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let existing: Vec<PathBuf> = roots.iter().filter(|r| r.exists()).cloned().collect();
    std::thread::scope(|s| {
        let tm = s.spawn(|| isexcluded(&existing));
        let md: Vec<_> = existing
            .iter()
            .map(|root| s.spawn(move || indexing(root)))
            .collect();
        let tm = tm.join().unwrap_or_default();
        existing
            .iter()
            .zip(md)
            .map(|(root, job)| RootStatus {
                root: root.clone(),
                excluded: tm.iter().find(|e| &e.path == root).map(|e| e.excluded),
                indexing: job.join().unwrap_or(Indexing::Unknown),
            })
            .collect()
    })
}

fn indexing(root: &Path) -> Indexing {
    match cmd::stdout(Command::new("mdutil").arg("-s").arg(root)) {
        Some(text) if text.contains("Indexing enabled") => Indexing::On,
        Some(text) if text.contains("disabled") => Indexing::Off,
        _ => Indexing::Unknown,
    }
}

pub fn report_root(status: &RootStatus) {
    let root = status.root.display();
    match status.excluded {
        Some(true) => say(Level::Ok, format!("{root} is excluded from Time Machine")),
        Some(false) => say(
            Level::Warn,
            format!("{root} is backed up by Time Machine.\nFix: tmutil addexclusion {root}"),
        ),
        None => {}
    }
    match status.indexing {
        Indexing::On => say(
            Level::Warn,
            format!(
                "Spotlight indexes {root}, which costs CPU on every build.\nFix (volume roots): sudo mdutil -i off {root}"
            ),
        ),
        Indexing::Off => say(Level::Ok, format!("Spotlight doesn't index {root}")),
        Indexing::Unknown => {}
    }
}

/// Build directories on the system disk that Time Machine still backs up.
/// Cargo excludes target directories it creates itself; ones a script
/// created first aren't.
pub fn report_build_dirs(on_system_disk: usize, statuses: &[Exclusion]) {
    if !cfg!(target_os = "macos") || on_system_disk == 0 {
        return;
    }
    let included: Vec<&Exclusion> = statuses.iter().filter(|e| !e.excluded).collect();
    say(
        Level::Info,
        format!(
            "{on_system_disk} build directories are on the system disk, where local snapshots keep deleted files\n\
             for about a day. A separate APFS volume avoids that: `cargo steward setup --volume NAME`."
        ),
    );
    if included.is_empty() {
        if !statuses.is_empty() {
            say(Level::Ok, "Time Machine excludes all of them");
        }
        return;
    }
    let list: Vec<String> = included
        .iter()
        .map(|e| e.path.display().to_string())
        .collect();
    let fixes: Vec<String> = included
        .iter()
        .map(|e| format!("tmutil addexclusion {}", shell_quote(&e.path)))
        .collect();
    say(
        Level::Warn,
        format!(
            "Time Machine backs up {} of them (created before cargo could exclude them):\n{}\n\
             Fix (a sticky exclusion; no sudo needed):\n{}",
            included.len(),
            list.join("\n"),
            fixes.join("\n")
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tmutil_isexcluded() {
        let text = "[Excluded]  /Users/me/p/target\n\
                    [Included]  /Users/me/q/build dir\n\
                    [UNKNOWN]   /nonexistent\n";
        assert_eq!(
            parse_isexcluded(text),
            [
                Exclusion {
                    path: "/Users/me/p/target".into(),
                    excluded: true
                },
                Exclusion {
                    path: "/Users/me/q/build dir".into(),
                    excluded: false
                },
            ]
        );
    }
}
