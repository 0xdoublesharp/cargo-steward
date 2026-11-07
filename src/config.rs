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
    /// For the `cargo` proxy: when it routes a workspace's build output, also
    /// point a missing `<workspace>/target` at it (when git ignores `target`),
    /// so scripts expecting `./target` and builds that bypass the proxy, such
    /// as an IDE's, land in the same place. Not for forced routes.
    pub link_target: bool,
    /// For the `cargo` proxy: settings for commands run under particular
    /// directories, overriding `target-root`, `tmp-root` and `max-builds`.
    /// The longest matching `path` wins.
    pub routes: Vec<Route>,
}

/// Proxy settings for commands run under one directory tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Route {
    /// Applies to commands whose directory is this one or below it.
    pub path: PathBuf,
    /// As the top-level `target-root`; unset routes no target directory.
    #[serde(default)]
    pub target_root: Option<PathBuf>,
    /// As the top-level `tmp-root`.
    #[serde(default)]
    pub tmp_root: Option<PathBuf>,
    /// As the top-level `max-builds`, with its own set of slots.
    #[serde(default)]
    pub max_builds: Option<u32>,
    /// Route even when `CARGO_TARGET_DIR` is already set or a workspace
    /// config sets `build.target-dir`, as a per-invocation shim that always
    /// sets the target dir would. Only `--target-dir` and `--config` on the
    /// command line still win.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
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
            routes: Vec::new(),
            link_target: false,
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

/// Reads the settings so that each one that's wrong falls back to its
/// default alone, and says what was wrong. For what must run whatever the
/// file holds (the wrapper and the proxy run every build) and for reports:
/// a typo in one setting no longer silently drops `inner-wrapper` (sccache)
/// and `auto-gc` with it. A file that isn't TOML at all gives the defaults.
pub fn load_lenient(paths: &Paths) -> (Config, Vec<String>) {
    let path = paths.config();
    match fs::read_to_string(&path) {
        Ok(text) => lenient(&text, &path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => (Config::default(), Vec::new()),
        Err(e) => (
            Config::default(),
            vec![format!(
                "reading {}: {e}; using the defaults",
                path.display()
            )],
        ),
    }
}

/// [`Config`] and its thresholds ([`Config::policy`]) from `table`.
fn parse_table(table: &toml::Table) -> std::result::Result<Config, String> {
    let config: Config = toml::Value::Table(table.clone())
        .try_into()
        .map_err(|e: toml::de::Error| e.message().to_owned())?;
    config.policy().map_err(|e| format!("{e:#}"))?;
    Ok(config)
}

fn lenient(text: &str, path: &std::path::Path) -> (Config, Vec<String>) {
    let table: toml::Table = match text.parse() {
        Ok(table) => table,
        Err(e) => {
            return (
                Config::default(),
                vec![format!(
                    "{} isn't valid TOML ({}); using the defaults",
                    path.display(),
                    e.message()
                )],
            );
        }
    };
    // Settings are taken one at a time, in rounds, so one that only fits
    // with another (`target-free` above a raised `min-free`) still counts.
    let mut accepted = toml::Table::new();
    let mut config = Config::default();
    let mut pending: Vec<(String, toml::Value, String)> = table
        .into_iter()
        .map(|(key, value)| (key, value, String::new()))
        .collect();
    loop {
        let before = pending.len();
        pending.retain_mut(|(key, value, error)| {
            let mut candidate = accepted.clone();
            candidate.insert(key.clone(), value.clone());
            match parse_table(&candidate) {
                Ok(parsed) => {
                    accepted = candidate;
                    config = parsed;
                    false
                }
                Err(e) => {
                    *error = e;
                    true
                }
            }
        });
        if pending.is_empty() || pending.len() == before {
            break;
        }
    }
    let mut problems: Vec<String> = pending
        .into_iter()
        .map(|(key, _, error)| format!("{key}: {error}; using the default"))
        .collect();
    // A key that changes nothing and isn't one cargo-steward writes back is
    // one it doesn't know: most likely a typo.
    let known: std::collections::BTreeSet<String> = toml::Value::try_from(&config)
        .ok()
        .and_then(|v| v.as_table().map(|t| t.keys().cloned().collect()))
        .unwrap_or_default();
    for key in accepted.keys().filter(|k| !known.contains(*k)) {
        let mut without = accepted.clone();
        without.remove(key);
        if parse_table(&without).is_ok_and(|c| c == config) {
            problems.push(format!("{key}: not a cargo-steward setting; ignored"));
        }
    }
    (config, problems)
}

/// [`load_lenient`], printing what was wrong as warnings.
pub fn load_warning(paths: &Paths) -> Config {
    let (config, problems) = load_lenient(paths);
    for problem in problems {
        eprintln!("warning: cargo-steward: settings: {problem}");
    }
    config
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
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing {}", path.display()));
    }
    Ok(())
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

    #[test]
    fn keeps_every_file_in_the_home_directory() {
        let paths = Paths {
            home: PathBuf::from("/s"),
        };
        assert_eq!(paths.config(), PathBuf::from("/s/config.toml"));
        assert_eq!(paths.ledger(), PathBuf::from("/s/ledger.jsonl"));
        assert_eq!(paths.registry(), PathBuf::from("/s/registry.json"));
        assert_eq!(paths.stamp(), PathBuf::from("/s/gc.stamp"));
        assert_eq!(paths.gc_lock(), PathBuf::from("/s/gc.lock"));
        assert_eq!(paths.log(), PathBuf::from("/s/gc.log"));
    }

    #[test]
    fn saves_loads_and_reports_unreadable_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: tmp.path().join("steward"),
        };
        assert_eq!(load(&paths).unwrap(), Config::default());
        let config = Config {
            auto_gc: true,
            scan_roots: vec!["/v".into()],
            ..Config::default()
        };
        save(&paths, &config).unwrap();
        assert_eq!(load(&paths).unwrap(), config);
        // Only the settings file is left; the temporary one was renamed.
        let names: Vec<_> = fs::read_dir(&paths.home).unwrap().flatten().collect();
        assert_eq!(names.len(), 1);

        fs::write(paths.config(), "auto-gc = \"yes\"\n").unwrap();
        let err = format!("{:#}", load(&paths).unwrap_err());
        assert!(err.contains("reading"), "{err}");

        fs::remove_file(paths.config()).unwrap();
        fs::create_dir(paths.config()).unwrap();
        assert!(load(&paths).is_err(), "a directory isn't a missing file");
    }

    #[test]
    fn names_the_setting_that_doesnt_parse() {
        let config = Config {
            dir_idle: "soon".into(),
            ..Config::default()
        };
        let err = format!("{:#}", config.policy().unwrap_err());
        assert!(err.contains("dir-idle = \"soon\""), "{err}");
        let config = Config {
            min_free: "lots".into(),
            ..Config::default()
        };
        let err = format!("{:#}", config.policy().unwrap_err());
        assert!(err.contains("min-free"), "{err}");

        let bad = || "never".to_string();
        type Set = fn(&mut Config, String);
        let cases: [(&str, Set); 8] = [
            ("gc-interval", |c, v| c.gc_interval = v),
            ("unit-idle", |c, v| c.unit_idle = v),
            ("incremental-idle", |c, v| c.incremental_idle = v),
            ("legacy-idle", |c, v| c.legacy_idle = v),
            ("orphan-grace", |c, v| c.orphan_grace = v),
            ("recent-guard", |c, v| c.recent_guard = v),
            ("target-free", |c, v| c.target_free = v),
            ("object-grace", |c, v| c.object_grace = v),
        ];
        for (key, set) in cases {
            let mut config = Config::default();
            set(&mut config, bad());
            let err = format!("{:#}", config.policy().unwrap_err());
            assert!(err.contains(&format!("{key} = \"never\"")), "{err}");
        }
    }

    #[test]
    fn parses_every_unit() {
        assert_eq!(parse_size("3b").unwrap(), 3);
        assert_eq!(parse_size("2k").unwrap(), 2 << 10);
        assert_eq!(parse_size("2KiB").unwrap(), 2 << 10);
        assert_eq!(parse_size("5mb").unwrap(), 5 << 20);
        assert_eq!(parse_size("1t").unwrap(), 1 << 40);
    }

    #[test]
    fn reports_settings_that_cant_be_written() {
        let tmp = tempfile::tempdir().unwrap();
        // The home is a file, so it can't be created as a directory.
        let file = tmp.path().join("file");
        fs::write(&file, "x").unwrap();
        let err = format!(
            "{:#}",
            save(&Paths { home: file.clone() }, &Config::default()).unwrap_err()
        );
        assert!(err.contains("creating"), "{err}");

        // No directory to write the temporary file in.
        let err = format!(
            "{:#}",
            write_atomic(&tmp.path().join("missing/config.toml"), b"x").unwrap_err()
        );
        assert!(err.contains("writing"), "{err}");

        // A non-empty directory where the file goes can't be replaced, and
        // the temporary file is still named in the error.
        let dir = tmp.path().join("config.toml");
        fs::create_dir_all(dir.join("inside")).unwrap();
        let err = format!("{:#}", write_atomic(&dir, b"x").unwrap_err());
        assert!(err.contains(&format!("writing {}", dir.display())), "{err}");
        assert!(dir.join("inside").is_dir());
        // The temporary file isn't left behind.
        let names: Vec<_> = fs::read_dir(tmp.path()).unwrap().flatten().collect();
        assert_eq!(names.len(), 2, "{names:?}");
    }

    fn lenient_str(text: &str) -> (Config, Vec<String>) {
        lenient(text, std::path::Path::new("config.toml"))
    }

    #[test]
    fn a_bad_setting_falls_back_alone() {
        let (config, problems) = lenient_str(
            "inner-wrapper = \"sccache\"\nauto-gc = \"yes\"\ndir-idle = \"soon\"\n\
             unit-idle = \"3d\"\n",
        );
        assert_eq!(config.inner_wrapper.as_deref(), Some("sccache"));
        assert_eq!(config.unit_idle, "3d");
        assert!(!config.auto_gc, "the default");
        assert_eq!(config.dir_idle, Config::default().dir_idle);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().any(|p| p.starts_with("auto-gc:")));
        assert!(problems.iter().any(|p| p.starts_with("dir-idle:")));
    }

    #[test]
    fn settings_that_only_fit_together_are_both_kept() {
        let (config, problems) = lenient_str("target-free = \"300G\"\nmin-free = \"200G\"\n");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(config.min_free, "200G");
        assert_eq!(config.target_free, "300G");
    }

    #[test]
    fn unknown_settings_and_broken_files_are_reported() {
        let (config, problems) = lenient_str("auto-gc = true\nmax_builds = 3\n");
        assert!(config.auto_gc);
        assert_eq!(config.max_builds, None);
        assert_eq!(
            problems,
            ["max_builds: not a cargo-steward setting; ignored"]
        );
        // A setting given its default value is still a known one.
        assert!(lenient_str("leak-fix = true\n").1.is_empty());

        let (config, problems) = lenient_str("auto-gc = \n");
        assert_eq!(config, Config::default());
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("isn't valid TOML"), "{problems:?}");

        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: tmp.path().to_path_buf(),
        };
        assert_eq!(load_lenient(&paths), (Config::default(), Vec::new()));
    }
}
