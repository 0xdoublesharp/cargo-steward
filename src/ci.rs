//! CI support: `cargo steward ci start` before a job builds, and
//! `cargo steward ci trim` before its build directory is cached or kept.
//!
//! `start` records when the job began and sets the access time of every
//! fingerprint file to the epoch. Cargo reads a unit's fingerprint whenever a
//! build considers the unit, even when it finds it fresh and compiles
//! nothing, and that read moves the access time to the present: Linux
//! (`relatime` or `strictatime`) and macOS update an access time older than
//! the modification time. So at `trim`, a unit whose fingerprint was neither
//! read nor written since the start was in no build of the job, and goes.
//!
//! Where the filesystem doesn't record reads (`noatime`), `start` notices,
//! leaves access times alone, and `trim` only applies the size budget.

mod marker;
mod trim;
mod units;

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Subcommand;

#[derive(Debug, clap::Args)]
pub struct Args {
    #[command(subcommand)]
    command: CiCommand,
}

#[derive(Debug, Subcommand)]
enum CiCommand {
    /// Run before the job builds: mark the start, so `trim` can tell which
    /// build units the job used
    Start {
        /// The build directory; by default the one `cargo metadata` reports
        #[arg(long, value_name = "DIR")]
        target_dir: Option<PathBuf>,
    },
    /// Run last, before the cache is saved: remove what the job didn't use,
    /// then the least recently used units until under the budget
    Trim(TrimArgs),
}

#[derive(Debug, clap::Args)]
struct TrimArgs {
    /// The build directory; by default the one `cargo metadata` reports
    #[arg(long, value_name = "DIR")]
    target_dir: Option<PathBuf>,
    /// Most to keep of the build units trim can remove, such as `8GiB`;
    /// `doc/`, final outputs and profiles a build is using don't count
    #[arg(long, value_name = "SIZE")]
    budget: Option<String>,
    /// Keep the workspace's own crates (removed by default: they change with
    /// most commits)
    #[arg(long)]
    keep_workspace: bool,
    /// Keep incremental caches (removed by default)
    #[arg(long)]
    keep_incremental: bool,
    /// Report what would be removed without removing anything
    #[arg(long)]
    dry_run: bool,
    /// List each item removed
    #[arg(short, long)]
    verbose: bool,
}

pub fn run(args: &Args) -> Result<()> {
    match &args.command {
        CiCommand::Start { target_dir } => {
            let root = match target_dir {
                Some(dir) => dir.clone(),
                None => metadata()?.build_dir,
            };
            start(&root, marker::probe_atime)
        }
        CiCommand::Trim(args) => {
            let budget = args
                .budget
                .as_deref()
                .filter(|b| !b.trim().is_empty())
                .map(|b| crate::config::parse_size(b).with_context(|| format!("--budget {b:?}")))
                .transpose()?;
            let (root, members) = match &args.target_dir {
                Some(dir) => (dir.clone(), metadata().ok().map(|m| m.members)),
                None => {
                    let meta = metadata()?;
                    (meta.build_dir, Some(meta.members))
                }
            };
            if members.is_none() && !args.keep_workspace {
                eprintln!(
                    "warning: cargo-steward: `cargo metadata` failed here, so the workspace's \
                     own crates can't be told apart and are trimmed like the rest"
                );
            }
            trim::run(&trim::Options {
                root,
                members: members.unwrap_or_default(),
                budget,
                keep_workspace: args.keep_workspace,
                keep_incremental: args.keep_incremental,
                dry_run: args.dry_run,
                verbose: args.verbose,
                step_summary: env::var_os("GITHUB_STEP_SUMMARY")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from),
            })
        }
    }
}

/// `probe` is [`marker::probe_atime`]; tests pass one for a `noatime` mount.
fn start(root: &Path, probe: fn(&Path) -> std::io::Result<bool>) -> Result<()> {
    fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let start = marker::now_secs();
    let atime =
        probe(root).with_context(|| format!("checking access times in {}", root.display()))?;
    let reset = if atime { marker::reset_atimes(root) } else { 0 };
    marker::write(root, &marker::Marker { start, atime })?;
    println!(
        "cargo-steward: marked the job's start in {}",
        root.display()
    );
    if atime {
        println!("  {reset} fingerprint files will show whether this job uses their unit");
    } else {
        println!(
            "  this filesystem doesn't record reads (noatime), so `ci trim` can't tell which \
             units the job used and only applies --budget"
        );
    }
    Ok(())
}

/// What `cargo metadata` says about the workspace in the current directory.
struct Metadata {
    build_dir: PathBuf,
    /// Package names of the workspace members.
    members: BTreeSet<String>,
}

fn metadata() -> Result<Metadata> {
    // Cargo sets `CARGO` for its subcommands to the cargo running them.
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let out = Command::new(&cargo)
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .with_context(|| format!("running {} metadata", cargo.to_string_lossy()))?;
    if !out.status.success() {
        bail!(
            "`cargo metadata` failed (pass --target-dir to name the build directory):\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_metadata(&out.stdout)
}

/// The build directory (`build_directory`, from Cargo 1.91, else
/// `target_directory`) and, from `--no-deps` output, the members' names.
fn parse_metadata(json: &[u8]) -> Result<Metadata> {
    let value: serde_json::Value =
        serde_json::from_slice(json).context("reading `cargo metadata` output")?;
    let build_dir = ["build_directory", "target_directory"]
        .iter()
        .find_map(|key| value.get(key).and_then(|v| v.as_str()))
        .map(PathBuf::from)
        .context("`cargo metadata` reported no target directory")?;
    let members = value
        .get("packages")
        .and_then(|p| p.as_array())
        .into_iter()
        .flatten()
        .filter_map(|p| p.get("name").and_then(|n| n.as_str()))
        .map(str::to_owned)
        .collect();
    Ok(Metadata { build_dir, members })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_build_directory_and_members_from_metadata() {
        let json = br#"{"packages":[{"name":"app"},{"name":"app-core"}],
            "target_directory":"/w/target","build_directory":"/w/build","version":1}"#;
        let meta = parse_metadata(json).unwrap();
        assert_eq!(meta.build_dir, PathBuf::from("/w/build"));
        assert_eq!(
            meta.members,
            BTreeSet::from(["app".to_owned(), "app-core".to_owned()])
        );
        let old = br#"{"packages":[],"target_directory":"/w/target"}"#;
        assert_eq!(
            parse_metadata(old).unwrap().build_dir,
            PathBuf::from("/w/target")
        );
        assert!(parse_metadata(b"{}").is_err());
    }

    #[test]
    fn start_on_a_noatime_mount_records_budget_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("target");
        start(&root, |_| Ok(false)).unwrap();
        let marker = marker::read(&root).unwrap().unwrap();
        assert!(!marker.atime);
        // The real probe on this machine's filesystem, for comparison.
        start(&root, marker::probe_atime).unwrap();
        assert!(marker::read(&root).unwrap().is_some());
    }
}
