//! Installed rustup toolchains: which ones leak stale object files, and
//! which ones no recorded project uses.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

use super::{Level, Report, cmd};
use crate::fsutil::{self, human};

/// A toolchain's rustc version: minor version, channel, and commit date.
#[derive(Debug, Clone, PartialEq)]
pub struct RustcVersion {
    minor: u32,
    nightly: bool,
    date: Option<String>,
}

impl RustcVersion {
    /// Parses `rustc 1.101.0-nightly (c980177d8 2026-11-06)`.
    pub fn parse(text: &str) -> Option<RustcVersion> {
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
    pub fn leaks(&self) -> bool {
        match self.minor {
            ..88 => false,
            88..=100 => true,
            101 if self.nightly => self.date.as_deref().is_none_or(|d| d < "2026-11-04"),
            _ => false,
        }
    }
}

/// One line of `rustup toolchain list`.
#[derive(Debug, Clone, PartialEq)]
pub struct Installed {
    /// Full name, such as `1.95.0-aarch64-apple-darwin`.
    pub name: String,
    /// The name without its host triple, as projects pin it: `1.95.0`.
    pub channel: String,
    pub default: bool,
    pub active: bool,
}

/// Parses `rustup toolchain list`:
///
/// ```text
/// stable-aarch64-apple-darwin (active, default)
/// 1.95.0-aarch64-apple-darwin
/// ```
pub fn parse_list(text: &str) -> Vec<Installed> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let (name, marks) = match line.split_once(' ') {
                Some((name, marks)) => (name, marks),
                None => (line, ""),
            };
            if name.is_empty() || name == "no" {
                // "no installed toolchains"
                return None;
            }
            Some(Installed {
                name: name.to_owned(),
                channel: channel_of(name),
                default: marks.contains("default"),
                active: marks.contains("active"),
            })
        })
        .collect()
}

/// Architectures that start a target triple, telling a host suffix apart
/// from the rest of a custom toolchain's name.
const ARCHES: [&str; 11] = [
    "x86_64",
    "aarch64",
    "arm",
    "i686",
    "i586",
    "riscv",
    "powerpc",
    "s390x",
    "loongarch",
    "mips",
    "sparc",
];

/// A toolchain name without its host triple: `1.95.0-aarch64-apple-darwin`
/// is `1.95.0`, `nightly-2026-10-12-x86_64-unknown-linux-gnu` is
/// `nightly-2026-10-12`. Names without one (custom, linked toolchains) are
/// returned whole.
pub fn channel_of(name: &str) -> String {
    let parts: Vec<&str> = name.split('-').collect();
    let mut keep = 1;
    let is_date = |p: &[&str]| {
        p.len() >= 3
            && p[0].len() == 4
            && p[1].len() == 2
            && p[2].len() == 2
            && p[..3].iter().all(|s| s.bytes().all(|b| b.is_ascii_digit()))
    };
    if parts.len() > 1 && is_date(&parts[1..]) {
        keep = 4;
    }
    let rest = &parts[keep.min(parts.len())..];
    let triple =
        rest.is_empty() || (rest.len() >= 2 && ARCHES.iter().any(|a| rest[0].starts_with(a)));
    if triple {
        parts[..keep.min(parts.len())].join("-")
    } else {
        name.to_owned()
    }
}

/// Parses `rustup override list`: `<directory><whitespace><toolchain>` per
/// line, or `no overrides`.
pub fn parse_overrides(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.split_whitespace().last())
        .filter(|t| *t != "overrides")
        .map(channel_of)
        .collect()
}

/// What `rustup` reports about installed toolchains, gathered in parallel.
#[derive(Debug, Default)]
pub struct Probe {
    pub installed: Vec<Installed>,
    pub versions: Vec<Option<RustcVersion>>,
    /// Size on disk of each toolchain; `None` for linked ones.
    pub sizes: Vec<Option<u64>>,
    pub overrides: Vec<String>,
    pub toolchains_dir: PathBuf,
}

/// `$RUSTUP_HOME`, defaulting to `~/.rustup` as rustup does.
pub fn rustup_home() -> Option<PathBuf> {
    env::var_os("RUSTUP_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".rustup")))
}

/// Lists toolchains, then asks each for its version and measures it, all at
/// once. `None` when rustup isn't installed.
pub fn probe() -> Option<Probe> {
    let list = cmd::stdout(Command::new("rustup").args(["toolchain", "list"]))?;
    let installed = parse_list(&list);
    let toolchains_dir = rustup_home().unwrap_or_default().join("toolchains");
    let (versions, sizes, overrides) = thread::scope(|s| {
        let overrides = s.spawn(|| {
            cmd::stdout(Command::new("rustup").args(["override", "list"]))
                .map(|t| parse_overrides(&t))
                .unwrap_or_default()
        });
        let jobs: Vec<_> = installed
            .iter()
            .map(|t| {
                let dir = toolchains_dir.join(&t.name);
                (
                    s.spawn(move || rustc_version(&t.name)),
                    s.spawn(move || toolchain_size(&dir)),
                )
            })
            .collect();
        let (versions, sizes): (Vec<_>, Vec<_>) = jobs
            .into_iter()
            .map(|(v, n)| (v.join().ok().flatten(), n.join().ok().flatten()))
            .unzip();
        (versions, sizes, overrides.join().unwrap_or_default())
    });
    Some(Probe {
        installed,
        versions,
        sizes,
        overrides,
        toolchains_dir,
    })
}

fn rustc_version(toolchain: &str) -> Option<RustcVersion> {
    let text = cmd::stdout(Command::new("rustup").args(["run", toolchain, "rustc", "-V"]))?;
    RustcVersion::parse(text.trim())
}

/// A toolchain directory's size; `None` for a linked (custom) toolchain,
/// whose files aren't rustup's to remove.
fn toolchain_size(dir: &Path) -> Option<u64> {
    let meta = fs::symlink_metadata(dir).ok()?;
    (!meta.file_type().is_symlink()).then(|| fsutil::tree_size(dir))
}

/// Installed toolchains that aren't the default or active one, aren't a
/// directory override, and that no project in `pins` asks for.
pub fn unused<'a>(
    probe: &'a Probe,
    pins: &BTreeMap<String, BTreeSet<PathBuf>>,
) -> Vec<(&'a Installed, Option<u64>)> {
    let pinned: BTreeSet<String> = pins
        .keys()
        .map(|p| channel_of(p))
        .chain(probe.overrides.iter().cloned())
        .collect();
    probe
        .installed
        .iter()
        .zip(&probe.sizes)
        .filter(|(t, _)| !t.default && !t.active && !pinned.contains(&t.channel))
        .map(|(t, size)| (t, *size))
        .collect()
}

pub fn report(
    out: &mut Report,
    probe: Option<&Probe>,
    pins: &BTreeMap<String, BTreeSet<PathBuf>>,
    projects: usize,
) {
    let Some(probe) = probe else {
        out.say(
            Level::Info,
            "rustup isn't installed; skipping the toolchain checks",
        );
        return;
    };
    let mut leaking = Vec::new();
    let mut fixed = Vec::new();
    for (toolchain, version) in probe.installed.iter().zip(&probe.versions) {
        match version {
            Some(v) if v.leaks() => leaking.push(toolchain.name.as_str()),
            Some(_) => fixed.push(toolchain.name.as_str()),
            None => {}
        }
    }
    if !leaking.is_empty() {
        out.say(
            Level::Info,
            format!(
                "{} of {} installed toolchains leave stale object files behind (Rust 1.88 to 1.100):\n{}\n\
                 cargo-steward's wrapper removes them as builds run.",
                leaking.len(),
                probe.installed.len(),
                leaking.join(", ")
            ),
        );
    }
    if !fixed.is_empty() {
        out.say(Level::Ok, format!("Not affected: {}", fixed.join(", ")));
    }

    let total: u64 = probe.sizes.iter().flatten().sum();
    if projects == 0 {
        out.say(
            Level::Info,
            format!(
                "{} toolchains take {} in {}; ones no project uses are reported once projects\n\
                 have built through cargo-steward",
                probe.installed.len(),
                human(total),
                probe.toolchains_dir.display()
            ),
        );
        return;
    }
    let unused = unused(probe, pins);
    if unused.is_empty() {
        out.say(
            Level::Ok,
            format!(
                "Every installed toolchain is the default, active, or pinned by a recorded project ({} in {})",
                human(total),
                probe.toolchains_dir.display()
            ),
        );
        return;
    }
    let reclaim: u64 = unused.iter().filter_map(|(_, s)| *s).sum();
    let width = unused.iter().map(|(t, _)| t.name.len()).max().unwrap_or(0);
    let lines: Vec<String> = unused
        .iter()
        .map(|(t, size)| {
            format!(
                "{:<width$}  {:>9}",
                t.name,
                size.map_or_else(|| "linked".to_owned(), human)
            )
        })
        .collect();
    let removable: Vec<&str> = unused
        .iter()
        .filter(|(_, size)| size.is_some())
        .map(|(t, _)| t.name.as_str())
        .collect();
    out.say(
        Level::Info,
        format!(
            "{} of {} installed toolchains ({} of {}) aren't the default, active, or pinned by any of\n\
             {projects} recorded project(s). Projects that haven't built through cargo-steward, and\n\
             `cargo +toolchain` commands, aren't seen; check before removing.\n{}\n\
             To remove: rustup toolchain uninstall {}",
            unused.len(),
            probe.installed.len(),
            human(reclaim),
            human(total),
            lines.join("\n"),
            removable.join(" ")
        ),
    );
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
    fn strips_host_triples_from_toolchain_names() {
        assert_eq!(channel_of("stable-aarch64-apple-darwin"), "stable");
        assert_eq!(channel_of("1.95.0-aarch64-apple-darwin"), "1.95.0");
        assert_eq!(channel_of("1.95-x86_64-unknown-linux-gnu"), "1.95");
        assert_eq!(
            channel_of("nightly-2026-10-12-aarch64-apple-darwin"),
            "nightly-2026-10-12"
        );
        assert_eq!(channel_of("nightly-2026-10-12"), "nightly-2026-10-12");
        assert_eq!(channel_of("1.99.0"), "1.99.0");
        assert_eq!(channel_of("beta-armv7-unknown-linux-gnueabihf"), "beta");
        // Custom linked toolchains keep their whole name.
        assert_eq!(channel_of("stage1"), "stage1");
        assert_eq!(channel_of("my-build"), "my-build");
    }

    #[test]
    fn parses_toolchain_lists() {
        let list = "stable-aarch64-apple-darwin (active, default)\n\
                    nightly-aarch64-apple-darwin\n\
                    1.95.0-aarch64-apple-darwin\n\
                    beta-aarch64-apple-darwin (default)\n\
                    stage1\n";
        let parsed = parse_list(list);
        assert_eq!(parsed.len(), 5);
        assert_eq!(parsed[0].channel, "stable");
        assert!(parsed[0].active && parsed[0].default);
        assert!(!parsed[1].active && !parsed[1].default);
        assert_eq!(parsed[2].channel, "1.95.0");
        assert!(parsed[3].default && !parsed[3].active);
        assert_eq!(parsed[4].channel, "stage1");
        assert!(parse_list("no installed toolchains\n").is_empty());
    }

    #[test]
    fn parses_override_lists() {
        assert!(parse_overrides("no overrides\n").is_empty());
        assert_eq!(
            parse_overrides(
                "/home/u/old-project                     1.80.0-x86_64-unknown-linux-gnu\n"
            ),
            ["1.80.0"]
        );
    }

    #[test]
    fn finds_toolchains_nothing_uses() {
        let probe = Probe {
            installed: parse_list(
                "stable-aarch64-apple-darwin (active, default)\n\
                 nightly-aarch64-apple-darwin\n\
                 1.95.0-aarch64-apple-darwin\n\
                 1.95-aarch64-apple-darwin\n\
                 1.80.0-aarch64-apple-darwin\n\
                 nightly-2026-10-12-aarch64-apple-darwin\n",
            ),
            versions: vec![None; 6],
            sizes: vec![Some(1), Some(2), Some(3), Some(4), Some(5), Some(6)],
            overrides: vec!["1.80.0".into()],
            toolchains_dir: PathBuf::new(),
        };
        let pins = BTreeMap::from([
            ("1.95.0".to_owned(), BTreeSet::from([PathBuf::from("/p")])),
            (
                "nightly-2026-10-12-aarch64-apple-darwin".to_owned(),
                BTreeSet::from([PathBuf::from("/q")]),
            ),
        ]);
        let names: Vec<(&str, Option<u64>)> = unused(&probe, &pins)
            .into_iter()
            .map(|(t, s)| (t.name.as_str(), s))
            .collect();
        // `1.95` and `1.95.0` are different toolchains to rustup.
        assert_eq!(
            names,
            [
                ("nightly-aarch64-apple-darwin", Some(2)),
                ("1.95-aarch64-apple-darwin", Some(4)),
            ]
        );
    }
}
