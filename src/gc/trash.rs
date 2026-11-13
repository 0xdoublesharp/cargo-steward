//! Recovery of renamed trees, even when their original build root is gone.
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{BuildDir, Cleanup, Kind, Skipped, Sweep, lock_build_dir};
use crate::{config, fsutil, scan};

/// Find trash in configured scan/routing roots and beside recorded builds.
pub fn inventory(config: &config::Config, dirs: &[BuildDir]) -> Vec<PathBuf> {
    let mut roots = super::scan_roots(config);
    roots.extend(crate::proxy::tmp_roots(config));
    let mut found = scan::discover_trash(&roots, super::SCAN_DEPTH);
    found.extend(near_builds(dirs));
    scan::outermost(found)
}

pub(super) fn near_builds(dirs: &[BuildDir]) -> Vec<PathBuf> {
    let roots: Vec<_> = dirs
        .iter()
        .flat_map(|d| {
            [Some(d.root.clone()), d.root.parent().map(Path::to_path_buf)]
                .into_iter()
                .flatten()
        })
        .collect();
    scan::discover_trash(&roots, 0)
}

#[derive(Serialize)]
pub struct Entry {
    path: PathBuf,
    owner_pid: Option<u32>,
    owner_running: bool,
    size_bytes: Option<u64>,
    unshared_bytes: Option<u64>,
}

pub fn entries(paths: &[PathBuf], sizes: bool) -> Vec<Entry> {
    fsutil::parallel_map(paths, |path| {
        let usage = sizes.then(|| fsutil::tree_usage(path));
        let owner_pid = fsutil::trash_owner(path);
        Entry {
            path: path.clone(),
            owner_pid,
            owner_running: owner_pid.is_some_and(fsutil::process_alive),
            size_bytes: usage.map(|u| u.allocated),
            unshared_bytes: usage.and_then(|u| u.private),
        }
    })
}

/// Finish only validated, unowned trees under the usual process/Cargo guards.
/// Automatic GC reports these but leaves recovery to an explicit apply.
pub(super) fn clean(paths: &[PathBuf], sweep: Sweep, recover: bool) -> Cleanup {
    let mut out = Cleanup::default();
    for path in paths {
        let reason = if !fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
            || path.canonicalize().as_ref().ok() != Some(path)
        {
            Some("trash path is not a real, resolved directory".to_owned())
        } else {
            match fsutil::trash_owner(path) {
                None => Some("invalid steward-trash name".to_owned()),
                Some(pid) if fsutil::process_alive(pid) => {
                    Some(format!("deletion owner pid {pid} is still running"))
                }
                _ if path.join(super::PIN_FILE).exists() => Some("pinned".to_owned()),
                _ if !recover => {
                    Some("interrupted deletion requires explicit gc --apply".to_owned())
                }
                _ => None,
            }
        };
        if let Some(reason) = reason {
            out.skipped.push(Skipped {
                kind: Kind::InterruptedTrash,
                path: path.clone(),
                reason,
            });
            continue;
        }
        // A partially deleted tree may contain Cargo builds nested in a
        // routed TMPDIR. Reuse the temporary-tree reader to lock each one.
        let Some(contents) = super::TmpContents::read(path, std::time::SystemTime::now()) else {
            out.skipped.push(Skipped {
                kind: Kind::InterruptedTrash,
                path: path.clone(),
                reason: "cannot inspect interrupted deletion".into(),
            });
            continue;
        };
        if contents.live_deletion {
            out.skipped.push(Skipped {
                kind: Kind::InterruptedTrash,
                path: path.clone(),
                reason: "contains a live deletion owner".into(),
            });
            continue;
        }
        if contents.pinned {
            out.skipped.push(Skipped {
                kind: Kind::InterruptedTrash,
                path: path.clone(),
                reason: "contains pinned artifacts".into(),
            });
            continue;
        }
        let mut locks = Vec::new();
        let mut busy = false;
        if sweep.apply {
            for root in std::iter::once(path)
                .chain(contents.build_dirs.iter())
                .collect::<std::collections::BTreeSet<_>>()
            {
                match lock_build_dir(root, &[]) {
                    Some(held) => locks.extend(held),
                    None => busy = true,
                }
            }
        }
        if busy {
            out.skipped.push(Skipped {
                kind: Kind::InterruptedTrash,
                path: path.clone(),
                reason: "a cargo build is using it".into(),
            });
            continue;
        }
        if let Some(process) = sweep.running.inside(path) {
            out.skipped.push(Skipped {
                kind: Kind::InterruptedTrash,
                path: path.clone(),
                reason: format!(
                    "{} is running from it (pid {})",
                    process.exe.display(),
                    process.pid
                ),
            });
            continue;
        }
        if sweep.apply
            && let Err(err) = fsutil::remove_trash(path)
        {
            out.skipped.push(Skipped {
                kind: Kind::InterruptedTrash,
                path: path.clone(),
                reason: err.to_string(),
            });
            continue;
        }
        out.actions.push(super::Action {
            kind: Kind::InterruptedTrash,
            path: path.clone(),
            files: 0,
            bytes: contents.bytes,
            note: "finishing an interrupted deletion".into(),
        });
    }
    out
}
