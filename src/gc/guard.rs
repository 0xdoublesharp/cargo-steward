//! The process guard: cleanup never removes a directory that a running
//! program was started from. A test binary in `deps/`, a server run from
//! `target/debug/`, or a tool `cargo install`ed from a build directory keeps
//! running after its file is deleted, but loses its symbolized backtraces,
//! and anything that restarts it breaks.
//!
//! Running programs are listed once per cleanup ([`Running::collect`]) and
//! each directory is checked against that list.

use std::path::{Path, PathBuf};

/// A running process and the executable it was started from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub exe: PathBuf,
}

/// The processes running when a cleanup started. Empty when the guard is
/// off or the platform can't list them.
#[derive(Debug, Default)]
pub struct Running {
    processes: Vec<Process>,
}

impl Running {
    /// Lists every process this user can see, with its executable. Others'
    /// processes that the system won't describe are left out.
    pub fn collect() -> Running {
        Running { processes: list() }
    }

    #[cfg(test)]
    pub fn of(processes: Vec<Process>) -> Running {
        Running { processes }
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

#[cfg(target_os = "macos")]
fn list() -> Vec<Process> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    // SAFETY: a null buffer of size 0 asks only for the number of processes.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let Ok(count) = usize::try_from(count) else {
        return Vec::new();
    };
    // Room for processes started between the two calls.
    let mut pids: Vec<libc::pid_t> = vec![0; count + 64];
    let Ok(bytes) = libc::c_int::try_from(pids.len() * size_of::<libc::pid_t>()) else {
        return Vec::new();
    };
    // SAFETY: `pids` is writable for `bytes` bytes and outlives the call.
    let found = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(usize::try_from(found).unwrap_or(0));

    let mut processes = Vec::with_capacity(pids.len());
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    for pid in pids {
        let Ok(id) = u32::try_from(pid) else {
            continue;
        };
        if id == 0 {
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
    processes
}

#[cfg(target_os = "linux")]
fn list() -> Vec<Process> {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    const DELETED: &[u8] = b" (deleted)";
    crate::fsutil::entries(Path::new("/proc"))
        .filter_map(|(name, path)| {
            let pid: u32 = name.parse().ok()?;
            let exe = std::fs::read_link(path.join("exe")).ok()?;
            // A replaced executable still names where it ran from.
            let bytes = exe.as_os_str().as_bytes();
            let exe = match bytes.strip_suffix(DELETED) {
                Some(kept) => PathBuf::from(OsString::from_vec(kept.to_vec())),
                None => exe,
            };
            Some(Process { pid, exe })
        })
        .collect()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn list() -> Vec<Process> {
    Vec::new()
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
