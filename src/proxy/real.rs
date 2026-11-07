//! Finding the real cargo behind the proxy.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The file a path names, following symlinks, as `(device, inode)`.
fn identity(path: &Path) -> Option<(u64, u64)> {
    fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Where to look for the real cargo.
pub struct Search<'a> {
    /// The `real-cargo` setting.
    pub configured: Option<&'a Path>,
    /// `PATH`.
    pub path: Option<&'a OsStr>,
    /// `$CARGO_HOME`, or its default.
    pub cargo_home: Option<&'a Path>,
    /// This executable; candidates that are the proxy itself are skipped.
    pub me: Option<&'a Path>,
}

impl Search<'_> {
    /// Candidates in order: the `real-cargo` setting, each `cargo` on PATH,
    /// `$CARGO_HOME/bin/cargo`, then the `cargo` next to rustup. Returns the
    /// first that is an executable file other than the proxy. Paths are
    /// returned as found, not canonicalized: rustup's `cargo` is a symlink
    /// to `rustup`, which dispatches on the name it's run as.
    pub fn find(&self) -> Option<PathBuf> {
        let me = self.me.and_then(identity);
        let usable = |p: &Path| is_executable(p) && (me.is_none() || identity(p) != me);
        if let Some(configured) = self.configured
            && usable(configured)
        {
            return Some(configured.to_path_buf());
        }
        let dirs: Vec<PathBuf> = self
            .path
            .map(|p| {
                env::split_paths(p)
                    .map(|d| {
                        if d.as_os_str().is_empty() {
                            PathBuf::from(".")
                        } else {
                            d
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(found) = dirs.iter().map(|d| d.join("cargo")).find(|p| usable(p)) {
            return Some(found);
        }
        if let Some(home) = self.cargo_home {
            let candidate = home.join("bin/cargo");
            if usable(&candidate) {
                return Some(candidate);
            }
        }
        let rustup = dirs
            .iter()
            .map(|d| d.join("rustup"))
            .find(|p| is_executable(p))?;
        let candidate = rustup.canonicalize().ok()?.with_file_name("cargo");
        usable(&candidate).then_some(candidate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn exe(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn skips_the_proxy_and_keeps_symlink_names() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let me = root.join("steward/cargo-steward");
        exe(&me);
        let shim_bin = root.join("shim");
        fs::create_dir_all(&shim_bin).unwrap();
        symlink(&me, shim_bin.join("cargo")).unwrap();
        // rustup's layout: `cargo` is a symlink to `rustup`.
        let rustup_bin = root.join("cargo-home/bin");
        exe(&rustup_bin.join("rustup"));
        symlink("rustup", rustup_bin.join("cargo")).unwrap();
        let not_exec = root.join("noexec");
        fs::create_dir_all(&not_exec).unwrap();
        fs::write(not_exec.join("cargo"), "").unwrap();

        let path = env::join_paths([&shim_bin, &not_exec, &rustup_bin]).unwrap();
        let search = Search {
            configured: None,
            path: Some(&path),
            cargo_home: None,
            me: Some(&me),
        };
        assert_eq!(search.find(), Some(rustup_bin.join("cargo")));

        // The setting wins, unless it's unusable.
        let other = root.join("other/cargo");
        exe(&other);
        let configured = Search {
            configured: Some(&other),
            ..search
        };
        assert_eq!(configured.find(), Some(other.clone()));
        let missing = root.join("missing/cargo");
        let broken = Search {
            configured: Some(&missing),
            path: Some(&path),
            cargo_home: None,
            me: Some(&me),
        };
        assert_eq!(broken.find(), Some(rustup_bin.join("cargo")));
        // A setting naming the proxy itself is skipped too.
        let itself = shim_bin.join("cargo");
        let loops = Search {
            configured: Some(&itself),
            path: Some(&path),
            cargo_home: None,
            me: Some(&me),
        };
        assert_eq!(loops.find(), Some(rustup_bin.join("cargo")));
    }

    #[test]
    fn falls_back_to_cargo_home_then_rustup() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let me = root.join("cargo-steward");
        exe(&me);
        let only_shim = root.join("shim");
        fs::create_dir_all(&only_shim).unwrap();
        symlink(&me, only_shim.join("cargo")).unwrap();
        let home = root.join("home");
        exe(&home.join("bin/cargo"));
        let path = env::join_paths([&only_shim]).unwrap();
        let search = Search {
            configured: None,
            path: Some(&path),
            cargo_home: Some(&home),
            me: Some(&me),
        };
        assert_eq!(search.find(), Some(home.join("bin/cargo")));

        // rustup on PATH through a symlink: cargo is found next to the
        // real rustup.
        let toolchain = root.join("rustup-real");
        exe(&toolchain.join("rustup"));
        exe(&toolchain.join("cargo"));
        let links = root.join("links");
        fs::create_dir_all(&links).unwrap();
        symlink(toolchain.join("rustup"), links.join("rustup")).unwrap();
        let path = env::join_paths([&only_shim, &links]).unwrap();
        let search = Search {
            configured: None,
            path: Some(&path),
            cargo_home: Some(&root.join("nowhere")),
            me: Some(&me),
        };
        assert_eq!(search.find(), Some(toolchain.join("cargo")));

        let path = env::join_paths([&only_shim]).unwrap();
        let none = Search {
            configured: None,
            path: Some(&path),
            cargo_home: None,
            me: Some(&me),
        };
        assert_eq!(none.find(), None);
    }

    #[test]
    fn an_empty_path_entry_is_the_current_directory() {
        // Tests run in the package root, which has no `cargo` or `rustup`:
        // the empty entry is searched as `.` and finds nothing there.
        assert!(!Path::new("cargo").exists() && !Path::new("rustup").exists());
        let path = std::ffi::OsString::from(":/nonexistent-dir");
        let search = Search {
            configured: None,
            path: Some(&path),
            cargo_home: None,
            me: None,
        };
        assert_eq!(search.find(), None);
    }
}
