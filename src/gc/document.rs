//! The `--json` document of `gc` and `remove`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

use super::{Cleanup, Kind, Options};
use crate::json;

/// What a cleanup removed (or would), what it kept back, and what the disks
/// gained.
#[derive(Debug, Serialize)]
pub struct Document {
    pub version: u32,
    /// "gc" or "remove".
    pub command: &'static str,
    /// Whether anything was removed (`--apply`), or this is a preview.
    pub applied: bool,
    /// `gc --apply` found another cleanup running and did nothing.
    pub busy: bool,
    /// `remove --worktree`: each worktree and the build directory it builds
    /// into.
    pub worktrees: Vec<Worktree>,
    /// `remove`: paths that aren't cargo build directories.
    pub refused: Vec<Refused>,
    pub actions: Vec<Action>,
    pub skipped: Vec<Skipped>,
    /// One per kind with actions, in [`Kind`] order.
    pub totals: Vec<Total>,
    pub total_bytes: u64,
    /// `gc --apply`: the measured change in free space on each disk it
    /// removed from. Other writes meanwhile count too, so it may be negative.
    pub freed_on_disk: Vec<Freed>,
    /// `gc --apply`, where it shares identical files afterwards: what it
    /// shared.
    pub shared: Option<Shared>,
}

#[derive(Debug, Serialize)]
pub struct Action {
    pub kind: &'static str,
    pub description: &'static str,
    pub path: String,
    /// Files removed, for kinds that count them (stale objects); else 0.
    pub files: usize,
    pub bytes: u64,
    pub note: String,
}

#[derive(Debug, Serialize)]
pub struct Skipped {
    pub kind: &'static str,
    pub description: &'static str,
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct Total {
    pub kind: &'static str,
    pub description: &'static str,
    pub count: usize,
    pub bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct Freed {
    /// A path on the disk.
    pub path: String,
    pub bytes: i64,
}

#[derive(Debug, Serialize)]
pub struct Shared {
    pub files: usize,
    pub bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct Worktree {
    pub worktree: String,
    pub build_dir: String,
    /// Whether the build directory exists (one that doesn't has nothing to
    /// remove).
    pub exists: bool,
}

#[derive(Debug, Serialize)]
pub struct Refused {
    pub path: String,
    pub reason: String,
}

impl Document {
    pub fn new(cleanup: &Cleanup, freed: &[(PathBuf, i128)], options: Options) -> Document {
        let mut totals: BTreeMap<Kind, (usize, u64)> = BTreeMap::new();
        for action in &cleanup.actions {
            let total = totals.entry(action.kind).or_default();
            total.0 += 1;
            total.1 += action.bytes;
        }
        Document {
            version: json::VERSION,
            command: options.command,
            applied: options.apply,
            busy: false,
            worktrees: Vec::new(),
            refused: Vec::new(),
            actions: cleanup
                .actions
                .iter()
                .map(|a| Action {
                    kind: a.kind.id(),
                    description: a.kind.describe(),
                    path: json::path(&a.path),
                    files: a.files,
                    bytes: a.bytes,
                    note: a.note.clone(),
                })
                .collect(),
            skipped: cleanup
                .skipped
                .iter()
                .map(|s| Skipped {
                    kind: s.kind.id(),
                    description: s.kind.describe(),
                    path: json::path(&s.path),
                    reason: s.reason.clone(),
                })
                .collect(),
            total_bytes: totals.values().map(|t| t.1).sum(),
            totals: totals
                .into_iter()
                .map(|(kind, (count, bytes))| Total {
                    kind: kind.id(),
                    description: kind.describe(),
                    count,
                    bytes,
                })
                .collect(),
            freed_on_disk: freed
                .iter()
                .map(|(probe, delta)| Freed {
                    path: json::path(probe),
                    bytes: i64::try_from(*delta).unwrap_or(if *delta < 0 {
                        i64::MIN
                    } else {
                        i64::MAX
                    }),
                })
                .collect(),
            shared: None,
        }
    }

    /// `cargo steward remove`'s document.
    pub fn removal(
        requested: &super::Requested,
        worktrees: Vec<Worktree>,
        apply: bool,
    ) -> Document {
        let mut doc = Document::new(
            &requested.cleanup,
            &requested.freed,
            Options {
                apply,
                command: requested.command,
                ..Options::default()
            },
        );
        doc.worktrees = worktrees;
        doc.refused = requested
            .refused
            .iter()
            .map(|(path, reason)| Refused {
                path: json::path(path),
                reason: reason.clone(),
            })
            .collect();
        doc
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{Action as Removed, Skipped as Kept};

    #[test]
    fn totals_each_kind_and_keeps_the_sign_of_what_was_freed() {
        let cleanup = Cleanup {
            actions: vec![
                Removed {
                    kind: Kind::Orphaned,
                    path: PathBuf::from("/b/x"),
                    files: 0,
                    bytes: 30,
                    note: "unused for 3d".into(),
                },
                Removed {
                    kind: Kind::StaleObjects,
                    path: PathBuf::from("/b/y/debug/deps"),
                    files: 2,
                    bytes: 10,
                    note: String::new(),
                },
                Removed {
                    kind: Kind::StaleObjects,
                    path: PathBuf::from("/b/z/debug/deps"),
                    files: 1,
                    bytes: 5,
                    note: String::new(),
                },
            ],
            skipped: vec![Kept {
                kind: Kind::Idle,
                path: PathBuf::from("/b/w"),
                reason: "busy".into(),
            }],
        };
        let options = Options {
            apply: true,
            command: "gc",
            ..Options::default()
        };
        let doc = Document::new(&cleanup, &[(PathBuf::from("/b"), -4096)], options);
        let value = serde_json::to_value(&doc).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["applied"], true);
        assert_eq!(value["total_bytes"], 45);
        assert_eq!(value["totals"][0]["kind"], "stale_objects");
        assert_eq!(value["totals"][0]["count"], 2);
        assert_eq!(value["totals"][0]["bytes"], 15);
        assert_eq!(value["totals"][1]["kind"], "orphaned");
        assert_eq!(value["actions"][1]["files"], 2);
        assert_eq!(value["skipped"][0]["kind"], "idle");
        assert_eq!(value["skipped"][0]["reason"], "busy");
        assert_eq!(value["freed_on_disk"][0]["bytes"], -4096);
        assert_eq!(value["shared"], serde_json::Value::Null);
    }

    #[test]
    fn every_kind_has_its_own_snake_case_id() {
        let kinds = [
            Kind::StaleObjects,
            Kind::OldSessions,
            Kind::LegacyLayout,
            Kind::IdleIncremental,
            Kind::IdleUnit,
            Kind::Orphaned,
            Kind::Idle,
            Kind::LowSpace,
            Kind::Requested,
            Kind::RoutedTmp,
        ];
        let ids: std::collections::BTreeSet<&str> = kinds.iter().map(|k| k.id()).collect();
        assert_eq!(ids.len(), kinds.len());
        assert!(
            ids.iter()
                .all(|id| id.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
        );
    }
}
