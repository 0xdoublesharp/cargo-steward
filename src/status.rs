//! `cargo steward status`: the build directories cargo-steward knows about.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::{self, Paths};
use crate::fsutil::{self, Usage, ago, human};
use crate::gc::{self, BuildDir};
use crate::json;

pub fn run(sizes: bool, as_json: bool, discover: bool) -> Result<()> {
    let paths = Paths::resolve().context("cannot locate the home directory")?;
    let config = config::load_warning(&paths);
    let mut dirs = if discover {
        gc::inventory(&paths, &gc::scan_roots(&config), false)?.1
    } else {
        gc::recorded_inventory(&paths)
    };
    let trash = gc::trash::inventory(&config, &dirs);
    if as_json {
        let mut doc = document(dirs, sizes, discover);
        doc.trash_dirs = gc::trash::entries(&trash, sizes);
        return json::print(&doc);
    }
    if !trash.is_empty() {
        println!("Interrupted steward deletions (gc previews recovery; gc --apply finishes it):");
        for path in &trash {
            let owner = fsutil::trash_owner(path);
            let state = match owner {
                Some(pid) if fsutil::process_alive(pid) => format!("live owner pid {pid}; kept"),
                Some(_) => "owner gone".into(),
                None => "invalid name; kept".into(),
            };
            let size = if sizes {
                human(fsutil::tree_size(path))
            } else {
                "-".into()
            };
            println!("{size:>10}  {} [{state}]", path.display());
        }
        println!();
    }
    if dirs.is_empty() {
        println!("No build directories found yet.");
        println!(
            "They are recorded as you build. Run `cargo steward status --discover` to search\nscan-roots in {}.",
            paths.config().display()
        );
        return Ok(());
    }
    dirs.sort_by_key(|d| std::cmp::Reverse(d.last_used));

    let sizes: Vec<Option<Usage>> = if sizes {
        fsutil::parallel_map(&dirs, |d| Some(fsutil::tree_usage(&d.root)))
    } else {
        vec![None; dirs.len()]
    };

    println!(
        "{:>9} {:>10} {:>10}  {:<7}  BUILD DIRECTORY",
        "LAST USED", "SIZE", "UNSHARED", "LAYOUT"
    );
    for (dir, usage) in dirs.iter().zip(&sizes) {
        let size = usage.map_or_else(|| "-".to_owned(), |u| human(u.allocated));
        let unshared = usage
            .and_then(|u| u.private)
            .map_or_else(|| "-".to_owned(), human);
        let mut notes = Vec::new();
        if dir.pinned {
            notes.push("pinned".to_owned());
        }
        if dir.orphaned() {
            notes.push("workspaces deleted".to_owned());
        }
        let notes = if notes.is_empty() {
            String::new()
        } else {
            format!("  [{}]", notes.join(", "))
        };
        println!(
            "{:>9} {size:>10} {unshared:>10}  {:<7}  {}{notes}",
            ago(dir.last_used),
            layout(dir),
            dir.root.display()
        );
    }
    if let Some(usages) = sizes.iter().copied().collect::<Option<Vec<Usage>>>() {
        let total: u64 = usages.iter().map(|u| u.allocated).sum();
        let unshared = usages
            .iter()
            .map(|u| u.private)
            .sum::<Option<u64>>()
            .map_or_else(|| "-".to_owned(), human);
        println!("{:>9} {:>10} {unshared:>10}", "", human(total));
        println!(
            "\nSIZE counts every file's blocks, as `du` does, so blocks APFS or btrfs clones share\n\
             count in each directory. UNSHARED counts only blocks no other file shares. Removing\n\
             a directory frees at least its UNSHARED size and at most its SIZE."
        );
    }

    let mut volumes: BTreeMap<u64, &BuildDir> = BTreeMap::new();
    for dir in &dirs {
        if let Some(device) = dir.device {
            volumes.entry(device).or_insert(dir);
        }
    }
    println!();
    for dir in volumes.values() {
        if let Some(free) = fsutil::free_space(&dir.root) {
            println!(
                "{} free on the disk holding {}",
                human(free),
                dir.root.display()
            );
        }
    }
    if !sizes.iter().any(Option::is_some) {
        println!("Run with --sizes to measure each directory.");
    }
    if !discover {
        println!(
            "Recorded builds and their last observed compile; --discover scans other directories and artifact use."
        );
    }
    Ok(())
}

/// `status --json`.
#[derive(Serialize)]
struct Document {
    version: u32,
    discovery_performed: bool,
    /// Whether sizes were measured (`--sizes`).
    sizes_measured: bool,
    /// Most recently used first.
    build_dirs: Vec<DirEntry>,
    /// Interrupted deletions, discovered independently of build roots.
    trash_dirs: Vec<gc::trash::Entry>,
    /// With `--sizes`: the sums over every directory.
    total_bytes: Option<u64>,
    total_unshared_bytes: Option<u64>,
    /// Free space on each disk holding build directories.
    disks: Vec<Disk>,
}

#[derive(Serialize)]
struct DirEntry {
    path: String,
    last_used: String,
    last_used_secs_ago: u64,
    /// "legacy", "v2", "mixed" or "empty".
    layout: &'static str,
    pinned: bool,
    /// Every workspace that built here has been deleted.
    orphaned: bool,
    workspaces: Vec<String>,
    /// As `du` counts it; with `--sizes` only.
    size_bytes: Option<u64>,
    /// Blocks no clone shares; with `--sizes`, where the filesystem can tell.
    unshared_bytes: Option<u64>,
}

#[derive(Serialize)]
struct Disk {
    /// The first build directory on the disk.
    path: String,
    free_bytes: u64,
}

fn document(mut dirs: Vec<BuildDir>, sizes: bool, discover: bool) -> Document {
    dirs.sort_by_key(|d| std::cmp::Reverse(d.last_used));
    let usages: Vec<Option<Usage>> = if sizes {
        fsutil::parallel_map(&dirs, |d| Some(fsutil::tree_usage(&d.root)))
    } else {
        vec![None; dirs.len()]
    };
    let mut seen = std::collections::BTreeSet::new();
    let disks = dirs
        .iter()
        .filter(|d| d.device.is_some_and(|device| seen.insert(device)))
        .filter_map(|d| {
            Some(Disk {
                path: json::path(&d.root),
                free_bytes: fsutil::free_space(&d.root)?,
            })
        })
        .collect();
    let measured: Option<Vec<Usage>> = sizes.then(|| usages.iter().flatten().copied().collect());
    Document {
        version: json::VERSION,
        discovery_performed: discover,
        sizes_measured: sizes,
        trash_dirs: Vec::new(),
        total_bytes: measured
            .as_ref()
            .map(|u| u.iter().map(|u| u.allocated).sum()),
        total_unshared_bytes: measured
            .as_ref()
            .and_then(|u| u.iter().map(|u| u.private).sum()),
        build_dirs: dirs
            .iter()
            .zip(&usages)
            .map(|(dir, usage)| DirEntry {
                path: json::path(&dir.root),
                last_used: json::time(dir.last_used),
                last_used_secs_ago: json::secs_ago(dir.last_used),
                layout: layout(dir),
                pinned: dir.pinned,
                orphaned: dir.orphaned(),
                workspaces: dir.workspaces.iter().map(|w| json::path(w)).collect(),
                size_bytes: usage.map(|u| u.allocated),
                unshared_bytes: usage.and_then(|u| u.private),
            })
            .collect(),
        disks,
    }
}

fn layout(dir: &BuildDir) -> &'static str {
    let legacy = dir.profiles.iter().any(|p| p.legacy);
    let v2 = dir.profiles.iter().any(|p| p.v2);
    match (legacy, v2) {
        (true, true) => "mixed",
        (true, false) => "legacy",
        (false, true) => "v2",
        (false, false) => "empty",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::Profile;
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn dir(flags: &[(bool, bool)]) -> BuildDir {
        BuildDir {
            root: PathBuf::from("/x"),
            profiles: flags
                .iter()
                .map(|&(legacy, v2)| Profile {
                    path: PathBuf::from("/x/debug"),
                    legacy,
                    v2,
                })
                .collect(),
            last_used: SystemTime::now(),
            workspaces: Default::default(),
            pinned: false,
            device: None,
            compile_ms: 0,
            linked: Default::default(),
            scans: Vec::new(),
        }
    }

    #[test]
    fn layout_names_what_the_profiles_hold() {
        assert_eq!(layout(&dir(&[])), "empty");
        assert_eq!(layout(&dir(&[(true, false)])), "legacy");
        assert_eq!(layout(&dir(&[(false, true)])), "v2");
        assert_eq!(layout(&dir(&[(true, false), (false, true)])), "mixed");
        assert_eq!(layout(&dir(&[(true, true)])), "mixed");
    }
}
