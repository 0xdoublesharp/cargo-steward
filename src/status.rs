//! `cargo steward status`: the build directories cargo-steward knows about.

use std::collections::BTreeMap;
use std::thread;

use anyhow::{Context, Result};

use crate::config::{self, Paths};
use crate::fsutil::{self, ago, human};
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

    let sizes: Vec<Option<u64>> = if sizes {
        thread::scope(|s| {
            let jobs: Vec<_> = dirs
                .iter()
                .map(|d| s.spawn(|| fsutil::tree_size(&d.root)))
                .collect();
            jobs.into_iter().map(|j| j.join().ok()).collect()
        })
    } else {
        vec![None; dirs.len()]
    };

    println!(
        "{:>9} {:>10}  {:<7}  BUILD DIRECTORY",
        "LAST USED", "SIZE", "LAYOUT"
    );
    for (dir, size) in dirs.iter().zip(&sizes) {
        let size = size.map_or_else(|| "-".to_owned(), human);
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
            "{:>9} {size:>10}  {:<7}  {}{notes}",
            ago(dir.last_used),
            layout(dir),
            dir.root.display()
        );
    }
    if let Some(total) = sizes.iter().copied().sum::<Option<u64>>() {
        println!(
            "{:>9} {:>10}  (blocks shared by clones count in each directory)",
            "",
            human(total)
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
