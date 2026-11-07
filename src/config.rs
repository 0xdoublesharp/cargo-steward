//! Settings, where cargo-steward keeps its files, and parsers for the sizes
//! and durations used in settings.

use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Where cargo-steward keeps its settings and bookkeeping:
/// `$CARGO_STEWARD_HOME`, else `$CARGO_HOME/steward`, else `~/.cargo/steward`.
#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
}

impl Paths {
    pub fn resolve() -> Option<Paths> {
        if let Some(dir) = env::var_os("CARGO_STEWARD_HOME").filter(|v| !v.is_empty()) {
            return Some(Paths { home: dir.into() });
        }
        Some(Paths {
            home: cargo_home()?.join("steward"),
        })
    }

    pub fn config(&self) -> PathBuf {
        self.home.join("config.toml")
    }

    /// Append-only record of compiles, written by the wrapper.
    pub fn ledger(&self) -> PathBuf {
        self.home.join("ledger.jsonl")
    }

    /// The ledger, compacted per build directory.
    pub fn registry(&self) -> PathBuf {
        self.home.join("registry.json")
    }

    /// Modified whenever a cleanup starts; its age decides when the next is due.
    pub fn stamp(&self) -> PathBuf {
        self.home.join("gc.stamp")
    }

    /// Held by the running cleanup so only one runs at a time.
    pub fn gc_lock(&self) -> PathBuf {
        self.home.join("gc.lock")
    }

    /// Output of automatic cleanups.
    pub fn log(&self) -> PathBuf {
        self.home.join("gc.log")
    }
}

/// `$CARGO_HOME`, defaulting to `~/.cargo` as cargo does.
pub fn cargo_home() -> Option<PathBuf> {
    env::var_os("CARGO_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
}

/// The settings file, `config.toml` in [`Paths::home`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    /// Wrapper that rustc runs through, such as `sccache`.
    pub inner_wrapper: Option<String>,
    /// Remove the debug object files that each rebuild leaves behind.
    pub leak_fix: bool,
    /// Start a cleanup in the background after builds when one is due.
    pub auto_gc: bool,
    /// How often an automatic cleanup is due.
    pub gc_interval: String,
    /// Directory trees searched for build directories, in addition to the
    /// ones the wrapper has seen.
    pub scan_roots: Vec<PathBuf>,
    /// Remove a build unit (new layout) that no build has used for this long.
    pub unit_idle: String,
    /// Remove a crate's incremental cache that no build has used for this long.
    pub incremental_idle: String,
    /// Remove the old-layout files of a build directory once the new layout
    /// is in use and no old-layout build has run for this long.
    pub legacy_idle: String,
    /// Remove a whole build directory that no build has used for this long.
    pub dir_idle: String,
    /// Remove a build directory this long after every workspace that built
    /// into it was deleted.
    pub orphan_grace: String,
    /// Never remove a whole build directory used more recently than this,
    /// even when the disk is low on space.
    pub recent_guard: String,
    /// When a disk has less free space than this, remove the least recently
    /// used build directories on it...
    pub min_free: String,
    /// ...until it has this much free again.
    pub target_free: String,
    /// `setup` set `build.build-dir`, so `setup --undo` removes it.
    pub managed_build_dir: bool,
    /// Keep replaced object files this long, so binaries that are still
    /// running keep readable backtraces.
    pub object_grace: String,
    /// Before removing a whole build directory, check that no running
    /// process was started from it.
    pub process_guard: bool,
    /// Start a new worktree's build directory as a clone of its repository's
    /// main build directory (copy-on-write; APFS, btrfs, XFS).
    pub seed: bool,
    /// Let cleanup replace identical files across build directories with
    /// copy-on-write clones.
    pub dedup: bool,
    /// For the `cargo` proxy: put each worktree's target directory under
    /// this root, as `<root>/<repo>-<hash>/<worktree>`.
    pub target_root: Option<PathBuf>,
    /// For the `cargo` proxy: point TMPDIR for builds at a per-worktree
    /// directory under this root.
    pub tmp_root: Option<PathBuf>,
    /// For the `cargo` proxy: run at most this many cargo builds at once.
    pub max_builds: Option<u32>,
    /// For the `cargo` proxy: the real cargo to run. Found on PATH if unset.
    pub real_cargo: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            inner_wrapper: None,
            leak_fix: true,
            auto_gc: false,
            gc_interval: "1h".into(),
            scan_roots: Vec::new(),
            unit_idle: "14d".into(),
            incremental_idle: "7d".into(),
            legacy_idle: "3d".into(),
            dir_idle: "21d".into(),
            orphan_grace: "2d".into(),
            recent_guard: "1d".into(),
            min_free: "50GiB".into(),
            target_free: "100GiB".into(),
            managed_build_dir: false,
            object_grace: "10m".into(),
            process_guard: true,
            seed: true,
            dedup: true,
            target_root: None,
            tmp_root: None,
            max_builds: None,
            real_cargo: None,
        }
    }
}

/// [`Config`]'s thresholds, parsed.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub gc_interval: Duration,
    pub unit_idle: Duration,
    pub incremental_idle: Duration,
    pub legacy_idle: Duration,
    pub dir_idle: Duration,
    pub orphan_grace: Duration,
    pub recent_guard: Duration,
    pub min_free: u64,
    pub target_free: u64,
    #[allow(dead_code)] // Read by the object-grace work in the wrapper and cleanup.
    pub object_grace: Duration,
}

impl Config {
    pub fn policy(&self) -> Result<Policy> {
        let duration = |key: &str, value: &str| {
            humantime::parse_duration(value.trim()).with_context(|| format!("{key} = {value:?}"))
        };
        let size = |key: &str, value: &str| {
            parse_size(value).with_context(|| format!("{key} = {value:?}"))
        };
        let policy = Policy {
            gc_interval: duration("gc-interval", &self.gc_interval)?,
            unit_idle: duration("unit-idle", &self.unit_idle)?,
            incremental_idle: duration("incremental-idle", &self.incremental_idle)?,
            legacy_idle: duration("legacy-idle", &self.legacy_idle)?,
            dir_idle: duration("dir-idle", &self.dir_idle)?,
            orphan_grace: duration("orphan-grace", &self.orphan_grace)?,
            recent_guard: duration("recent-guard", &self.recent_guard)?,
            min_free: size("min-free", &self.min_free)?,
            target_free: size("target-free", &self.target_free)?,
            object_grace: duration("object-grace", &self.object_grace)?,
        };
        if policy.target_free < policy.min_free {
            bail!("target-free must be at least min-free");
        }
        Ok(policy)
    }
}

/// Reads the settings file. A missing file means the defaults.
pub fn load(paths: &Paths) -> Result<Config> {
    let path = paths.config();
    match fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("reading {}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn save(paths: &Paths, config: &Config) -> Result<()> {
    fs::create_dir_all(&paths.home)
        .with_context(|| format!("creating {}", paths.home.display()))?;
    let text = toml::to_string_pretty(config)?;
    write_atomic(&paths.config(), text.as_bytes())
}

/// Writes through a temporary file and a rename, so readers never see a
/// partial file.
pub fn write_atomic(path: &std::path::Path, contents: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// Parses sizes like `50GiB`, `100G`, `512MiB` or `1024`. Units are binary
/// whether or not they say so.
pub fn parse_size(text: &str) -> Result<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let value: f64 = number.parse().context("expected a number")?;
    let multiplier: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        other => bail!("unknown unit {other:?}"),
    };
    Ok((value * multiplier as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("50GiB").unwrap(), 50 << 30);
        assert_eq!(parse_size(" 1.5 G ").unwrap(), 3 << 29);
        assert_eq!(parse_size("2tb").unwrap(), 2 << 40);
        assert!(parse_size("ten").is_err());
        assert!(parse_size("5 parsecs").is_err());
    }

    #[test]
    fn defaults_parse_and_round_trip() {
        let config = Config::default();
        config.policy().unwrap();
        let text = toml::to_string_pretty(&config).unwrap();
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), config);
    }

    #[test]
    fn partial_files_keep_the_other_defaults() {
        let config: Config = toml::from_str("auto-gc = true\ndir-idle = \"7d\"\n").unwrap();
        assert!(config.auto_gc);
        assert_eq!(config.dir_idle, "7d");
        assert_eq!(config.unit_idle, Config::default().unit_idle);
    }

    #[test]
    fn rejects_inverted_free_space_thresholds() {
        let config = Config {
            min_free: "100G".into(),
            target_free: "50G".into(),
            ..Config::default()
        };
        assert!(config.policy().is_err());
    }
}
