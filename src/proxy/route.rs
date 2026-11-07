//! Where a cargo invocation's output goes: the repository and worktree it
//! runs in, the namespace that names them, and whether something else has
//! already chosen a target directory.
//!
//! Namespaces are named exactly as a project shim that asks
//! `git rev-parse --git-common-dir` / `--absolute-git-dir` names them, so
//! build directories made by such a shim keep working, but the git
//! directories are found by reading `.git` here rather than running git.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

/// The git directories of a checkout, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDirs {
    /// `git rev-parse --git-common-dir`: the repository's shared directory.
    pub common: PathBuf,
    /// `git rev-parse --absolute-git-dir`: this checkout's own directory
    /// (the same as `common` in the main checkout).
    pub checkout: PathBuf,
    /// The checkout's working tree: the directory holding `.git`. `None`
    /// when started inside a git directory itself.
    pub worktree: Option<PathBuf>,
}

impl GitDirs {
    /// A linked worktree (`git worktree add`), as opposed to the main one.
    pub fn linked(&self) -> bool {
        self.common != self.checkout
    }
}

/// Finds the checkout that contains `start`, as git's discovery does from
/// there: at each ancestor, a `.git` directory, or a `.git` file holding
/// `gitdir: <path>`, else the ancestor itself if it's a git directory (a
/// bare repository, or inside `.git`). A linked checkout's `commondir` file
/// names the shared directory. `None` outside a repository, or when `.git`
/// points nowhere (git refuses such a checkout too).
pub fn find_git(start: &Path) -> Option<GitDirs> {
    for dir in start.ancestors() {
        let dot_git = dir.join(".git");
        let found = match fs::metadata(&dot_git) {
            // Git skips a `.git` directory that isn't a repository.
            Ok(meta) if meta.is_dir() => is_git_dir(&dot_git).then_some(dot_git),
            Ok(_) => {
                let text = fs::read_to_string(&dot_git).ok()?;
                let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
                Some(dir.join(target))
            }
            Err(_) => None,
        };
        let (gitdir, worktree) = match found {
            Some(gitdir) => (gitdir, Some(dir.to_path_buf())),
            None if is_git_dir(dir) => (dir.to_path_buf(), None),
            None => continue,
        };
        let checkout = gitdir.canonicalize().ok()?;
        let common = match fs::read_to_string(checkout.join("commondir")) {
            Ok(text) => checkout.join(text.trim_end_matches(['\n', '\r'])),
            Err(_) => checkout.clone(),
        }
        .canonicalize()
        .ok()?;
        return Some(GitDirs {
            common,
            checkout,
            worktree,
        });
    }
    None
}

/// Whether `dir` is a git directory: `HEAD`, and `objects` and `refs` or a
/// `commondir` pointing at them.
fn is_git_dir(dir: &Path) -> bool {
    dir.join("HEAD").is_file()
        && (dir.join("commondir").is_file()
            || (dir.join("objects").is_dir() && dir.join("refs").is_dir()))
}

/// A build's place under the roots: `<repo>/<key>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Namespace {
    /// `<label>-<first 10 hex digits of sha256(identity)>`.
    pub repo: String,
    /// `main`, the linked worktree's name, or `standalone` outside git.
    pub key: String,
    /// The checkout, when in git.
    pub git: Option<GitDirs>,
}

impl Namespace {
    /// The namespace for a command run in `dir` (already resolved).
    pub fn of(dir: &Path) -> Namespace {
        match find_git(dir) {
            Some(git) => {
                let (repo, key) = names(&git.common, &git.checkout);
                Namespace {
                    repo,
                    key,
                    git: Some(git),
                }
            }
            None => Namespace {
                repo: repo_name(&file_name(dir), dir.as_os_str()),
                key: "standalone".into(),
                git: None,
            },
        }
    }

    pub fn relative(&self) -> PathBuf {
        Path::new(&self.repo).join(&self.key)
    }
}

/// Repository and key names for resolved git directories.
pub fn names(common: &Path, checkout: &Path) -> (String, String) {
    let label = if common.file_name() == Some(OsStr::new(".git")) {
        common.parent().map(file_name).unwrap_or_default()
    } else {
        file_name(common)
    };
    let key = if common == checkout {
        "main".to_string()
    } else {
        file_name(checkout)
    };
    (repo_name(&label, common.as_os_str()), key)
}

/// `<label>-<hash>`, with every character of the label outside
/// `[A-Za-z0-9_-]` replaced by `-`.
pub fn repo_name(label: &str, identity: &OsStr) -> String {
    let label: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let digest = Sha256::digest(identity.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{label}-{}", &hex[..10])
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Cargo's arguments before `--`, where its own options are.
pub fn own_args(args: &[OsString]) -> &[OsString] {
    let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    &args[..end]
}

/// The directory a command works in: the directory of `--manifest-path`
/// when given, else `cwd`. Resolved.
pub fn command_dir(cwd: &Path, args: &[OsString]) -> PathBuf {
    let mut iter = own_args(args).iter();
    while let Some(arg) = iter.next() {
        let manifest = if arg == "--manifest-path" {
            iter.next().map(PathBuf::from)
        } else {
            arg.as_bytes()
                .strip_prefix(b"--manifest-path=")
                .map(|v| PathBuf::from(OsStr::from_bytes(v)))
        };
        if let Some(manifest) = manifest {
            let path = resolve(&cwd.join(manifest));
            if let Some(parent) = path.parent() {
                return parent.to_path_buf();
            }
        }
    }
    resolve(cwd)
}

/// Resolves symlinks like Python's `Path.resolve()`: as far as the path
/// exists, then the rest lexically.
pub fn resolve(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => {
                out.push(other);
                if let Ok(real) = out.canonicalize() {
                    out = real;
                }
            }
        }
    }
    out
}

/// Why a command's target directory is already chosen, if it is: by the
/// environment, the command line, or a workspace `.cargo/config.toml` in
/// one of `dirs` or their ancestors. `cargo_home`'s own `config.toml` is
/// skipped: a machine-wide `target-dir` is what the proxy replaces.
pub fn target_dir_chosen(
    args: &[OsString],
    var: impl Fn(&str) -> Option<OsString>,
    dirs: &[&Path],
    cargo_home: Option<&Path>,
) -> Option<String> {
    for name in ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"] {
        if var(name).is_some_and(|v| !v.is_empty()) {
            return Some(format!("{name} is set"));
        }
    }
    let mut iter = own_args(args).iter();
    while let Some(arg) = iter.next() {
        let text = arg.to_string_lossy();
        if text == "--target-dir" || text.starts_with("--target-dir=") {
            return Some("--target-dir is on the command line".into());
        }
        let value = if text == "--config" {
            iter.next().map(|v| v.to_string_lossy().into_owned())
        } else {
            text.strip_prefix("--config=").map(str::to_string)
        };
        if value.is_some_and(|v| v.contains("target-dir")) {
            return Some("--config sets build.target-dir".into());
        }
    }
    let cargo_home = cargo_home.and_then(|h| h.canonicalize().ok());
    let mut seen = HashSet::new();
    for dir in dirs.iter().flat_map(|d| d.ancestors()) {
        if !seen.insert(dir) {
            continue;
        }
        let cargo_dir = dir.join(".cargo");
        if cargo_home.as_deref() == Some(cargo_dir.as_path()) {
            continue;
        }
        for name in ["config.toml", "config"] {
            let path = cargo_dir.join(name);
            if config_sets_target_dir(&path) {
                return Some(format!("{} sets build.target-dir", path.display()));
            }
        }
    }
    None
}

/// Whether a cargo config file sets `build.target-dir`. Unreadable and
/// malformed files don't (cargo reports those itself).
pub fn config_sets_target_dir(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    if !text.contains("target-dir") {
        return false;
    }
    text.parse::<toml::Table>().is_ok_and(|table| {
        table
            .get("build")
            .and_then(|b| b.as_table())
            .is_some_and(|b| b.contains_key("target-dir"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    /// A minimal git directory, as `is_git_dir` recognizes one.
    fn git_dir(path: &Path) {
        fs::create_dir_all(path.join("objects")).unwrap();
        fs::create_dir_all(path.join("refs")).unwrap();
        fs::write(path.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    }

    fn tempdir() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        (tmp, root)
    }

    /// Expected values computed with the reference shim's own code:
    /// `re.sub(r"[^a-zA-Z0-9_-]", "-", label)` and
    /// `hashlib.sha256(str(common).encode()).hexdigest()[:10]`.
    #[test]
    fn names_match_the_reference_shim() {
        let common = Path::new("/Users/justin/Workspaces/0xdiesis/diesis-core/diesis/.git");
        assert_eq!(
            names(common, common),
            ("diesis-ae0deba9a3".to_string(), "main".to_string())
        );
        let checkout = common.join("worktrees/tps-harness-20261105");
        assert_eq!(
            names(common, &checkout),
            (
                "diesis-ae0deba9a3".to_string(),
                "tps-harness-20261105".to_string()
            )
        );
        // Every non-matching character, including non-ASCII, becomes one `-`.
        let odd = Path::new("/srv/my.repo v2\u{e9}/.git");
        assert_eq!(names(odd, odd).0, "my-repo-v2--1db38ed163");
        // A bare or submodule common directory is labeled by its own name.
        let bare = Path::new("/srv/modules/sub.git");
        assert_eq!(names(bare, bare).0, "sub-git-cbfdaf2bc7");
        // Outside git: the directory's name and path.
        assert_eq!(
            repo_name("scratch-proj", OsStr::new("/home/u/scratch-proj")),
            "scratch-proj-3a7939a892"
        );
    }

    #[test]
    fn finds_a_normal_repository() {
        let (_tmp, root) = tempdir();
        let repo = root.join("repo");
        git_dir(&repo.join(".git"));
        fs::create_dir_all(repo.join("crates/a/src")).unwrap();
        let git = find_git(&repo.join("crates/a/src")).unwrap();
        assert_eq!(git.common, repo.join(".git"));
        assert_eq!(git.checkout, repo.join(".git"));
        assert_eq!(git.worktree, Some(repo.clone()));
        assert!(!git.linked());
        let ns = Namespace::of(&repo.join("crates/a"));
        assert_eq!(ns.key, "main");
        assert_eq!(ns.repo, repo_name("repo", repo.join(".git").as_os_str()));
    }

    #[test]
    fn finds_a_linked_worktree() {
        let (_tmp, root) = tempdir();
        let common = root.join("repo/.git");
        let admin = common.join("worktrees/feature-x");
        git_dir(&common);
        fs::create_dir_all(&admin).unwrap();
        fs::write(admin.join("HEAD"), "ref: refs/heads/x\n").unwrap();
        fs::write(admin.join("commondir"), "../..\n").unwrap();
        // The worktree's directory name differs from its admin name: the
        // key is the admin name, as `--absolute-git-dir` reports.
        let wt = root.join("checkouts/x");
        fs::create_dir_all(wt.join("src")).unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        let git = find_git(&wt.join("src")).unwrap();
        assert_eq!(git.common, common);
        assert_eq!(git.checkout, admin);
        assert_eq!(git.worktree, Some(wt.clone()));
        assert!(git.linked());
        let ns = Namespace::of(&wt);
        assert_eq!(ns.key, "feature-x");
        assert_eq!(ns.repo, repo_name("repo", common.as_os_str()));
    }

    #[test]
    fn finds_a_submodule_through_a_relative_gitdir() {
        let (_tmp, root) = tempdir();
        let modules = root.join("super/.git/modules/sub");
        git_dir(&modules);
        let sub = root.join("super/sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join(".git"), "gitdir: ../.git/modules/sub\n").unwrap();
        let git = find_git(&sub).unwrap();
        assert_eq!(git.common, modules);
        assert_eq!(git.checkout, modules);
        assert_eq!(git.worktree, Some(sub.clone()));
        let ns = Namespace::of(&sub);
        assert_eq!(ns.key, "main");
        assert_eq!(ns.repo, repo_name("sub", modules.as_os_str()));
        // Inside the git directory itself, git treats it as a bare
        // repository.
        let inside = find_git(&modules.join("refs")).unwrap();
        assert_eq!(inside.common, modules);
        assert_eq!(inside.worktree, None);
        assert_eq!(Namespace::of(&modules.join("refs")).repo, ns.repo);
    }

    #[test]
    fn outside_git_or_with_a_dangling_gitdir_is_standalone() {
        let (_tmp, root) = tempdir();
        let dir = root.join("loose");
        fs::create_dir_all(&dir).unwrap();
        // A `.git` directory that isn't a repository is skipped, as git does.
        fs::create_dir_all(root.join("loose/.git")).unwrap();
        let ns = Namespace::of(&dir);
        if ns.git.is_none() {
            assert_eq!(ns.key, "standalone");
            assert_eq!(ns.repo, repo_name("loose", dir.as_os_str()));
        }
        let broken = root.join("broken");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join(".git"), "gitdir: /nonexistent/x\n").unwrap();
        let ns = Namespace::of(&broken);
        assert_eq!(ns.key, "standalone");
        assert!(ns.git.is_none());
    }

    #[test]
    fn manifest_path_sets_the_command_directory() {
        let (_tmp, root) = tempdir();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/Cargo.toml"), "").unwrap();
        let cwd = root.join("a");
        assert_eq!(command_dir(&cwd, &os(&["build"])), cwd);
        assert_eq!(
            command_dir(&cwd, &os(&["build", "--manifest-path", "b/Cargo.toml"])),
            root.join("a/b")
        );
        assert_eq!(
            command_dir(
                &cwd,
                &os(&["+nightly", "test", "--manifest-path=b/Cargo.toml"])
            ),
            root.join("a/b")
        );
        // After `--` it belongs to the program being run.
        assert_eq!(
            command_dir(&cwd, &os(&["run", "--", "--manifest-path", "b/Cargo.toml"])),
            cwd
        );
        // A path that doesn't exist yet still names its directory.
        assert_eq!(
            command_dir(
                &cwd,
                &os(&["build", "--manifest-path", "new/../c/Cargo.toml"])
            ),
            root.join("a/c")
        );
        // A missing value is cargo's error to report.
        assert_eq!(command_dir(&cwd, &os(&["build", "--manifest-path"])), cwd);
    }

    #[test]
    fn detects_a_target_dir_chosen_elsewhere() {
        let (_tmp, root) = tempdir();
        let ws = root.join("ws/crate");
        fs::create_dir_all(&ws).unwrap();
        let none = |_: &str| None;
        assert_eq!(target_dir_chosen(&os(&["build"]), none, &[&ws], None), None);

        let env = |k: &str| (k == "CARGO_TARGET_DIR").then(|| OsString::from("/x"));
        assert!(target_dir_chosen(&os(&["build"]), env, &[&ws], None).is_some());
        let empty = |k: &str| (k == "CARGO_TARGET_DIR").then(OsString::new);
        assert_eq!(
            target_dir_chosen(&os(&["build"]), empty, &[&ws], None),
            None
        );
        let build_env = |k: &str| (k == "CARGO_BUILD_TARGET_DIR").then(|| OsString::from("/x"));
        assert!(target_dir_chosen(&os(&["build"]), build_env, &[&ws], None).is_some());

        for args in [
            &["build", "--target-dir", "/x"][..],
            &["build", "--target-dir=/x"],
            &["--config", "build.target-dir='/x'", "build"],
            &["--config=build.target-dir=\"/x\"", "build"],
        ] {
            assert!(
                target_dir_chosen(&os(args), none, &[&ws], None).is_some(),
                "{args:?}"
            );
        }
        assert_eq!(
            target_dir_chosen(
                &os(&["run", "--", "--target-dir", "/x"]),
                none,
                &[&ws],
                None
            ),
            None
        );

        // Other build settings, or target-dir elsewhere, don't count.
        fs::create_dir_all(root.join("ws/.cargo")).unwrap();
        let config = root.join("ws/.cargo/config.toml");
        fs::write(&config, "[build]\njobs = 4\n[env]\ntarget-dir = \"x\"\n").unwrap();
        assert_eq!(target_dir_chosen(&os(&["build"]), none, &[&ws], None), None);
        fs::write(&config, "[build]\ntarget-dir = \"/x\"\n").unwrap();
        assert!(target_dir_chosen(&os(&["build"]), none, &[&ws], None).is_some());
        fs::write(&config, "build.target-dir = \"/x\"\n").unwrap();
        assert!(target_dir_chosen(&os(&["build"]), none, &[&ws], None).is_some());
        // The legacy name counts too.
        fs::remove_file(&config).unwrap();
        fs::write(
            root.join("ws/.cargo/config"),
            "[build]\ntarget-dir = \"/x\"\n",
        )
        .unwrap();
        assert!(target_dir_chosen(&os(&["build"]), none, &[&ws], None).is_some());
        // ...unless it's CARGO_HOME's machine-wide config.
        assert_eq!(
            target_dir_chosen(&os(&["build"]), none, &[&ws], Some(&root.join("ws/.cargo"))),
            None
        );
    }

    #[test]
    fn resolves_like_python() {
        let (_tmp, root) = tempdir();
        fs::create_dir_all(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        assert_eq!(resolve(&root.join("link")), root.join("real"));
        assert_eq!(
            resolve(&root.join("link/missing/../other")),
            root.join("real/other")
        );
    }
}
