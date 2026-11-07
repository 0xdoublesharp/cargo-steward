//! `cargo steward status`: the build directories cargo-steward knows about.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

use crate::config::{self, Paths};
use crate::fsutil::{self, Usage, ago, human};
use crate::gc::{self, BuildDir};

pub fn run(sizes: bool) -> Result<()> {
    let paths = Paths::resolve().context("cannot locate the home directory")?;
    let config = config::load(&paths)?;
    let (_, mut dirs) = gc::inventory(&paths, &config.scan_roots)?;
    if dirs.is_empty() {
        println!("No build directories found yet.");
        println!(
            "They are recorded as you build, and found under `scan-roots` in {}.",
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
    Ok(())
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
