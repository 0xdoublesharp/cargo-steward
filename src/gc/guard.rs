//! The process guard: cleanup never removes a directory that a running
//! program was started from. A test binary in `deps/`, a server run from
//! `target/debug/`, or a tool `cargo install`ed from a build directory keeps
//! running after its file is deleted, but loses its symbolized backtraces,
//! and anything that restarts it breaks.
//!
//! Running programs are listed once per cleanup ([`Running::collect`]) and
//! each directory is checked against that list. The same list tells which
//! build units a running program was started from ([`UnitExe`]), whose
//! replaced object files its backtraces may still need.

use std::cell::OnceCell;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// A running process and the executable it was started from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub exe: PathBuf,
}

/// The processes running when a cleanup started. Empty, and not
/// [`listed`](Running::listed), when the guard is off or the platform can't
/// list them.
#[derive(Debug, Default)]
pub struct Running {
    processes: Vec<Process>,
    listed: bool,
}

impl Running {
    /// Lists every process this user can see, with its executable. Others'
    /// processes that the system won't describe are left out.
    pub fn collect() -> Running {
        Running::from(list(None))
    }

    /// Lists only the processes that may have been started from `exe` or a
    /// copy of it, judged by the name the system keeps for each process,
    /// which is cheaper to read than its executable's path.
    pub fn collect_for(exe: &UnitExe) -> Running {
        Running::from(list(Some(&|short: &[u8]| exe.may_be_named(short))))
    }

    fn from(processes: Option<Vec<Process>>) -> Running {
        Running {
            listed: processes.is_some(),
            processes: processes.unwrap_or_default(),
        }
    }

    #[cfg(test)]
    pub fn of(processes: Vec<Process>) -> Running {
        Running {
            processes,
            listed: true,
        }
    }

    /// Whether the processes were listed, so that an empty list means
    /// nothing is running rather than that nobody looked.
    pub fn listed(&self) -> bool {
        self.listed
    }

    /// A process started from the unit's executable or a copy of it.
    pub fn started_from(&self, exe: &UnitExe) -> Option<&Process> {
        self.processes.iter().find(|p| exe.started(p))
    }

    /// A process whose executable is inside `dir` (compared whole path
    /// components at a time, and by `dir`'s resolved path too, since the
    /// system reports executables by their real paths).
    pub fn inside(&self, dir: &Path) -> Option<&Process> {
        if self.processes.is_empty() {
            return None;
        }
        let resolved = dir.canonicalize().ok().filter(|r| r != dir);
        self.processes.iter().find(|p| {
            p.exe.starts_with(dir) || resolved.as_ref().is_some_and(|r| p.exe.starts_with(r))
        })
    }

    #[cfg(test)]
    pub fn processes(&self) -> &[Process] {
        &self.processes
    }
}

/// How much of an executable's file name the system keeps as a process's
/// name (macOS's `MAXCOMLEN`, Linux's `TASK_COMM_LEN` less its NUL).
const SHORT_NAME: usize = 15;

/// A filter on the name the system keeps for a process (at most
/// [`SHORT_NAME`] bytes of its executable's file name, or all of it).
type NameFilter<'a> = &'a dyn Fn(&[u8]) -> bool;

/// Every process that passes `wanted`, or `None` if they can't be listed.
#[cfg(target_os = "macos")]
fn list(wanted: Option<NameFilter>) -> Option<Vec<Process>> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let pids = crate::proxy::process_ids()?;
    let mut processes = Vec::with_capacity(pids.len());
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    for pid in pids {
        let Ok(id) = u32::try_from(pid) else {
            continue;
        };
        if id == 0 {
            continue;
        }
        if let Some(wanted) = wanted
            && !short_name(pid).is_some_and(|name| wanted(&name))
        {
            continue;
        }
        // SAFETY: `buf` is writable for its full length, which is passed,
        // and outlives the call.
        let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        let Ok(len) = usize::try_from(len) else {
            continue;
        };
        if len == 0 || len > buf.len() {
            continue;
        }
        processes.push(Process {
            pid: id,
            exe: PathBuf::from(OsString::from_vec(buf[..len].to_vec())),
        });
    }
    Some(processes)
}

/// The name macOS keeps for a process: the first [`SHORT_NAME`] bytes of
/// its executable's file name. Much cheaper than the executable's path.
#[cfg(target_os = "macos")]
fn short_name(pid: libc::pid_t) -> Option<Vec<u8>> {
    // SAFETY: `proc_bsdshortinfo` is plain data, for which all zeroes is
    // valid.
    let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(size_of::<libc::proc_bsdshortinfo>()).ok()?;
    // SAFETY: `info` is writable for `size` bytes and outlives the call.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    (written == size).then(|| {
        info.pbsi_comm
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect()
    })
}

#[cfg(target_os = "linux")]
fn list(wanted: Option<NameFilter>) -> Option<Vec<Process>> {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    const DELETED: &[u8] = b" (deleted)";
    if !Path::new("/proc/self/exe").exists() {
        return None;
    }
    let processes = crate::fsutil::entries(Path::new("/proc"))
        .filter_map(|(name, path)| {
            let pid: u32 = name.parse().ok()?;
            let exe = std::fs::read_link(path.join("exe")).ok()?;
            // A replaced executable still names where it ran from.
            let bytes = exe.as_os_str().as_bytes();
            let exe = match bytes.strip_suffix(DELETED) {
                Some(kept) => PathBuf::from(OsString::from_vec(kept.to_vec())),
                None => exe,
            };
            if let Some(wanted) = wanted {
                let file = exe.file_name().map(OsStrExt::as_bytes).unwrap_or_default();
                if !wanted(file) {
                    return None;
                }
            }
            Some(Process { pid, exe })
        })
        .collect();
    Some(processes)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn list(_wanted: Option<NameFilter>) -> Option<Vec<Process>> {
    None
}

/// A build unit's executable (a binary, test or example), to tell which
/// running processes were started from it: from the file itself in the
/// unit's output directory, through a hard link, or from the copy cargo
/// uplifts into the profile directory (named after the target, the same
/// size).
#[derive(Debug, Clone)]
pub struct UnitExe {
    /// `<output directory>/<unit>`, e.g. `deps/app-0123456789abcdef` or,
    /// in the new layout, `build/app/<hash>/out/app`.
    path: PathBuf,
    /// The output directory's resolved path, where it differs; found when
    /// first needed.
    resolved: OnceCell<Option<PathBuf>>,
    /// The crate name the unit's name starts with: what an uplifted copy is
    /// named, with `-` for `_`.
    crate_name: String,
    /// The executable as it was each time it was looked at: a compile
    /// replaces it, and a running copy may be of the one it replaced.
    seen: Vec<FileId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileId {
    dev: u64,
    ino: u64,
    len: u64,
}

impl UnitExe {
    /// The executable of `unit` in `dir`, as it is now.
    pub fn new(dir: &Path, unit: &str) -> UnitExe {
        let crate_name = match unit.rsplit_once('-') {
            Some((name, hash)) if crate::scan::is_hash(hash) => name,
            _ => unit,
        };
        let mut exe = UnitExe {
            path: dir.join(unit),
            resolved: OnceCell::new(),
            crate_name: normalize(crate_name.as_bytes()),
            seen: Vec::new(),
        };
        exe.look();
        exe
    }

    /// Records the executable as it is now, keeping what it was before.
    pub fn look(&mut self) {
        if let Ok(meta) = fs::metadata(&self.path)
            && meta.is_file()
        {
            let id = FileId {
                dev: meta.dev(),
                ino: meta.ino(),
                len: meta.len(),
            };
            if !self.seen.contains(&id) {
                self.seen.push(id);
            }
        }
    }

    /// Whether the unit has had an executable: libraries have none, so no
    /// process can have been started from them.
    pub fn exists(&self) -> bool {
        !self.seen.is_empty()
    }

    fn file_name(&self) -> &[u8] {
        use std::os::unix::ffi::OsStrExt;
        self.path
            .file_name()
            .map(OsStrExt::as_bytes)
            .unwrap_or_default()
    }

    /// Whether a process the system names `short` may have been started
    /// from this executable or an uplifted copy: `short` is its file name or
    /// the crate's, or the start of one when cut to [`SHORT_NAME`] bytes.
    fn may_be_named(&self, short: &[u8]) -> bool {
        let short = normalize(short);
        [normalize(self.file_name()), self.crate_name.clone()]
            .iter()
            .any(|name| *name == short || (short.len() >= SHORT_NAME && name.starts_with(&short)))
    }

    /// Whether `process` was started from this executable: from its path in
    /// the output directory, from a hard link to it, or from a copy of it
    /// named after the crate.
    fn started(&self, process: &Process) -> bool {
        let Some(name) = self.path.file_name() else {
            return false;
        };
        let resolved = self.resolved.get_or_init(|| {
            let dir = self.path.parent()?;
            dir.canonicalize().ok().filter(|r| r != dir)
        });
        if process.exe == self.path
            || resolved
                .as_ref()
                .is_some_and(|dir| process.exe == dir.join(name))
        {
            return true;
        }
        let Some(file) = process.exe.file_name() else {
            return false;
        };
        use std::os::unix::ffi::OsStrExt;
        if file != name && normalize(file.as_bytes()) != self.crate_name {
            return false;
        }
        let Ok(meta) = fs::metadata(&process.exe) else {
            return false;
        };
        let same_file = |id: &FileId| id.dev == meta.dev() && id.ino == meta.ino();
        let same_size = |id: &FileId| meta.is_file() && id.len == meta.len();
        self.seen.iter().any(|id| same_file(id) || same_size(id))
    }
}

/// A name with `-` read as `_`, as cargo turns target names into crate
/// names.
fn normalize(name: &[u8]) -> String {
    String::from_utf8_lossy(name).replace('-', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(exes: &[&str]) -> Running {
        Running::of(
            exes.iter()
                .enumerate()
                .map(|(i, exe)| Process {
                    pid: i as u32 + 100,
                    exe: PathBuf::from(exe),
                })
                .collect(),
        )
    }

    #[test]
    fn matches_whole_path_components() {
        let running = running(&["/b/target/debug/deps/app-0123456789abcdef", "/usr/bin/vim"]);
        let found = running.inside(Path::new("/b/target")).unwrap();
        assert_eq!(found.pid, 100);
        assert!(running.inside(Path::new("/b/target/debug/deps")).is_some());
        // A sibling whose name only starts the same way isn't inside.
        assert!(running.inside(Path::new("/b/targ")).is_none());
        assert!(running.inside(Path::new("/b/target2")).is_none());
        assert!(running.inside(Path::new("/b/target/release")).is_none());
        assert!(Running::default().inside(Path::new("/")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn matches_a_directory_reached_through_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().canonicalize().unwrap().join("real");
        std::fs::create_dir_all(real.join("debug")).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let exe = real.join("debug/app");
        let running = running(&[exe.to_str().unwrap()]);
        assert!(running.inside(&link).is_some());
        assert!(running.inside(&link.join("debug")).is_some());
    }

    fn process(pid: u32, exe: &Path) -> Process {
        Process {
            pid,
            exe: exe.to_path_buf(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn tells_programs_started_from_a_units_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().canonicalize().unwrap().join("debug");
        let deps = profile.join("deps");
        fs::create_dir_all(profile.join("examples")).unwrap();
        fs::create_dir_all(&deps).unwrap();
        let exe = deps.join("my_app-0123456789abcdef");
        fs::write(&exe, "the binary").unwrap();
        let unit = UnitExe::new(&deps, "my_app-0123456789abcdef");
        assert!(unit.exists());
        let found = |exe: &Path| {
            Running::of(vec![process(9, exe)])
                .started_from(&unit)
                .is_some()
        };

        // The executable itself, also through a symlinked directory.
        assert!(found(&exe));
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&profile, &link).unwrap();
        let via_link = UnitExe::new(&link.join("deps"), "my_app-0123456789abcdef");
        assert!(
            Running::of(vec![process(9, &exe)])
                .started_from(&via_link)
                .is_some()
        );
        // Cargo's uplifted copy, named after the target: a hard link, or a
        // copy of the same size.
        let uplifted = profile.join("my-app");
        fs::hard_link(&exe, &uplifted).unwrap();
        assert!(found(&uplifted));
        fs::remove_file(&uplifted).unwrap();
        fs::write(&uplifted, "the binarY").unwrap();
        assert!(found(&uplifted));
        // Not a copy of it: another size, or another name.
        fs::write(&uplifted, "another binary").unwrap();
        assert!(!found(&uplifted));
        let other = profile.join("other");
        fs::write(&other, "the binary").unwrap();
        assert!(!found(&other));
        // Another unit's executable in the same directory.
        let sibling = deps.join("my_app-fedcba9876543210");
        fs::write(&sibling, "the binary").unwrap();
        assert!(!found(&sibling));
        // A process listing that found nothing.
        assert!(Running::of(Vec::new()).started_from(&unit).is_none());
    }

    #[test]
    fn remembers_the_executable_a_compile_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let deps = tmp.path().join("deps");
        fs::create_dir_all(&deps).unwrap();
        // A library unit has no executable.
        assert!(!UnitExe::new(&deps, "lib-0123456789abcdef").exists());

        let exe = deps.join("app-0123456789abcdef");
        fs::write(&exe, "old").unwrap();
        let mut unit = UnitExe::new(&deps, "app-0123456789abcdef");
        fs::remove_file(&exe).unwrap();
        fs::write(&exe, "the new one").unwrap();
        unit.look();
        // A copy of the old one still counts, and so does one of the new.
        let copy = tmp.path().join("app");
        fs::write(&copy, "OLD").unwrap();
        assert!(
            Running::of(vec![process(9, &copy)])
                .started_from(&unit)
                .is_some()
        );
        fs::write(&copy, "THE NEW ONE").unwrap();
        assert!(
            Running::of(vec![process(9, &copy)])
                .started_from(&unit)
                .is_some()
        );
    }

    #[test]
    fn matches_the_short_names_the_system_keeps() {
        let unit = UnitExe::new(Path::new("/b/deps"), "my_long_crate_name-0123456789abcdef");
        // Cut to 15 bytes, of the unit's file name or the crate's.
        assert!(unit.may_be_named(b"my_long_crate_n"));
        assert!(unit.may_be_named(b"my-long-crate-n"));
        assert!(unit.may_be_named(b"my-long-crate-name"));
        assert!(!unit.may_be_named(b"my_long"));
        assert!(!unit.may_be_named(b"other_long_name"));
        let short = UnitExe::new(Path::new("/b/build/app/0123456789abcdef/out"), "app");
        assert!(short.may_be_named(b"app"));
        assert!(!short.may_be_named(b"apple"));
        assert!(!short.may_be_named(b"ap"));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn lists_only_processes_that_may_be_a_units() {
        let me = std::env::current_exe().unwrap().canonicalize().unwrap();
        let dir = me.parent().unwrap();
        let name = me.file_name().unwrap().to_str().unwrap();
        let unit = UnitExe::new(dir, name);
        let running = Running::collect_for(&unit);
        assert!(running.listed());
        assert!(running.started_from(&unit).is_some());
        // This process among them; other tests may run copies of this binary
        // at the same time, which count too.
        assert!(
            running
                .processes()
                .iter()
                .any(|p| p.pid == std::process::id() && unit.started(p))
        );
        // Others are left out by name, so the list is short.
        assert!(running.processes().len() < Running::collect().processes().len());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn lists_this_process() {
        let me = std::env::current_exe().unwrap().canonicalize().unwrap();
        let running = Running::collect();
        assert!(
            running
                .processes()
                .iter()
                .any(|p| p.pid == std::process::id() && p.exe == me),
            "{me:?} not among {} processes",
            running.processes().len()
        );
    }
}
