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
    /// The linked git worktree the package is in, if it is in one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked: Option<Linked>,
}

/// Where git keeps a linked worktree (`git worktree add`): what cleanup asks
/// git about once the worktree is gone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Linked {
    /// Its admin directory, `<repository>/worktrees/<name>`.
    pub admin: PathBuf,
    /// The worktree: the directory holding its `.git` file.
    pub worktree: PathBuf,
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
    /// The linked worktree each of `workspaces` was in, for those in one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub linked: BTreeMap<PathBuf, Linked>,
}

impl RootInfo {
    /// Combines what is known about the same directory from two entries.
    pub fn absorb(&mut self, other: &RootInfo) {
        self.last_used = self.last_used.max(other.last_used);
        self.compile_ms = self.compile_ms.saturating_add(other.compile_ms);
        self.workspaces.extend(other.workspaces.iter().cloned());
        for (workspace, linked) in &other.linked {
            self.linked
                .entry(workspace.clone())
                .or_insert_with(|| linked.clone());
        }
    }
}

impl Registry {
    fn merge(&mut self, record: Record) {
        let info = self.roots.entry(record.root).or_default();
        info.last_used = info.last_used.max(record.ts);
        info.compile_ms = info.compile_ms.saturating_add(record.ms);
        if let Some(workspace) = record.workspace {
            // The latest record says where the workspace is now.
            match record.linked {
                Some(linked) => info.linked.insert(workspace.clone(), linked),
                None => info.linked.remove(&workspace),
            };
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

/// The registry with the ledger's records folded in, changing nothing on
/// disk: for commands that only report, which may run where the steward
/// home is read-only (an agent's sandbox).
pub fn peek(paths: &Paths) -> Registry {
    let mut registry = load(paths);
    let pending = crate::fsutil::entries(&paths.home)
        .filter(|(name, _)| name.starts_with("ledger.") && name.ends_with(".processing"))
        .map(|(_, path)| path);
    for path in std::iter::once(paths.ledger()).chain(pending) {
        let text = fs::read_to_string(path).unwrap_or_default();
        for record in text
            .lines()
            .filter_map(|l| serde_json::from_str::<Record>(l).ok())
        {
            registry.merge(record);
        }
    }
    registry
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
                    linked: None,
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
                linked: None,
            },
        );
        let registry = compact(&paths).unwrap();
        assert_eq!(registry.roots[&root].last_used, 40);
        assert_eq!(registry.roots[&root].compile_ms, 16);
    }

    #[test]
    fn keeps_where_each_workspace_was_in_git_and_reads_older_files() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: tmp.path().to_path_buf(),
        };
        // A registry and a ledger line written before worktrees were recorded.
        fs::write(
            paths.registry(),
            r#"{"roots":{"/b/old":{"last_used":5,"workspaces":["/w/old"],"compile_ms":1}}}"#,
        )
        .unwrap();
        fs::write(
            paths.ledger(),
            "{\"ts\":6,\"root\":\"/b/old\",\"workspace\":\"/w/old\",\"ms\":1}\n",
        )
        .unwrap();
        let root = PathBuf::from("/b/target");
        let linked = Linked {
            admin: "/r/.git/worktrees/wt".into(),
            worktree: "/w/wt".into(),
        };
        let record = |ts, linked: Option<Linked>| Record {
            ts,
            root: root.clone(),
            workspace: Some("/w/wt/crate".into()),
            ms: 1,
            linked,
        };
        append(&paths, &record(10, Some(linked.clone())));
        let registry = compact(&paths).unwrap();
        assert_eq!(registry.roots[&PathBuf::from("/b/old")].last_used, 6);
        assert!(registry.roots[&PathBuf::from("/b/old")].linked.is_empty());
        assert_eq!(
            registry.roots[&root].linked[&PathBuf::from("/w/wt/crate")],
            linked
        );
        // The latest record decides: that path is no longer in a worktree.
        append(&paths, &record(20, None));
        let registry = compact(&paths).unwrap();
        assert!(registry.roots[&root].linked.is_empty());
        assert_eq!(load(&paths), registry);
    }

    #[test]
    fn absorb_keeps_the_latest_use_and_sums_compile_time() {
        let mut a = RootInfo {
            last_used: 10,
            workspaces: [PathBuf::from("/w/a")].into(),
            compile_ms: 5,
            linked: BTreeMap::new(),
        };
        let b = RootInfo {
            last_used: 7,
            workspaces: [PathBuf::from("/w/b")].into(),
            compile_ms: u64::MAX,
            linked: BTreeMap::new(),
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
                linked: None,
            },
        );
        assert_eq!(fs::metadata(paths.ledger()).unwrap().len(), MAX_LEDGER + 1);
    }

    #[cfg(unix)]
    #[test]
    fn compact_fails_when_it_cannot_claim_the_ledger() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: tmp.path().join("home"),
        };
        append(
            &paths,
            &Record {
                ts: 1,
                root: PathBuf::from("/x"),
                workspace: None,
                ms: 0,
                linked: None,
            },
        );
        let mode = |m| fs::set_permissions(&paths.home, fs::Permissions::from_mode(m)).unwrap();
        mode(0o555);
        let result = compact(&paths);
        mode(0o755);
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("claiming the ledger"), "{err}");
        // The records are still there for the next try.
        assert!(paths.ledger().exists());
        assert_eq!(compact(&paths).unwrap().roots.len(), 1);
    }

    #[test]
    fn peeking_reads_every_record_and_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: tmp.path().to_path_buf(),
        };
        let root = PathBuf::from("/b/target");
        let record = |ts| Record {
            ts,
            root: root.clone(),
            workspace: None,
            ms: 2,
            linked: None,
        };
        append(&paths, &record(10));
        compact(&paths).unwrap();
        append(&paths, &record(30));
        // Left by a compaction that was interrupted.
        fs::write(
            tmp.path().join("ledger.99.processing"),
            format!("{}\n", serde_json::to_string(&record(20)).unwrap()),
        )
        .unwrap();
        let before: Vec<_> = crate::fsutil::entries(tmp.path()).map(|(n, _)| n).collect();
        let registry = peek(&paths);
        assert_eq!(registry.roots[&root].last_used, 30);
        assert_eq!(registry.roots[&root].compile_ms, 6);
        let after: Vec<_> = crate::fsutil::entries(tmp.path()).map(|(n, _)| n).collect();
        assert_eq!(before, after);
        assert_eq!(load(&paths).roots[&root].last_used, 10, "not saved");
    }
}
