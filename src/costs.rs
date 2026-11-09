//! Small per-unit observations, kept with the build output they describe.
//! These are eviction/admission hints, never compiler-cache validity keys.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{fsutil, json, scan};

pub const PREFIX: &str = ".cargo-steward-";

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Observation {
    pub version: u32,
    pub crate_name: String,
    pub output_dir: PathBuf,
    pub calls: u64,
    pub execution_ms: u64,
    pub queue_ms: u64,
    pub cpu_ms: u64,
    /// A short rolling history; cache-client time is never a rebuild hint.
    pub cpu_samples_ms: Vec<u64>,
    pub max_execution_ms: u64,
    pub max_rss_bytes: Option<u64>,
}

pub fn path(out: &Path, extra: Option<&str>) -> Option<PathBuf> {
    let hash = extra?.strip_prefix('-')?;
    scan::is_hash(hash).then(|| out.join(format!("{PREFIX}{hash}.json")))
}

/// Bounded, no-follow reads: build output may contain arbitrary files.
pub fn read(path: &Path) -> Option<Observation> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(65536).read_to_end(&mut bytes).ok()?;
    let observation: Observation = serde_json::from_slice(&bytes).ok()?;
    (observation.version == 1).then_some(observation)
}

pub fn millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

/// Best effort, atomically replacing this unit's own metadata file.
pub fn record(
    path: &Path,
    name: &str,
    out: &Path,
    execution: Duration,
    queue: Duration,
    rss: Option<u64>,
    cpu_ms: Option<u64>,
) {
    let mut observation = read(path).unwrap_or_default();
    if observation.output_dir != out {
        // A seeded build retains cost estimates, but not the producer's
        // invocation/queue counters as observations of this checkout.
        observation.calls = 0;
        observation.execution_ms = 0;
        observation.queue_ms = 0;
        observation.cpu_ms = 0;
    }
    observation.version = 1;
    observation.crate_name = name.to_owned();
    observation.output_dir = out.to_path_buf();
    observation.calls = observation.calls.saturating_add(1);
    observation.execution_ms = observation.execution_ms.saturating_add(millis(execution));
    observation.queue_ms = observation.queue_ms.saturating_add(millis(queue));
    observation.max_execution_ms = observation.max_execution_ms.max(millis(execution));
    observation.max_rss_bytes = observation.max_rss_bytes.into_iter().chain(rss).max();
    if let Some(cpu) = cpu_ms {
        observation.cpu_ms = observation.cpu_ms.saturating_add(cpu);
        observation.cpu_samples_ms.push(cpu.max(1));
        let excess = observation.cpu_samples_ms.len().saturating_sub(9);
        observation.cpu_samples_ms.drain(..excess);
    }
    let Some(name) = path.file_name() else {
        return;
    };
    let temporary = path.with_file_name(format!(
        "{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let Ok(mut file) = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
    else {
        return;
    };
    let saved = (|| -> std::io::Result<()> {
        file.write_all(&serde_json::to_vec(&observation)?)?;
        fs::rename(&temporary, path)
    })();
    if saved.is_err() {
        let _ = fs::remove_file(&temporary);
    }
}

/// The peak RSS of the largest process this one waited for, and their total
/// CPU time. Both describe the compiler only when it was spawned directly.
pub fn child_usage() -> (Option<u64>, Option<u64>) {
    // SAFETY: rusage is plain data; getrusage writes into this live buffer.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: the pointer is valid for a rusage and lives through the call.
    if unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) } != 0 {
        return (None, None);
    }
    let micros = |t: libc::timeval| {
        u64::try_from(t.tv_sec)
            .unwrap_or(0)
            .saturating_mul(1_000_000)
            .saturating_add(u64::try_from(t.tv_usec).unwrap_or(0))
    };
    let cpu_ms = micros(usage.ru_utime).saturating_add(micros(usage.ru_stime)) / 1000;
    let bytes = u64::try_from(usage.ru_maxrss).unwrap_or(0);
    #[cfg(target_os = "linux")]
    let bytes = bytes.saturating_mul(1024);
    ((bytes > 0).then_some(bytes), Some(cpu_ms))
}

/// What a peak measured through a cache wrapper says about the compiler.
/// The cache's client is usually all that was measured: its compiler runs in
/// a separate server process. A client that ran the compiler itself, as
/// sccache does for units it can't cache (executables among them), reports
/// the compiler's peak instead. A client stays far below `fallback`, the
/// estimate for an unmeasured compile, so only a larger peak is a compiler's.
pub fn compiler_rss_through_cache(rss: Option<u64>, fallback: u64) -> Option<u64> {
    rss.filter(|&bytes| bytes > fallback)
}

impl Observation {
    /// Median of recent compiler CPU samples. Waiting and cache hits do
    /// not inflate it, and a historical outlier eventually ages out.
    pub fn rebuild_ms(&self) -> Option<u64> {
        let mut samples = self.cpu_samples_ms.clone();
        samples.sort_unstable();
        samples.get(samples.len() / 2).copied().filter(|&n| n > 0)
    }
}

/// Cost observations directly in an output directory, never recursively
/// reading arbitrary build-script output or incremental caches.
pub fn in_dir(dir: &Path) -> Vec<(PathBuf, Observation)> {
    fsutil::entries(dir)
        .filter(|(name, _)| name.starts_with(PREFIX) && name.ends_with(".json"))
        .filter_map(|(_, path)| {
            let mut observation = read(&path)?;
            if observation.output_dir != dir {
                observation.calls = 0;
                observation.execution_ms = 0;
                observation.queue_ms = 0;
                observation.cpu_ms = 0;
            }
            Some((path, observation))
        })
        .collect()
}

pub fn collect(root: &Path) -> Vec<(PathBuf, Observation)> {
    let mut all = Vec::new();
    for profile in scan::profiles(root) {
        let mut dirs = scan::object_dirs(&profile);
        dirs.extend(fsutil::subdirs(&profile.path.join("build")).filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.rsplit_once('-'))
                .is_some_and(|(_, h)| scan::is_hash(h))
        }));
        for dir in dirs {
            all.extend(in_dir(&dir));
        }
    }
    all.sort_by(|a, b| a.0.cmp(&b.0));
    all.dedup_by(|a, b| a.0 == b.0);
    all
}

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Build directory; defaults to the current workspace's Cargo output
    #[arg(long, value_name = "DIR")]
    pub target_dir: Option<PathBuf>,
    /// Print one JSON document
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: &Args) -> Result<()> {
    let root = match &args.target_dir {
        Some(root) => root.clone(),
        None => crate::gc::build_dir_of(&std::env::current_dir().context("current directory")?)?,
    };
    let observations = collect(&root);
    if args.json {
        return json::print(
            &serde_json::json!({"version":json::VERSION,"target_dir":root,
            "units":observations.into_iter().map(|(path, observation)| serde_json::json!({
                "path":path,"observation":observation
            })).collect::<Vec<_>>()}),
        );
    }
    println!(
        "{:>8} {:>10} {:>10} {:>10} {:>10}  CRATE",
        "CALLS", "EXECUTION", "QUEUED", "CPU", "PEAK RSS"
    );
    for (_, o) in observations {
        println!(
            "{:>8} {:>9}ms {:>9}ms {:>9}ms {:>10}  {}",
            o.calls,
            o.execution_ms,
            o.queue_ms,
            o.cpu_ms,
            o.max_rss_bytes
                .map(fsutil::human)
                .unwrap_or_else(|| "-".into()),
            o.crate_name
        );
    }
    println!(
        "Execution includes any inner cache wrapper. CPU is recorded only for direct compiler children; RSS also for compilers a cache wrapper ran itself."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cache_clients_own_peak_is_not_the_compilers() {
        let fallback = 2 << 30;
        assert_eq!(compiler_rss_through_cache(Some(6 << 20), fallback), None);
        assert_eq!(compiler_rss_through_cache(Some(fallback), fallback), None);
        assert_eq!(
            compiler_rss_through_cache(Some(19 << 30), fallback),
            Some(19 << 30)
        );
        assert_eq!(compiler_rss_through_cache(None, fallback), None);
    }

    #[test]
    fn seeded_observations_do_not_claim_the_producers_calls() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let seeded = temp.path().join("seeded");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&seeded).unwrap();
        let name = ".cargo-steward-0123456789abcdef.json";
        record(
            &source.join(name),
            "dep",
            &source,
            Duration::from_millis(50),
            Duration::from_millis(9),
            Some(400),
            Some(20),
        );
        fs::copy(source.join(name), seeded.join(name)).unwrap();
        let all = in_dir(&seeded);
        let o = &all[0].1;
        assert_eq!((o.calls, o.execution_ms, o.queue_ms), (0, 0, 0));
        assert_eq!(o.max_execution_ms, 50);
        assert_eq!(o.max_rss_bytes, Some(400));
    }

    #[test]
    fn observations_accumulate_and_seeded_counters_restart() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("cost.json");
        record(
            &file,
            "dep",
            Path::new("/a"),
            Duration::from_millis(50),
            Duration::from_millis(3),
            Some(500),
            Some(20),
        );
        record(
            &file,
            "dep",
            Path::new("/a"),
            Duration::from_millis(10),
            Duration::from_millis(2),
            None,
            Some(4),
        );
        let o = read(&file).unwrap();
        assert_eq!(
            (o.calls, o.execution_ms, o.queue_ms, o.max_execution_ms),
            (2, 60, 5, 50)
        );
        record(
            &file,
            "dep",
            Path::new("/b"),
            Duration::from_millis(4),
            Duration::ZERO,
            None,
            Some(4),
        );
        let o = read(&file).unwrap();
        assert_eq!(
            (o.calls, o.execution_ms, o.queue_ms, o.max_execution_ms),
            (1, 4, 0, 50)
        );
        assert_eq!(o.max_rss_bytes, Some(500));
    }

    #[test]
    fn bad_metadata_and_symlinks_are_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("cost.json");
        fs::write(&file, b"not json").unwrap();
        assert!(read(&file).is_none());
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(read(&link).is_none());
        assert!(path(temp.path(), Some("-../escape")).is_none());
    }
}
