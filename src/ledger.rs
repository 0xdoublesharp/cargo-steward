//! What the wrapper saw: one line per workspace-member compile, appended to
//! `ledger.jsonl`, and compacted by cleanup into `registry.json` per build
//! directory. It tells cleanup when a build directory was last used and which
//! workspaces build into it, so it can tell when they have all been deleted.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{Paths, write_atomic};

/// The wrapper stops appending past this size until a cleanup compacts it.
const MAX_LEDGER: u64 = 16 << 20;

/// One compile of a workspace member.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// Unix seconds.
    pub ts: u64,
    /// The build directory (the directory holding cargo's `CACHEDIR.TAG`).
    pub root: PathBuf,
    /// The package's directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    /// How long the compile took.
    #[serde(default)]
    pub ms: u64,
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Appends a record. Best effort: bookkeeping must never fail a build.
pub fn append(paths: &Paths, record: &Record) {
    let path = paths.ledger();
    if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LEDGER) {
        return;
    }
    let Ok(mut line) = serde_json::to_vec(record) else {
        return;
    };
    line.push(b'\n');
    let _ = fs::create_dir_all(&paths.home);
    // One short write with O_APPEND, so concurrent compiles can't interleave.
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(&line);
    }
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Registry {
    pub roots: BTreeMap<PathBuf, RootInfo>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct RootInfo {
    /// Unix seconds of the last compile seen.
    pub last_used: u64,
    /// Packages that have built into this directory.
    pub workspaces: BTreeSet<PathBuf>,
    /// Total compile time seen, a rough measure of what rebuilding costs.
    pub compile_ms: u64,
}

impl RootInfo {
    /// Combines what is known about the same directory from two entries.
    pub fn absorb(&mut self, other: &RootInfo) {
        self.last_used = self.last_used.max(other.last_used);
        self.compile_ms = self.compile_ms.saturating_add(other.compile_ms);
        self.workspaces.extend(other.workspaces.iter().cloned());
    }
}

impl Registry {
    fn merge(&mut self, record: Record) {
        let info = self.roots.entry(record.root).or_default();
        info.last_used = info.last_used.max(record.ts);
        info.compile_ms = info.compile_ms.saturating_add(record.ms);
        if let Some(workspace) = record.workspace {
            info.workspaces.insert(workspace);
        }
    }
}

pub fn load(paths: &Paths) -> Registry {
    fs::read(paths.registry())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save(paths: &Paths, registry: &Registry) -> Result<()> {
    fs::create_dir_all(&paths.home)?;
    write_atomic(&paths.registry(), &serde_json::to_vec_pretty(registry)?)
}

/// Folds the ledger into the registry and empties it. The ledger is renamed
/// first, so compiles that finish meanwhile start a fresh one and nothing is
/// lost; leftovers from an interrupted compaction are picked up too.
pub fn compact(paths: &Paths) -> Result<Registry> {
    let mut registry = load(paths);
    let claimed = paths
        .home
        .join(format!("ledger.{}.processing", std::process::id()));
    match fs::rename(paths.ledger(), &claimed) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("claiming the ledger"),
    }
    let pending: Vec<PathBuf> = crate::fsutil::entries(&paths.home)
        .filter(|(name, _)| name.starts_with("ledger.") && name.ends_with(".processing"))
        .map(|(_, path)| path)
        .collect();
    if pending.is_empty() {
        return Ok(registry);
    }
    for path in &pending {
        let text = fs::read_to_string(path).unwrap_or_default();
        for record in text
            .lines()
            .filter_map(|l| serde_json::from_str::<Record>(l).ok())
        {
            registry.merge(record);
        }
    }
    save(paths, &registry)?;
    for path in pending {
        let _ = fs::remove_file(path);
    }
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compacts_records_per_root() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: tmp.path().to_path_buf(),
        };
        let root = PathBuf::from("/b/target");
        for (ts, ws) in [(10, Some("/w/a")), (30, Some("/w/b")), (20, None)] {
            append(
                &paths,
                &Record {
                    ts,
                    root: root.clone(),
                    workspace: ws.map(PathBuf::from),
                    ms: 5,
                },
            );
        }
        // A torn line is skipped, not fatal.
        fs::OpenOptions::new()
            .append(true)
            .open(paths.ledger())
            .unwrap()
            .write_all(b"{\"ts\":")
            .unwrap();

        let registry = compact(&paths).unwrap();
        let info = &registry.roots[&root];
        assert_eq!(info.last_used, 30);
        assert_eq!(info.compile_ms, 15);
        assert_eq!(info.workspaces.len(), 2);
        assert!(!paths.ledger().exists());

        // The registry persists, and later records merge into it.
        append(
            &paths,
            &Record {
                ts: 40,
                root: root.clone(),
                workspace: None,
                ms: 1,
            },
        );
        let registry = compact(&paths).unwrap();
        assert_eq!(registry.roots[&root].last_used, 40);
        assert_eq!(registry.roots[&root].compile_ms, 16);
    }

    #[test]
    fn absorb_keeps_the_latest_use_and_sums_compile_time() {
        let mut a = RootInfo {
            last_used: 10,
            workspaces: [PathBuf::from("/w/a")].into(),
            compile_ms: 5,
        };
        let b = RootInfo {
            last_used: 7,
            workspaces: [PathBuf::from("/w/b")].into(),
            compile_ms: u64::MAX,
        };
        a.absorb(&b);
        assert_eq!(a.last_used, 10);
        assert_eq!(a.compile_ms, u64::MAX);
        assert_eq!(a.workspaces.len(), 2);
    }

    #[test]
    fn append_stops_once_the_ledger_is_too_big() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: tmp.path().to_path_buf(),
        };
        let file = fs::File::create(paths.ledger()).unwrap();
        file.set_len(MAX_LEDGER + 1).unwrap();
        append(
            &paths,
            &Record {
                ts: 1,
                root: PathBuf::from("/x"),
                workspace: None,
                ms: 0,
            },
        );
        assert_eq!(fs::metadata(paths.ledger()).unwrap().len(), MAX_LEDGER + 1);
    }
}
