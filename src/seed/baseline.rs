//! Which build directory a new worktree is seeded from: the main checkout's,
//! or another worktree's of the same repository.
//!
//! Candidates are found without running cargo in each worktree: a build
//! directory inside the worktree (`<worktree>/target`, a relative
//! `build.build-dir`) is at the same place in every other worktree, the
//! `cargo` proxy keeps every worktree's target directory side by side under
//! `<target-root>/<repo>-<hash>/`, and the wrapper's records say which build
//! directories each worktree's packages compiled into.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::ledger::Registry;
use crate::proxy::route;
use crate::{fsutil, scan};

use super::Dirs;

/// A build directory to seed from.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Candidate {
    /// The directory to clone into the build directory.
    pub dir: PathBuf,
    /// Its worktree's target directory, when that is separate from `dir`
    /// (`build.build-dir`): cloned into the target directory.
    pub target: Option<PathBuf>,
    /// Whose it is (`the main checkout`, `worktree feature-x`); empty for a
    /// directory named on the command line.
    pub label: String,
    /// Used first whenever a build isn't using it: the main checkout's, or
    /// the first `--from`.
    pub main: bool,
    /// When a build last used it, as far as is known.
    pub last_used: SystemTime,
}

impl Candidate {
    /// A directory named on the command line.
    pub(crate) fn given(dir: &Path, main: bool, registry: &Registry) -> Candidate {
        Candidate {
            dir: dir.to_path_buf(),
            target: None,
            label: String::new(),
            main,
            last_used: last_used(dir, registry),
        }
    }

    /// Whose it is and, for any but the preferred one, how recently it was
    /// used: `worktree feature-x, last used 5m ago`.
    pub(crate) fn about(&self) -> String {
        if self.main || self.label.is_empty() || self.last_used == UNIX_EPOCH {
            return self.label.clone();
        }
        format!(
            "{}, last used {} ago",
            self.label,
            fsutil::ago(self.last_used)
        )
    }
}

/// Candidates in the order they're tried: the preferred one (the main
/// checkout's) first, then the most recently used first. A directory listed
/// twice is kept once, where it ranks highest.
pub(crate) fn rank(mut candidates: Vec<Candidate>) -> Vec<Candidate> {
    candidates.sort_by(|a, b| b.main.cmp(&a.main).then(b.last_used.cmp(&a.last_used)));
    let mut seen = HashSet::new();
    candidates.retain(|c| seen.insert(c.dir.canonicalize().unwrap_or_else(|_| c.dir.clone())));
    candidates
}

/// When a build last used the build directory `dir`: the wrapper's record
/// of its last compile, or the newest modification time of the directories
/// a build writes into (each profile, its `deps/`, `.fingerprint/`, `build/`
/// and `incremental/`). Reads a few directory entries, never a whole tree.
pub(crate) fn last_used(dir: &Path, registry: &Registry) -> SystemTime {
    let mut newest = UNIX_EPOCH;
    let real = dir.canonicalize().ok();
    for key in [Some(dir), real.as_deref()].into_iter().flatten() {
        if let Some(info) = registry.roots.get(key) {
            newest = newest.max(UNIX_EPOCH + Duration::from_secs(info.last_used));
        }
    }
    let mut stamp = |path: &Path| {
        if let Ok(t) = fs::metadata(path).and_then(|m| m.modified()) {
            newest = newest.max(t);
        }
    };
    stamp(dir);
    for profile in scan::profiles(dir) {
        stamp(&profile.path);
        for sub in ["deps", ".fingerprint", "build", "incremental"] {
            stamp(&profile.path.join(sub));
        }
    }
    newest
}

/// One of the repository's checkouts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Worktree {
    pub path: PathBuf,
    pub main: bool,
}

/// The checkouts of the repository holding `dir`, as `git worktree list`
/// reports them, the main one first. A bare repository has no main checkout.
pub(crate) fn worktrees(dir: &Path) -> Vec<Worktree> {
    let Some(list) = super::git(dir, &["worktree", "list", "--porcelain"]) else {
        return Vec::new();
    };
    let common = route::find_git(&route::resolve(dir)).map(|g| g.common);
    parse_worktrees(&list, common.as_deref())
}

fn parse_worktrees(list: &str, common: Option<&Path>) -> Vec<Worktree> {
    let mut found = Vec::new();
    for (i, block) in list.split("\n\n").enumerate() {
        let Some(path) = block
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("worktree "))
        else {
            continue;
        };
        if block.lines().any(|l| l == "bare") {
            continue;
        }
        let path = PathBuf::from(path);
        // Git lists the git directory itself when it doesn't know where the
        // main worktree is (`git init --separate-git-dir`).
        if i == 0 && common.is_some_and(|c| route::resolve(&path) == c) {
            continue;
        }
        found.push(Worktree { main: i == 0, path });
    }
    found
}

/// The proxy's namespace directory holding `target`, and `target`'s key in
/// it, when `target` is where the proxy routes builds run in `dir`:
/// `<target-root>/<repo>-<hash>/<key>`.
fn proxy_namespace(dir: &Path, target: &Path) -> Option<(PathBuf, String)> {
    let ns = route::Namespace::of(&route::resolve(dir));
    ns.git.as_ref()?;
    let parent = target.parent()?;
    let key = target.file_name()?.to_str()?;
    (parent.file_name()? == ns.repo.as_str() && key == ns.key)
        .then(|| (parent.to_path_buf(), key.to_owned()))
}

/// The label of the build directory the proxy keeps for `key`.
fn key_label(key: &str) -> String {
    if key == "main" {
        "the main checkout".into()
    } else {
        format!("worktree {key}")
    }
}

/// The other target directories in the proxy's namespace directory `ns`
/// (`<target-root>/<repo>-<hash>`), besides `own`'s: one per worktree that
/// has built through the proxy, including worktrees since deleted.
pub(crate) fn namespace_candidates(ns: &Path, own: &str, registry: &Registry) -> Vec<Candidate> {
    let found = fsutil::entries(ns)
        .filter(|(name, path)| !name.starts_with('.') && name != own && path.is_dir())
        .map(|(name, path)| Candidate {
            last_used: last_used(&path, registry),
            dir: path,
            target: None,
            label: key_label(&name),
            main: name == "main",
        })
        .collect();
    rank(found)
}

/// The build directories the worktree holding `dir`, whose own are `mine`,
/// can be seeded from, ranked: each other worktree's, found where the same
/// layout puts them, and the main checkout's found by `cargo metadata` there
/// only when nothing else can say where it is.
pub(crate) fn discover(dir: &Path, mine: &Dirs, registry: &Registry) -> Vec<Candidate> {
    let separate = !super::same_path(&mine.build, &mine.target);
    let namespace = proxy_namespace(dir, &mine.target);
    if let Some((ns, own)) = &namespace
        && !separate
    {
        // Every worktree's directory is in the namespace: no git needed.
        return namespace_candidates(ns, own, registry);
    }

    let here = route::resolve(dir);
    let all = worktrees(dir);
    let paths: Vec<PathBuf> = all.iter().map(|w| route::resolve(&w.path)).collect();
    // This checkout: the deepest one holding `dir`, as a worktree can be
    // inside another's directory.
    let Some(top) = paths
        .iter()
        .filter(|p| here.starts_with(p))
        .max_by_key(|p| p.components().count())
        .cloned()
    else {
        return Vec::new();
    };
    let mine_build = route::resolve(&mine.build);
    let mine_target = route::resolve(&mine.target);
    let moved = |path: &Path, to: &Path| path.strip_prefix(&top).ok().map(|rel| to.join(rel));
    let recorded = recorded_roots(registry, &paths, &mine_build);

    let mut found = Vec::new();
    for (wt, path) in all.iter().zip(&paths) {
        if *path == top {
            continue;
        }
        let key = || route::find_git(path).map(|g| route::names(&g.common, &g.checkout).1);
        let target = moved(&mine_target, path).or_else(|| {
            let (ns, _) = namespace.as_ref()?;
            Some(ns.join(key()?))
        });
        let mut dirs = if separate {
            moved(&mine_build, path)
                .or_else(|| recorded.get(path).cloned())
                .map(|build| (build, target))
        } else {
            target
                .or_else(|| recorded.get(path).cloned())
                .map(|build| (build, None))
        };
        // A build directory outside the worktree that nothing recorded,
        // such as a `build-dir` template: ask cargo, in the main checkout
        // only.
        if wt.main
            && namespace.is_none()
            && moved(&mine_build, path).is_none()
            && dirs.as_ref().is_none_or(|(b, _)| !super::holds_build(b))
            && let Some(theirs) = super::cargo_dirs(
                &path.join(here.strip_prefix(&top).unwrap_or(Path::new(""))),
                None,
            )
        {
            let target = (separate && !super::same_path(&theirs.build, &theirs.target))
                .then_some(theirs.target);
            dirs = Some((theirs.build, target));
        }
        let Some((build, target)) = dirs else {
            continue;
        };
        found.push(Candidate {
            last_used: last_used(&build, registry),
            dir: build,
            target,
            label: if wt.main {
                "the main checkout".into()
            } else {
                format!(
                    "worktree {}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                )
            },
            main: wt.main,
        });
    }
    rank(found)
}

/// For each worktree in `worktrees`, the build directory its packages most
/// recently compiled into, as the wrapper recorded, other than `mine`. A
/// package belongs to the deepest worktree holding it.
fn recorded_roots(
    registry: &Registry,
    worktrees: &[PathBuf],
    mine: &Path,
) -> BTreeMap<PathBuf, PathBuf> {
    let mut best: BTreeMap<PathBuf, (u64, PathBuf)> = BTreeMap::new();
    for (root, info) in &registry.roots {
        if super::same_path(root, mine) {
            continue;
        }
        for workspace in &info.workspaces {
            let workspace = route::resolve(workspace);
            let Some(owner) = worktrees
                .iter()
                .filter(|w| workspace.starts_with(w))
                .max_by_key(|w| w.components().count())
            else {
                continue;
            };
            let entry = best
                .entry(owner.clone())
                .or_insert_with(|| (info.last_used, root.clone()));
            if info.last_used > entry.0 {
                *entry = (info.last_used, root.clone());
            }
        }
    }
    best.into_iter().map(|(w, (_, root))| (w, root)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::RootInfo;
    use std::process::Command;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn candidate(dir: &Path, main: bool, secs: u64) -> Candidate {
        Candidate {
            dir: dir.to_path_buf(),
            target: None,
            label: format!("worktree {}", dir.display()),
            main,
            last_used: at(secs),
        }
    }

    #[test]
    fn ranks_main_first_then_the_most_recently_used() {
        let ranked = rank(vec![
            candidate(Path::new("/old"), false, 100),
            candidate(Path::new("/new"), false, 300),
            candidate(Path::new("/main"), true, 1),
            candidate(Path::new("/mid"), false, 200),
            candidate(Path::new("/new"), false, 50),
        ]);
        let dirs: Vec<&Path> = ranked.iter().map(|c| c.dir.as_path()).collect();
        assert_eq!(dirs, ["/main", "/new", "/mid", "/old"].map(Path::new));
        assert_eq!(
            ranked[1].last_used,
            at(300),
            "the higher-ranked copy is kept"
        );
    }

    #[test]
    fn describes_a_candidate() {
        let mut c = candidate(Path::new("/w"), false, 0);
        c.label = "worktree w".into();
        assert_eq!(c.about(), "worktree w");
        c.last_used = SystemTime::now();
        assert_eq!(c.about(), "worktree w, last used 0m ago");
        c.main = true;
        assert_eq!(c.about(), "worktree w");
    }

    #[test]
    fn last_use_comes_from_the_records_or_the_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let dir = root.join("target");
        fs::create_dir_all(dir.join("debug/deps")).unwrap();
        fs::create_dir_all(dir.join("debug/.fingerprint")).unwrap();
        let listed = last_used(&dir, &Registry::default());
        assert!(listed > at(0));

        let mut registry = Registry::default();
        let future = ledger_secs(SystemTime::now()) + 86_400;
        registry.roots.insert(
            dir.clone(),
            RootInfo {
                last_used: future,
                ..RootInfo::default()
            },
        );
        assert_eq!(last_used(&dir, &registry), at(future));
        assert_eq!(
            last_used(&root.join("missing"), &Registry::default()),
            at(0)
        );
    }

    fn ledger_secs(t: SystemTime) -> u64 {
        t.duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    #[test]
    fn parses_the_worktree_list() {
        let list = "worktree /r\nHEAD abc\nbranch refs/heads/main\n\n\
                    worktree /w/a\nHEAD abc\ndetached\n\n\
                    worktree /w/gone\nHEAD abc\nprunable gitdir file points to non-existent location";
        assert_eq!(
            parse_worktrees(list, None),
            [
                Worktree {
                    path: "/r".into(),
                    main: true
                },
                Worktree {
                    path: "/w/a".into(),
                    main: false
                },
                Worktree {
                    path: "/w/gone".into(),
                    main: false
                },
            ]
        );
        let bare = "worktree /r.git\nbare\n\nworktree /w/a\nHEAD abc";
        assert_eq!(
            parse_worktrees(bare, None),
            [Worktree {
                path: "/w/a".into(),
                main: false
            }]
        );
        assert!(parse_worktrees("", None).is_empty());
    }

    /// The registry says where a worktree's packages built, each package
    /// counted for the deepest worktree holding it.
    #[test]
    fn finds_recorded_build_dirs_by_worktree() {
        let info = |secs: u64, workspaces: &[&str]| RootInfo {
            last_used: secs,
            workspaces: workspaces.iter().map(PathBuf::from).collect(),
            compile_ms: 0,
        };
        let mut registry = Registry::default();
        registry
            .roots
            .insert("/b/main-old".into(), info(100, &["/r/crates/a"]));
        registry
            .roots
            .insert("/b/main".into(), info(200, &["/r/crates/a", "/r"]));
        registry
            .roots
            .insert("/b/nested".into(), info(300, &["/r/.wt/x/crates/a"]));
        registry
            .roots
            .insert("/b/mine".into(), info(400, &["/r/.wt/me"]));
        registry
            .roots
            .insert("/b/elsewhere".into(), info(500, &["/other"]));
        let worktrees: Vec<PathBuf> = ["/r", "/r/.wt/x", "/r/.wt/me"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let found = recorded_roots(&registry, &worktrees, Path::new("/b/mine"));
        assert_eq!(found.get(Path::new("/r")), Some(&PathBuf::from("/b/main")));
        assert_eq!(
            found.get(Path::new("/r/.wt/x")),
            Some(&PathBuf::from("/b/nested"))
        );
        assert_eq!(found.get(Path::new("/r/.wt/me")), None);
        assert_eq!(found.len(), 2);
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// A build directory, as cargo leaves one.
    fn built(dir: &Path) {
        fs::create_dir_all(dir.join("debug/.fingerprint")).unwrap();
        fs::write(dir.join(".rustc_info.json"), "{}").unwrap();
    }

    /// A repository with a main checkout and worktrees `a` and `b`.
    fn repo(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let a = root.join("a");
        let b = root.join("b");
        git(&repo, &["worktree", "add", "-q", a.to_str().unwrap()]);
        git(&repo, &["worktree", "add", "-q", b.to_str().unwrap()]);
        (repo, a, b)
    }

    #[test]
    fn finds_the_other_worktrees_build_dirs_at_the_same_place() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (repo, a, b) = repo(&root);
        built(&repo.join("target"));
        built(&a.join("target"));
        let mine = Dirs::one(&b.join("target"));
        let found = discover(&b, &mine, &Registry::default());
        let dirs: Vec<PathBuf> = found.iter().map(|c| c.dir.clone()).collect();
        assert_eq!(dirs, [repo.join("target"), a.join("target")]);
        assert!(found[0].main);
        assert_eq!(found[0].label, "the main checkout");
        assert_eq!(found[1].label, "worktree a");
        assert_eq!(found[1].target, None);

        // From a subdirectory, and with a separate build-dir: each pairs
        // with the same worktree's target dir.
        fs::create_dir_all(b.join("sub")).unwrap();
        let mine = Dirs {
            build: b.join("bdir"),
            target: b.join("target"),
        };
        let found = discover(&b.join("sub"), &mine, &Registry::default());
        assert_eq!(found[1].dir, a.join("bdir"));
        assert_eq!(found[1].target, Some(a.join("target")));
    }

    #[test]
    fn finds_the_proxy_namespace_siblings() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (_repo, a, b) = repo(&root);
        let ns = route::Namespace::of(&b);
        let space = root.join("targets").join(&ns.repo);
        for key in ["main", "a", "gone", ".a.steward-seed-1"] {
            built(&space.join(key));
        }
        fs::create_dir_all(space.join("b")).unwrap();

        let mine = Dirs::one(&space.join("b"));
        let found = discover(&b, &mine, &Registry::default());
        let labels: Vec<&str> = found.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels.len(), 3, "{labels:?}");
        assert_eq!(labels[0], "the main checkout");
        assert!(labels.contains(&"worktree a") && labels.contains(&"worktree gone"));

        // With a separate build-dir inside each worktree, each live
        // worktree's is paired with its target dir in the namespace.
        let mine = Dirs {
            build: b.join("bdir"),
            target: space.join("b"),
        };
        let found = discover(&b, &mine, &Registry::default());
        let a_found = found.iter().find(|c| c.label == "worktree a").unwrap();
        assert_eq!(a_found.dir, a.join("bdir"));
        assert_eq!(a_found.target, Some(space.join("a")));
        assert!(found.iter().all(|c| c.label != "worktree gone"));

        // Elsewhere than the proxy puts it, the namespace isn't looked at.
        assert_eq!(proxy_namespace(&b, &root.join("other/b")), None);
    }

    #[test]
    fn finds_build_dirs_only_the_records_know() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (_repo, a, b) = repo(&root);
        let theirs = root.join("cache/a-hash");
        built(&theirs);
        let mut registry = Registry::default();
        registry.roots.insert(
            theirs.clone(),
            RootInfo {
                last_used: 1,
                workspaces: [a.clone()].into(),
                compile_ms: 0,
            },
        );
        let mine = Dirs::one(&root.join("cache/b-hash"));
        let found = discover(&b, &mine, &registry);
        assert!(
            found
                .iter()
                .any(|c| c.dir == theirs && c.label == "worktree a"),
            "{found:?}"
        );
    }
}
