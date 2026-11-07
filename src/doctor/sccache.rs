//! sccache: whether it's installed, where its cache is and how big, and
//! whether projects split it into separate caches.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{Level, cmd, say};
use crate::config;
use crate::fsutil::{self, human};
use crate::wrapper;

/// The parts of `sccache --show-stats` doctor reports.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    /// `Local disk: "/path"`, `S3, bucket: ...`, and so on.
    pub location: Option<String>,
    /// The directory, when the cache is on local disk.
    pub local_dir: Option<PathBuf>,
    pub size: Option<String>,
    pub max_size: Option<String>,
}

/// Parses the text form of `sccache --show-stats`, whose lines are a label
/// and a value separated by a run of spaces:
///
/// ```text
/// Cache location                  Local disk: "/Users/me/Library/Caches/Mozilla.sccache"
/// Cache size                           2 GiB
/// Max cache size                      10 GiB
/// ```
pub fn parse_stats(text: &str) -> Stats {
    let mut stats = Stats::default();
    for line in text.lines() {
        let Some((label, value)) = line.split_once("  ") else {
            continue;
        };
        let value = value.trim();
        match label.trim() {
            "Cache location" => {
                stats.local_dir = value
                    .strip_prefix("Local disk:")
                    .map(|p| PathBuf::from(p.trim().trim_matches('"')));
                stats.location = Some(value.to_owned());
            }
            "Cache size" => stats.size = Some(value.to_owned()),
            "Max cache size" => stats.max_size = Some(value.to_owned()),
            _ => {}
        }
    }
    stats
}

/// What doctor learned from sccache itself.
#[derive(Debug, Default)]
pub struct Probe {
    pub program: Option<PathBuf>,
    pub stats: Option<Stats>,
    /// The local cache directory's size on disk, when sccache didn't say.
    pub measured: Option<u64>,
}

/// Finds sccache and asks it for its statistics (which starts its server if
/// it isn't running).
pub fn probe() -> Probe {
    let Some(program) = wrapper::find_program("sccache".as_ref()) else {
        return Probe::default();
    };
    let stats = cmd::stdout(Command::new(&program).arg("--show-stats")).map(|t| parse_stats(&t));
    let measured = stats
        .as_ref()
        .filter(|s| s.size.is_none())
        .and_then(|s| s.local_dir.as_deref())
        .filter(|d| d.is_dir())
        .map(fsutil::tree_size);
    Probe {
        program: Some(program),
        stats,
        measured,
    }
}

/// The `SCCACHE_DIR` a Cargo config file sets in its `[env]` table, as a
/// string or as `{ value = "...", relative = true }`. `base` is the
/// directory relative values are resolved against: the one holding `.cargo`.
pub fn env_sccache_dir(config_text: &str, base: &Path) -> Option<PathBuf> {
    let doc: toml::Table = config_text.parse().ok()?;
    let entry = doc.get("env")?.get("SCCACHE_DIR")?;
    let (value, relative) = match entry {
        toml::Value::String(s) => (s.as_str(), false),
        toml::Value::Table(t) => (
            t.get("value")?.as_str()?,
            t.get("relative")
                .and_then(toml::Value::as_bool)
                .unwrap_or(false),
        ),
        _ => return None,
    };
    let path = PathBuf::from(value);
    Some(if relative || path.is_relative() {
        base.join(path)
    } else {
        path
    })
}

/// Project Cargo configs at or above each of `dirs` that set their own
/// `SCCACHE_DIR`: the cache directory, and the config files that choose it.
pub fn project_dirs(dirs: &BTreeSet<PathBuf>) -> BTreeMap<PathBuf, BTreeSet<PathBuf>> {
    let user = config::cargo_home().map(|h| h.join("config.toml"));
    let mut found: BTreeMap<PathBuf, BTreeSet<PathBuf>> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for dir in dirs {
        for ancestor in dir.ancestors() {
            for name in ["config.toml", "config"] {
                let file = ancestor.join(".cargo").join(name);
                if Some(&file) == user.as_ref() || !seen.insert(file.clone()) {
                    continue;
                }
                let Ok(text) = fs::read_to_string(&file) else {
                    continue;
                };
                if let Some(cache) = env_sccache_dir(&text, ancestor) {
                    found.entry(cache).or_default().insert(file);
                }
            }
        }
    }
    found
}

/// The `SCCACHE_DIR` the user-wide Cargo config sets for every build.
fn user_config_dir() -> Option<PathBuf> {
    let home = config::cargo_home()?;
    let text = fs::read_to_string(home.join("config.toml")).ok()?;
    env_sccache_dir(&text, home.parent()?)
}

pub fn report(probe: &Probe, workspaces: &BTreeSet<PathBuf>) {
    let Some(program) = &probe.program else {
        say(
            Level::Info,
            "sccache isn't installed. It shares compiled dependencies between build directories\n\
             (worktrees, toolchains' rebuilds); cargo-steward runs it as the inner wrapper if you\n\
             install it (`cargo install sccache`) and run `cargo steward setup --inner sccache`.",
        );
        return;
    };
    let size_env = env::var("SCCACHE_CACHE_SIZE")
        .ok()
        .filter(|v| !v.is_empty());
    match &probe.stats {
        Some(stats) => {
            let size = stats
                .size
                .clone()
                .or_else(|| probe.measured.map(human))
                .unwrap_or_else(|| "size unknown".to_owned());
            say(
                Level::Ok,
                format!(
                    "sccache ({}): {}\n{size} used of {} (the running server's limit).\n\
                     SCCACHE_CACHE_SIZE here: {}",
                    program.display(),
                    stats
                        .location
                        .as_deref()
                        .unwrap_or("cache location unknown"),
                    stats.max_size.as_deref().unwrap_or("an unknown maximum"),
                    size_env
                        .as_deref()
                        .unwrap_or("unset (sccache's default is 10 GiB)")
                ),
            );
            if let (Some(server), Some(here)) = (
                &stats.local_dir,
                env::var_os("SCCACHE_DIR").filter(|v| !v.is_empty()),
            ) && Path::new(&here) != server
            {
                say(
                    Level::Warn,
                    format!(
                        "SCCACHE_DIR here is {}, but the running sccache server uses {}:\n\
                         the first build to start the server decides its cache.\n\
                         Fix: `sccache --stop-server`, then build from an environment with the SCCACHE_DIR you want",
                        Path::new(&here).display(),
                        server.display()
                    ),
                );
            }
        }
        None => say(
            Level::Warn,
            format!(
                "sccache is installed at {}, but `sccache --show-stats` failed or timed out",
                program.display()
            ),
        ),
    }

    let projects = project_dirs(workspaces);
    let mut caches: BTreeSet<PathBuf> = projects.keys().cloned().collect();
    let shared = env::var_os("SCCACHE_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(user_config_dir)
        .or_else(|| probe.stats.as_ref().and_then(|s| s.local_dir.clone()));
    caches.extend(shared.clone());
    if projects.is_empty() || caches.len() < 2 {
        return;
    }
    let mut lines = Vec::new();
    for (cache, files) in &projects {
        for file in files {
            lines.push(format!("{}  (set in {})", cache.display(), file.display()));
        }
    }
    let suggested = probe
        .stats
        .as_ref()
        .and_then(|s| s.max_size.as_deref())
        .and_then(|m| config::parse_size(m).ok())
        .map_or_else(
            || "a larger size".to_owned(),
            |max| format!("about {}", human(max * caches.len() as u64)),
        );
    say(
        Level::Warn,
        format!(
            "{} sccache directories are in use, so dependency builds aren't shared between them:\n{}\n\
             sccache runs one server per port, and the first build to start it decides its cache,\n\
             so per-project SCCACHE_DIR settings either split the cache or are silently ignored.\n\
             Fix: remove SCCACHE_DIR from those configs, set it once (shell profile, or [env] in\n\
             ~/.cargo/config.toml){}, and raise SCCACHE_CACHE_SIZE to {suggested}.",
            caches.len(),
            lines.join("\n"),
            shared
                .map(|s| format!(" such as {}", s.display()))
                .unwrap_or_default(),
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sccache_stats() {
        let text = "Compile requests                    257\n\
                    Cache hits rate                       -\n\
                    \n\
                    Non-cacheable reasons:\n\
                    crate-type (bin)                    136\n\
                    \n\
                    Cache location                  Local disk: \"/Volumes/Scratch/sccache\"\n\
                    Use direct/preprocessor mode?   yes\n\
                    Version (client)                0.19.0\n\
                    Cache size                           3 GiB\n\
                    Max cache size                      10 GiB\n";
        let stats = parse_stats(text);
        assert_eq!(
            stats.location.as_deref(),
            Some("Local disk: \"/Volumes/Scratch/sccache\"")
        );
        assert_eq!(
            stats.local_dir,
            Some(PathBuf::from("/Volumes/Scratch/sccache"))
        );
        assert_eq!(stats.size.as_deref(), Some("3 GiB"));
        assert_eq!(stats.max_size.as_deref(), Some("10 GiB"));

        let remote = parse_stats("Cache location                  S3, bucket: Bucket(name=b)\n");
        assert_eq!(remote.local_dir, None);
        assert!(remote.location.unwrap().starts_with("S3"));
        assert_eq!(parse_stats(""), Stats::default());
    }

    #[test]
    fn reads_sccache_dir_from_cargo_env() {
        let base = Path::new("/w/p");
        assert_eq!(
            env_sccache_dir("[env]\nSCCACHE_DIR = \"/cache/p\"\n", base),
            Some(PathBuf::from("/cache/p"))
        );
        assert_eq!(
            env_sccache_dir(
                "[env]\nSCCACHE_DIR = { value = \".sccache\", relative = true }\n",
                base
            ),
            Some(PathBuf::from("/w/p/.sccache"))
        );
        assert_eq!(env_sccache_dir("[build]\njobs = 4\n", base), None);
        assert_eq!(env_sccache_dir("not toml [", base), None);
    }

    #[test]
    fn finds_project_sccache_dirs_above_packages() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("p");
        fs::create_dir_all(project.join(".cargo")).unwrap();
        fs::create_dir_all(project.join("crates/a")).unwrap();
        fs::write(
            project.join(".cargo/config.toml"),
            "[env]\nSCCACHE_DIR = \"/cache/p\"\n",
        )
        .unwrap();
        let found = project_dirs(&BTreeSet::from([project.join("crates/a"), project.clone()]));
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[Path::new("/cache/p")],
            BTreeSet::from([project.join(".cargo/config.toml")])
        );
    }
}
