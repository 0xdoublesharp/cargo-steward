//! The job-start marker, and the fingerprint access times that tell which
//! units a job used.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File, FileTimes};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use super::units;
use crate::fsutil::{entries, touched};
use crate::scan;

/// Written by `ci start` at the root of the build directory, removed by
/// `ci trim`.
pub const MARKER_FILE: &str = ".cargo-steward-ci-start";

/// Written by `ci start` next to the marker before it resets access times:
/// when each fingerprint directory was last used, one `<unix seconds>\t<path
/// under the build directory>` line each. Access times are put back from it
/// by `trim`, or by the next `start` when a job never got to `trim`, and
/// `gc` reads it meanwhile.
pub const SAVED_FILE: &str = ".cargo-steward-ci-atimes";

const PROBE_FILE: &str = ".cargo-steward-atime-probe";

/// Access times this close to the epoch are ones `start` set.
const RESET_WINDOW: Duration = Duration::from_secs(86_400);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Marker {
    /// When the job started, in Unix seconds.
    pub start: u64,
    /// The filesystem records reads in access times, and fingerprint access
    /// times were reset at the start.
    pub atime: bool,
}

impl Marker {
    pub fn start_time(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(self.start)
    }

    /// The marker file's contents: the start time on the first line, then
    /// `key=value` lines.
    pub fn render(&self) -> String {
        let atime = if self.atime { "updates" } else { "frozen" };
        format!("{}\natime={atime}\n", self.start)
    }

    pub fn parse(text: &str) -> Result<Marker> {
        let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
        let first = lines.next().context("empty marker")?;
        let start = first
            .parse()
            .with_context(|| format!("expected Unix seconds, found {first:?}"))?;
        // Without a recorded probe, assume the access times weren't reset.
        let mut atime = false;
        for line in lines {
            match line.split_once('=') {
                Some(("atime", "updates")) => atime = true,
                Some(("atime", "frozen")) => atime = false,
                Some(("atime", other)) => bail!("unknown atime={other:?}"),
                // Keys from later versions.
                _ => {}
            }
        }
        Ok(Marker { start, atime })
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn write(root: &Path, marker: &Marker) -> Result<()> {
    crate::config::write_atomic(&root.join(MARKER_FILE), marker.render().as_bytes())
}

/// The marker `ci start` left in `root`, if any.
pub fn read(root: &Path) -> Result<Option<Marker>> {
    let path = root.join(MARKER_FILE);
    match fs::read_to_string(&path) {
        Ok(text) => Marker::parse(&text)
            .map(Some)
            .with_context(|| format!("reading {}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Removes the marker and the saved access times.
pub fn remove(root: &Path) -> io::Result<()> {
    remove_file(&root.join(SAVED_FILE))?;
    remove_file(&root.join(MARKER_FILE))
}

fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// When each fingerprint directory was last used before `ci start` reset its
/// access times, by path; empty when nothing is saved (or it can't be read).
pub fn load_saved(root: &Path) -> BTreeMap<PathBuf, SystemTime> {
    let Ok(text) = fs::read_to_string(root.join(SAVED_FILE)) else {
        return BTreeMap::new();
    };
    text.lines()
        .filter_map(|line| {
            let (secs, rel) = line.split_once('\t')?;
            let secs: u64 = secs.parse().ok()?;
            Some((root.join(rel), UNIX_EPOCH + Duration::from_secs(secs)))
        })
        .collect()
}

/// Whether reading a file in `dir` updates its access time once that is set
/// older than its modification time, as `start` sets fingerprint files. False
/// on `noatime` mounts, or where access times can't be set or read.
pub fn probe_atime(dir: &Path) -> io::Result<bool> {
    let path = dir.join(PROBE_FILE);
    fs::write(&path, b"cargo-steward atime probe\n")?;
    let updates = (|| -> io::Result<bool> {
        if !set_atime(&path, UNIX_EPOCH)? {
            return Ok(false);
        }
        let mut contents = Vec::new();
        File::open(&path)?.read_to_end(&mut contents)?;
        Ok(fs::metadata(&path)?.accessed()? > UNIX_EPOCH + RESET_WINDOW)
    })();
    let _ = fs::remove_file(&path);
    Ok(updates.unwrap_or(false))
}

/// Sets a file's access time, keeping its modification time. False when the
/// filesystem didn't take it.
fn set_atime(path: &Path, when: SystemTime) -> io::Result<bool> {
    // Opening a file doesn't change its access time; only reading does.
    File::open(path)?.set_times(FileTimes::new().set_accessed(when))?;
    Ok(fs::metadata(path)?.accessed()? == when)
}

/// Saves when each fingerprint directory in the build directory was last
/// used ([`SAVED_FILE`]), then sets the access time of every fingerprint file
/// to the epoch. Returns how many were set.
pub fn reset_atimes(root: &Path) -> Result<usize> {
    let mut saved = String::new();
    let mut files = Vec::new();
    for profile in scan::profiles(root) {
        for dir in units::fingerprint_dirs(&profile.path) {
            let mut last_used = None;
            for (_, file) in entries(&dir) {
                if let Ok(meta) = fs::symlink_metadata(&file)
                    && meta.is_file()
                {
                    last_used = last_used.max(Some(touched(&meta)));
                    files.push(file);
                }
            }
            let rel = dir.strip_prefix(root).ok().and_then(Path::to_str);
            if let (Some(time), Some(rel)) = (last_used, rel)
                && !rel.contains(['\n', '\t'])
            {
                let secs = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
                let _ = writeln!(saved, "{secs}\t{rel}");
            }
        }
    }
    // Before anything is reset, so nothing is lost if this stops partway.
    crate::config::write_atomic(&root.join(SAVED_FILE), saved.as_bytes())?;
    Ok(files
        .iter()
        .filter(|file| set_atime(file, UNIX_EPOCH).is_ok())
        .count())
}

/// Puts back the access times `ci start` reset in every profile, from
/// [`SAVED_FILE`], and removes it. Setting access times removes nothing, so
/// profiles a build is using are included.
pub fn restore_all(root: &Path) -> io::Result<()> {
    let saved = load_saved(root);
    for profile in scan::profiles(root) {
        for dir in units::fingerprint_dirs(&profile.path) {
            restore_atimes(&dir, saved.get(&dir).copied());
        }
    }
    remove_file(&root.join(SAVED_FILE))
}

/// Gives fingerprint files in `dir` that no build read since the reset the
/// access time they had before it, as far as `saved` (the directory's last
/// use) tells, and at least their modification time, so that neither `gc` nor
/// other tools see them as untouched for decades.
pub fn restore_atimes(dir: &Path, saved: Option<SystemTime>) {
    for (_, file) in entries(dir) {
        let Ok(meta) = fs::symlink_metadata(&file) else {
            continue;
        };
        let (Ok(accessed), Ok(modified)) = (meta.accessed(), meta.modified()) else {
            continue;
        };
        if meta.is_file() && accessed < UNIX_EPOCH + RESET_WINDOW {
            let _ = set_atime(&file, saved.map_or(modified, |s| s.max(modified)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_round_trips_and_tolerates_later_keys() {
        for atime in [true, false] {
            let marker = Marker {
                start: 1_794_000_000,
                atime,
            };
            assert_eq!(Marker::parse(&marker.render()).unwrap(), marker);
        }
        assert_eq!(
            Marker::parse("  1794000000  \n\natime=updates\nfuture=1\n").unwrap(),
            Marker {
                start: 1_794_000_000,
                atime: true
            }
        );
        // An old marker without the probe result: budget only.
        assert!(!Marker::parse("1794000000\n").unwrap().atime);
        assert!(Marker::parse("").is_err());
        assert!(Marker::parse("yesterday\n").is_err());
        assert!(Marker::parse("1794000000\natime=sometimes\n").is_err());
    }

    #[test]
    fn writes_reads_and_removes_the_marker() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read(tmp.path()).unwrap(), None);
        let marker = Marker {
            start: now_secs(),
            atime: true,
        };
        write(tmp.path(), &marker).unwrap();
        assert_eq!(read(tmp.path()).unwrap(), Some(marker));
        remove(tmp.path()).unwrap();
        remove(tmp.path()).unwrap();
        assert_eq!(read(tmp.path()).unwrap(), None);
    }

    #[test]
    fn an_unreadable_marker_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        // Something other than a file in the marker's place.
        fs::create_dir(tmp.path().join(MARKER_FILE)).unwrap();
        let err = read(tmp.path()).unwrap_err();
        assert!(format!("{err:#}").contains("reading"), "{err:#}");
    }

    #[test]
    fn restoring_skips_files_it_cant_stat() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("fingerprint");
        fs::create_dir(&dir).unwrap();
        let file = dir.join("lib-x");
        fs::write(&file, b"x").unwrap();
        File::open(&file)
            .unwrap()
            .set_times(FileTimes::new().set_accessed(UNIX_EPOCH))
            .unwrap();
        // Listable but not searchable: names are read, but not metadata.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o444)).unwrap();
        assert_eq!(entries(&dir).count(), 1);
        restore_atimes(&dir, None);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(fs::metadata(&file).unwrap().accessed().unwrap(), UNIX_EPOCH);
    }

    #[test]
    fn resets_fingerprint_access_times_and_keeps_modification_times() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let legacy = root.join("debug/.fingerprint/serde-0123456789abcdef/lib-serde");
        let v2 = root.join("debug/build/serde/0123456789abcdef/fingerprint/lib-serde");
        let output = root.join("debug/deps/libserde-0123456789abcdef.rlib");
        for file in [&legacy, &v2, &output] {
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, b"x").unwrap();
        }
        let modified = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        for file in [&legacy, &v2] {
            File::open(file)
                .unwrap()
                .set_times(FileTimes::new().set_modified(modified))
                .unwrap();
        }

        assert_eq!(reset_atimes(root).unwrap(), 2);
        for file in [&legacy, &v2] {
            let meta = fs::metadata(file).unwrap();
            assert_eq!(meta.accessed().unwrap(), UNIX_EPOCH, "{}", file.display());
            assert_eq!(meta.modified().unwrap(), modified, "{}", file.display());
        }
        // Outputs aren't fingerprints.
        assert!(fs::metadata(&output).unwrap().accessed().unwrap() > UNIX_EPOCH + RESET_WINDOW);

        restore_atimes(legacy.parent().unwrap(), None);
        assert_eq!(fs::metadata(&legacy).unwrap().accessed().unwrap(), modified);
        assert_eq!(fs::metadata(&v2).unwrap().accessed().unwrap(), UNIX_EPOCH);
    }

    #[test]
    fn the_probe_leaves_nothing_behind() {
        let tmp = tempfile::tempdir().unwrap();
        // Whether it updates depends on the mount; either answer is fine.
        let _ = probe_atime(tmp.path()).unwrap();
        assert_eq!(entries(tmp.path()).count(), 0);
    }
}
