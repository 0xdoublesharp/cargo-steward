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
//! `start` saves the access times it resets first, and `trim` puts them
//! back. When a job never gets to `trim`, the next `start` does, and `gc`
//! goes by the saved times until then.
//!
//! Where the filesystem doesn't record reads (`noatime`), `start` notices,
//! leaves access times alone, and `trim` only applies the size budget.

mod marker;
mod trim;
mod units;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use clap::Subcommand;

/// When each unit's fingerprint in `root` was last used before a `ci start`
/// whose job hasn't reached `ci trim` reset its access times, by fingerprint
/// directory. Until then those access times only say whether the job read
/// the unit, so cleanup goes by these instead.
pub fn saved_last_use(root: &Path) -> BTreeMap<PathBuf, SystemTime> {
    marker::load_saved(root)
}

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
    /// then enforce the budget using the selected eviction policy
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
    /// Keep selected workspace packages eligible for caching (still subject
    /// to job usage and the budget); may be repeated
    #[arg(long = "keep-package", value_name = "PACKAGE")]
    keep_packages: Vec<String>,
    /// Budget eviction policy; rebuild-cost falls back to LRU when unmeasured
    #[arg(long, value_enum, default_value = "rebuild-cost")]
    eviction: trim::Eviction,
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
                keep_packages: args.keep_packages.iter().cloned().collect(),
                eviction: args.eviction,
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
    if !root.is_dir() {
        // Cargo tags a build directory it creates (`CACHEDIR.TAG`, and on
        // macOS a Time Machine exclusion), but not one that already exists,
        // so creating it is left to cargo. Everything the job builds there is
        // new and so used: there's nothing to mark.
        println!(
            "cargo-steward: {} doesn't exist yet, so everything the job builds there counts as used",
            root.display()
        );
        return Ok(());
    }
    // A job that never got to `trim` (it failed, or was cancelled) left
    // access times reset: put them back before saving them again.
    if root.join(marker::SAVED_FILE).exists() {
        marker::restore_all(root)
            .with_context(|| format!("restoring access times in {}", root.display()))?;
    }
    let start = marker::now_secs();
    let atime =
        probe(root).with_context(|| format!("checking access times in {}", root.display()))?;
    let reset = if atime {
        marker::reset_atimes(root)
            .with_context(|| format!("saving access times in {}", root.display()))?
    } else {
        0
    };
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
    use std::fs;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn access_times_survive_a_job_that_never_trims() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("target");
        let dir = root.join("debug/.fingerprint/serde-0123456789abcdef");
        let file = dir.join("lib-serde");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&file, b"x").unwrap();
        let built = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let read = UNIX_EPOCH + Duration::from_secs(1_750_000_000);
        fs::File::open(&file)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(built).set_accessed(read))
            .unwrap();
        let accessed = || fs::metadata(&file).unwrap().accessed().unwrap();

        start(&root, marker::probe_atime).unwrap();
        if !marker::read(&root).unwrap().unwrap().atime {
            return; // `noatime`: nothing was reset.
        }
        assert_eq!(accessed(), UNIX_EPOCH);
        // Cleanup meanwhile goes by when the unit was last read, not built.
        assert_eq!(saved_last_use(&root).get(&dir), Some(&read));

        // The job fails before `trim`; the next job's start keeps that time.
        start(&root, marker::probe_atime).unwrap();
        assert_eq!(accessed(), UNIX_EPOCH);
        assert_eq!(saved_last_use(&root).get(&dir), Some(&read));

        // And `trim` puts it back.
        // Keep this unit using a fixed job start instead of its directory's
        // creation time coincidentally falling within the current second.
        marker::write(
            &root,
            &marker::Marker {
                start: 1_600_000_000,
                atime: true,
            },
        )
        .unwrap();
        trim::run(&trim::Options {
            root: root.clone(),
            members: BTreeSet::new(),
            budget: None,
            keep_workspace: true,
            keep_incremental: true,
            keep_packages: BTreeSet::new(),
            eviction: trim::Eviction::default(),
            dry_run: false,
            verbose: false,
            step_summary: None,
        })
        .unwrap();
        assert_eq!(accessed(), read);
        assert!(saved_last_use(&root).is_empty());
        assert_eq!(marker::read(&root).unwrap(), None);
    }

    #[test]
    fn start_leaves_creating_the_build_directory_to_cargo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("target");
        start(&root, marker::probe_atime).unwrap();
        assert!(
            !root.exists(),
            "cargo only tags a build directory it creates"
        );
    }

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
        // An existing build directory: one cargo creates has nothing to mark.
        std::fs::create_dir_all(&root).unwrap();
        start(&root, |_| Ok(false)).unwrap();
        let marker = marker::read(&root).unwrap().unwrap();
        assert!(!marker.atime);
        // The real probe on this machine's filesystem, for comparison.
        start(&root, marker::probe_atime).unwrap();
        assert!(marker::read(&root).unwrap().is_some());
    }
}
