//! The `cargo` proxy: installed as `cargo` ahead of the real one on PATH, it
//! routes each worktree's build output and temporary files, seeds new
//! worktrees, limits concurrent builds, then `exec`s the real cargo.
//!
//! It replaces per-project `cargo` shims, naming its directories exactly as
//! such a shim does (see [`route`]), so build directories they made keep
//! working. It never stops cargo from running: anything that goes wrong is
//! one warning line, and cargo runs unmodified. The only fatal error is
//! having no real cargo to run.

mod gate;
mod real;
mod route;
mod shim;

use std::env;
use std::ffi::OsString;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};

use anyhow::{Result, bail};

use crate::config::{self, Config, Paths};
use route::Namespace;

/// `cargo steward shim`.
#[derive(Debug, clap::Args)]
pub struct ShimArgs {
    #[command(subcommand)]
    command: shim::ShimCommand,
}

/// Runs `cargo steward shim <command>`.
pub fn run_shim(args: &ShimArgs) -> Result<()> {
    shim::run(&args.command)
}

/// Runs as `cargo`: `args[0]` is the program name as invoked.
pub fn run(args: Vec<OsString>) -> ! {
    let cargo_args: Vec<OsString> = args.into_iter().skip(1).collect();
    let paths = Paths::resolve();
    let config = settings(paths.as_ref(), true);
    let me = this_exe();

    let Some(real) = real_cargo(&config, me.as_deref()) else {
        eprintln!(
            "error: cargo-steward: no real cargo found behind the proxy (on PATH, in \
             $CARGO_HOME/bin, or next to rustup); set `real-cargo` in its settings"
        );
        process::exit(127);
    };

    let vars = prepare(
        &config,
        paths.as_ref(),
        env::current_dir(),
        &cargo_args,
        me.as_deref(),
    );

    let mut command = Command::new(&real);
    command.arg0("cargo").args(&cargo_args).envs(vars);
    let err = command.exec();
    eprintln!(
        "error: cargo-steward: failed to run {}: {err}",
        real.display()
    );
    process::exit(127);
}

/// Routes the build and takes a build slot, returning the variables to run
/// cargo with. Never fails: what goes wrong is a warning and less routing.
fn prepare(
    config: &Config,
    paths: Option<&Paths>,
    cwd: std::io::Result<PathBuf>,
    cargo_args: &[OsString],
    me: Option<&Path>,
) -> Vec<(OsString, OsString)> {
    let plan = match cwd {
        Ok(cwd) => Plan::new(config, &cwd, cargo_args),
        Err(e) => Err(e.into()),
    };
    let mut vars = Vec::new();
    match &plan {
        Ok(plan) => match route_env(plan, me) {
            Ok(routed) => vars = routed,
            Err(e) => warn(&format!("not routing this build: {e:#}")),
        },
        Err(e) => warn(&format!("not routing this build: {e:#}")),
    }
    if let Ok(plan) = &plan
        && let Some(limit) = plan.max_builds.filter(|&n| n > 0)
        && let Some(paths) = paths
        && gate::is_build(cargo_args)
        && let Err(e) = gate::acquire(&slot_dir(paths, &plan.slots), limit, &mut vars)
    {
        warn(&format!("running without a build slot: {e}"));
    }
    vars
}

/// One warning line on stderr.
fn warn(message: &str) {
    eprintln!("warning: cargo-steward: {}", message.replace('\n', " "));
}

/// The settings, each one that can't be used replaced by its default
/// ([`config::load_lenient`]).
fn settings(paths: Option<&Paths>, warn_if_broken: bool) -> Config {
    let Some(paths) = paths else {
        return Config::default();
    };
    let (config, problems) = config::load_lenient(paths);
    if warn_if_broken {
        for problem in problems {
            warn(&format!("settings: {problem}"));
        }
    }
    config
}

/// This executable, resolved (on macOS it may be reported as the `cargo`
/// symlink it was run through).
fn this_exe() -> Option<PathBuf> {
    env::current_exe().ok()?.canonicalize().ok()
}

/// The real cargo, as [`real::Search::find`] looks for it.
fn real_cargo(config: &Config, me: Option<&Path>) -> Option<PathBuf> {
    let path = env::var_os("PATH");
    let cargo_home = config::cargo_home();
    real::Search {
        configured: config.real_cargo.as_deref(),
        path: path.as_deref(),
        cargo_home: cargo_home.as_deref(),
        me,
    }
    .find()
}

/// Where a set of build slots' lock files live: `slots/` for the top-level
/// `max-builds`, a subdirectory of it for a route's.
fn slot_dir(paths: &Paths, set: &str) -> PathBuf {
    let slots = paths.home.join("slots");
    if set.is_empty() {
        slots
    } else {
        slots.join(set)
    }
}

/// The proxy settings for commands run in one directory: the longest
/// matching route's, else the top-level ones.
#[derive(Debug, Clone, PartialEq)]
pub struct Effective {
    pub target_root: Option<PathBuf>,
    pub tmp_root: Option<PathBuf>,
    pub max_builds: Option<u32>,
    /// Which set of build slots `max_builds` counts: empty for the
    /// top-level one, else the route's own.
    pub slots: String,
    /// The route's `path`, when one applies.
    pub route: Option<PathBuf>,
    /// Route over a target dir chosen by the environment or a workspace
    /// config.
    pub force: bool,
}

impl Effective {
    pub fn of(config: &Config, dir: &Path) -> Effective {
        let matched = config
            .routes
            .iter()
            .filter_map(|r| Some((root(&r.path).ok().map(|p| route::resolve(&p))?, r)))
            .filter(|(path, _)| dir.starts_with(path))
            .max_by_key(|(path, _)| path.components().count());
        match matched {
            Some((path, r)) => Effective {
                target_root: r.target_root.clone(),
                tmp_root: r.tmp_root.clone(),
                max_builds: r.max_builds,
                slots: format!(
                    "route-{}",
                    route::repo_name("", path.as_os_str()).trim_start_matches('-')
                ),
                route: Some(path),
                force: r.force,
            },
            None => Effective {
                target_root: config.target_root.clone(),
                tmp_root: config.tmp_root.clone(),
                max_builds: config.max_builds,
                slots: String::new(),
                route: None,
                force: false,
            },
        }
    }
}

/// What the proxy does for one invocation.
#[derive(Debug)]
pub struct Plan {
    /// The directory the command works in.
    pub dir: PathBuf,
    pub namespace: Namespace,
    /// Why nothing is routed, when it isn't.
    pub skip: Option<String>,
    /// `CARGO_TARGET_DIR`, when `target-root` is set.
    pub target: Option<PathBuf>,
    /// `TMPDIR`, `TMP` and `TEMP`, when `tmp-root` is set.
    pub tmp: Option<PathBuf>,
    /// For a build in a linked worktree with `seed` on: the main checkout's
    /// target directory, to seed a new one from.
    pub seed_from: Option<PathBuf>,
    /// The build limit that applies, and which set of slots it counts.
    pub max_builds: Option<u32>,
    pub slots: String,
    /// The route that applies, if any.
    pub route: Option<PathBuf>,
    /// Point a missing `<workspace>/target` at the routed directory.
    pub link_target: bool,
}

impl Plan {
    /// Works out the routing for `args` run in `cwd`. Reads files but
    /// changes nothing.
    pub fn new(config: &Config, cwd: &Path, args: &[OsString]) -> Result<Plan> {
        let dir = route::command_dir(cwd, args);
        let effective = Effective::of(config, &dir);
        if effective.target_root.is_none() && effective.tmp_root.is_none() {
            // Nothing to route: skip the git and config lookups.
            return Ok(Plan {
                namespace: Namespace {
                    repo: String::new(),
                    key: String::new(),
                    git: None,
                },
                dir,
                skip: Some("no target-root or tmp-root applies here".into()),
                target: None,
                tmp: None,
                seed_from: None,
                max_builds: effective.max_builds,
                slots: effective.slots,
                route: effective.route,
                link_target: false,
            });
        }
        let namespace = Namespace::of(&dir);
        let real_cwd = route::resolve(cwd);
        let cargo_home = config::cargo_home();
        let skip = if effective.force {
            // Only the command line can override a forced route.
            route::target_dir_chosen(args, |_| None, &[], None)
        } else {
            route::target_dir_chosen(
                args,
                |name| env::var_os(name),
                &[&real_cwd, &dir],
                cargo_home.as_deref(),
            )
        };
        let label = namespace
            .repo
            .rsplit_once('-')
            .map_or(namespace.repo.as_str(), |(label, _)| label);
        let expand = |path: &Path| -> Result<PathBuf> {
            let text = path.to_string_lossy().replace("{label}", label);
            root(Path::new(&text))
        };
        let target_root = effective.target_root.as_deref().map(expand).transpose()?;
        let tmp_root = effective.tmp_root.as_deref().map(expand).transpose()?;
        let target = target_root.as_ref().map(|r| r.join(namespace.relative()));
        let tmp = tmp_root.map(|r| r.join(namespace.relative()));
        let seed_from = match (&target_root, &namespace.git) {
            (Some(r), Some(git))
                if config.seed
                    && git.linked()
                    && git.worktree.is_some()
                    && gate::is_build(args) =>
            {
                Some(r.join(&namespace.repo).join("main"))
            }
            _ => None,
        };
        Ok(Plan {
            dir,
            namespace,
            skip,
            target,
            tmp,
            seed_from,
            max_builds: effective.max_builds,
            slots: effective.slots,
            link_target: config.link_target && !effective.force,
            route: effective.route,
        })
    }

    /// The environment variables to set.
    pub fn vars(&self) -> Vec<(OsString, OsString)> {
        let mut vars = Vec::new();
        if self.skip.is_some() {
            return vars;
        }
        if let Some(target) = &self.target {
            vars.push(("CARGO_TARGET_DIR".into(), target.into()));
        }
        if let Some(tmp) = &self.tmp {
            let mut with_slash = tmp.as_os_str().to_owned();
            with_slash.push("/");
            vars.push(("TMPDIR".into(), with_slash));
            vars.push(("TMP".into(), tmp.into()));
            vars.push(("TEMP".into(), tmp.into()));
        }
        vars
    }
}

/// A root setting: absolute, or under `~/`.
fn root(path: &Path) -> Result<PathBuf> {
    let path = match path.strip_prefix("~") {
        Ok(rest) => match env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => bail!("{} needs HOME", path.display()),
        },
        Err(_) => path.to_path_buf(),
    };
    if !path.is_absolute() {
        bail!("{} isn't an absolute path", path.display());
    }
    Ok(path)
}

/// Works out the routing, creates the directories, seeds a new worktree's
/// target directory, and returns the variables to set. Nothing (and no
/// subprocess) when no root is set or a target directory is already chosen.
fn route_env(plan: &Plan, me: Option<&Path>) -> Result<Vec<(OsString, OsString)>> {
    if plan.skip.is_some() {
        return Ok(Vec::new());
    }
    if let Some(target) = &plan.target {
        let fresh = is_empty(target);
        fs::create_dir_all(target)?;
        if fresh
            && let (Some(from), Some(git), Some(me)) = (&plan.seed_from, &plan.namespace.git, me)
            && let Some(worktree) = &git.worktree
            && from.is_dir()
        {
            seed(me, target, from, worktree);
        }
        if plan.link_target {
            link_workspace_target(&plan.dir, target, plan.namespace.git.is_some());
        }
    }
    if let Some(tmp) = &plan.tmp {
        fs::create_dir_all(tmp)?;
    }
    Ok(plan.vars())
}

/// Points a workspace's missing `target` at its routed directory, so
/// `./target/...` paths in scripts, and builds that don't go through the
/// proxy (Cargo's default target dir is `<workspace>/target`), reach it too.
/// Never replaces anything, and inside git only where git ignores `target`,
/// so the link can't end up committed. Best effort.
fn link_workspace_target(dir: &Path, target: &Path, in_git: bool) {
    let Some(workspace) = workspace_root(dir) else {
        return;
    };
    let link = workspace.join("target");
    if fs::symlink_metadata(&link).is_ok() {
        return;
    }
    if in_git && !git_ignores(&workspace, "target") {
        return;
    }
    #[cfg(unix)]
    let _ = std::os::unix::fs::symlink(target, &link);
}

/// The workspace root for a command run in `dir`, without running cargo:
/// the nearest ancestor whose `Cargo.toml` has a `[workspace]` table, else
/// the nearest one with a `Cargo.toml`.
fn workspace_root(dir: &Path) -> Option<PathBuf> {
    let mut nearest = None;
    for ancestor in dir.ancestors() {
        let Ok(text) = fs::read_to_string(ancestor.join("Cargo.toml")) else {
            continue;
        };
        if text.lines().any(|l| l.trim() == "[workspace]") {
            return Some(ancestor.to_path_buf());
        }
        nearest.get_or_insert_with(|| ancestor.to_path_buf());
    }
    nearest
}

/// Whether git ignores `name` in `dir`.
fn git_ignores(dir: &Path, name: &str) -> bool {
    Command::new("git")
        .args(["check-ignore", "-q", name])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Missing, or a directory with nothing in it.
fn is_empty(dir: &Path) -> bool {
    match fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => !dir.exists(),
    }
}

/// Runs `cargo-steward seed` for a new worktree's target directory and
/// waits for it. Best effort: a failed seed only means a cold build.
fn seed(me: &Path, target: &Path, from: &Path, worktree: &Path) {
    let _ = Command::new(me)
        .arg("seed")
        .arg("--target-dir")
        .arg(target)
        .arg("--from")
        .arg(from)
        .arg("--worktree")
        .arg(worktree)
        .arg("--quiet")
        .stdin(Stdio::null())
        // Cargo's stdout may be machine-read (`--message-format json`).
        .stdout(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_must_be_absolute() {
        assert_eq!(root(Path::new("/r")).unwrap(), PathBuf::from("/r"));
        assert!(root(Path::new("rel/dir")).is_err());
        // Without HOME, `~/` roots fail; covered end to end in
        // tests/plumbing.rs, as the test can't unset HOME for itself.
        let home = env::var_os("HOME").expect("tests run with HOME set");
        assert_eq!(
            root(Path::new("~/x")).unwrap(),
            PathBuf::from(home).join("x")
        );
    }

    #[test]
    fn route_env_links_the_workspace_root_target() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let ws = base.join("ws");
        fs::create_dir_all(ws.join("crates/a")).unwrap();
        fs::write(
            ws.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/a\"]\n",
        )
        .unwrap();
        fs::write(ws.join("crates/a/Cargo.toml"), "[package]\nname = \"a\"\n").unwrap();
        let target = base.join("routed/target");
        let plan = Plan {
            dir: ws.join("crates/a"),
            namespace: Namespace::of(Path::new("/")),
            skip: None,
            target: Some(target.clone()),
            tmp: None,
            seed_from: None,
            max_builds: None,
            slots: String::new(),
            route: None,
            link_target: true,
        };
        assert!(plan.namespace.git.is_none());
        let vars = route_env(&plan, None).unwrap();
        assert_eq!(var(&vars, "CARGO_TARGET_DIR"), Some(&target.clone().into()));
        assert!(target.is_dir());
        // The `[workspace]` root gets the link, not the member.
        assert_eq!(fs::read_link(ws.join("target")).unwrap(), target);
        assert!(fs::symlink_metadata(ws.join("crates/a/target")).is_err());

        // Outside any Cargo project there is nothing to link.
        let loose = base.join("loose");
        fs::create_dir_all(&loose).unwrap();
        assert_eq!(workspace_root(&loose), None);
        link_workspace_target(&loose, &target, false);
        assert!(fs::symlink_metadata(loose.join("target")).is_err());
    }

    #[test]
    fn tmpdir_gets_a_trailing_slash_and_skips_route_nothing() {
        let mut plan = Plan {
            dir: "/w".into(),
            namespace: Namespace::of(Path::new("/")),
            skip: None,
            target: Some("/t/repo-0123456789/main".into()),
            tmp: Some("/tmp-root/repo-0123456789/main".into()),
            seed_from: None,
            max_builds: None,
            slots: String::new(),
            route: None,
            link_target: false,
        };
        let vars = plan.vars();
        let get = |k: &str| {
            vars.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.to_string_lossy().into_owned())
        };
        assert_eq!(get("CARGO_TARGET_DIR").unwrap(), "/t/repo-0123456789/main");
        assert_eq!(get("TMPDIR").unwrap(), "/tmp-root/repo-0123456789/main/");
        assert_eq!(get("TMP").unwrap(), "/tmp-root/repo-0123456789/main");
        assert_eq!(get("TEMP").unwrap(), "/tmp-root/repo-0123456789/main");
        plan.skip = Some("CARGO_TARGET_DIR is set".into());
        assert!(plan.vars().is_empty());
    }

    #[test]
    fn the_longest_matching_route_wins_and_labels_expand() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let git_init = |dir: &Path| {
            std::fs::create_dir_all(dir).unwrap();
            let status = Command::new("git")
                .args(["init", "-q"])
                .current_dir(dir)
                .status()
                .unwrap();
            assert!(status.success());
        };
        let repo = base.join("work/proj");
        git_init(&repo);
        std::fs::create_dir_all(repo.join("crates/a")).unwrap();
        let config = Config {
            target_root: Some(base.join("{label}/cargo")),
            max_builds: Some(8),
            routes: vec![
                config::Route {
                    path: base.join("work"),
                    target_root: Some(base.join("work-root")),
                    tmp_root: None,
                    max_builds: Some(2),
                    force: false,
                },
                config::Route {
                    path: repo.clone(),
                    target_root: Some(base.join("proj-root")),
                    tmp_root: Some(base.join("proj-tmp")),
                    max_builds: Some(3),
                    force: false,
                },
            ],
            ..Config::default()
        };
        let none: [OsString; 0] = [];

        // Inside the repository, its own route applies.
        let plan = Plan::new(&config, &repo.join("crates/a"), &none).unwrap();
        assert_eq!(plan.route.as_deref(), Some(repo.as_path()));
        assert_eq!(plan.max_builds, Some(3));
        let text = |p: &Path| p.to_string_lossy().into_owned();
        let target = plan.target.unwrap();
        assert!(
            text(&target).starts_with(&text(&base.join("proj-root/proj-"))),
            "{target:?}"
        );
        assert!(target.ends_with("main"));
        assert!(plan.tmp.unwrap().starts_with(base.join("proj-tmp")));
        assert!(plan.slots.starts_with("route-"));

        // Elsewhere, the top-level settings, with `{label}` expanded.
        let other = base.join("elsewhere/tool");
        git_init(&other);
        let plan = Plan::new(&config, &other, &none).unwrap();
        assert_eq!(plan.route, None);
        assert_eq!(plan.max_builds, Some(8));
        assert!(plan.slots.is_empty());
        let target = plan.target.unwrap();
        assert!(
            text(&target).starts_with(&text(&base.join("tool/cargo/tool-"))),
            "{target:?}"
        );

        // A route without roots routes nothing, but keeps its own limit.
        let config = Config {
            routes: vec![config::Route {
                path: base.clone(),
                target_root: None,
                tmp_root: None,
                max_builds: Some(1),
                force: false,
            }],
            ..Config::default()
        };
        let plan = Plan::new(&config, &other, &none).unwrap();
        assert!(plan.skip.is_some() && plan.vars().is_empty());
        assert_eq!(plan.max_builds, Some(1));
    }

    #[test]
    fn a_forced_route_overrides_a_workspace_target_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let repo = base.join("proj");
        std::fs::create_dir_all(repo.join(".cargo")).unwrap();
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(
            repo.join(".cargo/config.toml"),
            "[build]\ntarget-dir = \"elsewhere\"\n",
        )
        .unwrap();
        let route = |force| config::Route {
            path: repo.clone(),
            target_root: Some(base.join("root")),
            tmp_root: None,
            max_builds: None,
            force,
        };
        let none: [OsString; 0] = [];
        let respectful = Config {
            routes: vec![route(false)],
            ..Config::default()
        };
        assert!(Plan::new(&respectful, &repo, &none).unwrap().skip.is_some());
        let forced = Config {
            routes: vec![route(true)],
            ..Config::default()
        };
        let plan = Plan::new(&forced, &repo, &none).unwrap();
        assert_eq!(plan.skip, None);
        assert!(plan.target.is_some());
        // The command line still wins.
        let args = [OsString::from("build"), OsString::from("--target-dir=x")];
        assert!(Plan::new(&forced, &repo, &args).unwrap().skip.is_some());
    }

    #[test]
    fn links_an_ignored_missing_target_to_the_routed_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let crate_at = |dir: &Path, ignore: Option<&str>| {
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(
                dir.join("Cargo.toml"),
                "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            )
            .unwrap();
            std::fs::write(dir.join("src/lib.rs"), "").unwrap();
            let status = Command::new("git")
                .args(["init", "-q"])
                .current_dir(dir)
                .status()
                .unwrap();
            assert!(status.success());
            if let Some(pattern) = ignore {
                std::fs::write(dir.join(".gitignore"), pattern).unwrap();
            }
        };
        let routed = base.join("scratch/x");
        std::fs::create_dir_all(&routed).unwrap();

        let ignored = base.join("ignored");
        crate_at(&ignored, Some("/target\n"));
        link_workspace_target(&ignored.join("src"), &routed, true);
        assert_eq!(std::fs::read_link(ignored.join("target")).unwrap(), routed);
        // A second call leaves the link alone.
        link_workspace_target(&ignored, &base.join("elsewhere"), true);
        assert_eq!(std::fs::read_link(ignored.join("target")).unwrap(), routed);

        // Not ignored by git: no link, so it can't be committed.
        let tracked = base.join("tracked");
        crate_at(&tracked, None);
        link_workspace_target(&tracked, &routed, true);
        assert!(std::fs::symlink_metadata(tracked.join("target")).is_err());

        // An existing target directory is never replaced.
        let existing = base.join("existing");
        crate_at(&existing, Some("target/\n"));
        std::fs::create_dir_all(existing.join("target/debug")).unwrap();
        link_workspace_target(&existing, &routed, true);
        assert!(existing.join("target/debug").is_dir());
        assert!(
            !std::fs::symlink_metadata(existing.join("target"))
                .unwrap()
                .is_symlink()
        );
    }

    #[test]
    fn routes_round_trip_through_the_settings_file() {
        let text = "target-root = \"/s/{label}/cargo\"\n\n[[routes]]\npath = \"/w/diesis\"\n\
                    target-root = \"/s/0xdiesis/cargo\"\nmax-builds = 3\n";
        let config: Config = toml::from_str(text).unwrap();
        assert_eq!(config.routes.len(), 1);
        assert_eq!(config.routes[0].max_builds, Some(3));
        assert_eq!(config.routes[0].tmp_root, None);
        let again: Config = toml::from_str(&toml::to_string_pretty(&config).unwrap()).unwrap();
        assert_eq!(again, config);
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// A forced route over `base`, so the test's own `CARGO_TARGET_DIR`
    /// doesn't stop the routing.
    fn forced(base: &Path, target: Option<PathBuf>, tmp: Option<PathBuf>) -> config::Route {
        config::Route {
            path: base.to_path_buf(),
            target_root: target,
            tmp_root: tmp,
            max_builds: None,
            force: true,
        }
    }

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn var<'a>(vars: &'a [(OsString, OsString)], name: &str) -> Option<&'a OsString> {
        vars.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    #[test]
    fn prepare_routes_and_creates_the_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let work = base.join("work");
        fs::create_dir_all(&work).unwrap();
        let config = Config {
            routes: vec![forced(
                &base,
                Some(base.join("targets")),
                Some(base.join("tmps")),
            )],
            ..Config::default()
        };
        let vars = prepare(&config, None, Ok(work.clone()), &os(&["build"]), None);
        let target = PathBuf::from(var(&vars, "CARGO_TARGET_DIR").unwrap());
        assert!(target.starts_with(base.join("targets")) && target.is_dir());
        let tmpdir = PathBuf::from(var(&vars, "TMP").unwrap());
        assert!(tmpdir.starts_with(base.join("tmps")) && tmpdir.is_dir());
        // No slot without `max-builds`.
        assert!(var(&vars, gate::SLOT_VAR).is_none());
    }

    #[test]
    fn prepare_never_fails_the_build() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let build = os(&["build"]);
        // No working directory: nothing routed.
        let none = prepare(
            &Config::default(),
            None,
            Err(std::io::Error::other("gone")),
            &build,
            None,
        );
        assert!(none.is_empty());

        // A root that can't be created (a file is in the way): nothing
        // routed, but cargo still runs.
        fs::write(base.join("blocked"), "").unwrap();
        let config = Config {
            routes: vec![forced(&base, Some(base.join("blocked/sub")), None)],
            ..Config::default()
        };
        assert!(prepare(&config, None, Ok(base.clone()), &build, None).is_empty());
        let config = Config {
            routes: vec![forced(&base, None, Some(base.join("blocked/sub")))],
            ..Config::default()
        };
        assert!(prepare(&config, None, Ok(base.clone()), &build, None).is_empty());

        // A relative root is an error for the plan, not for cargo.
        let config = Config {
            routes: vec![forced(&base, Some("relative".into()), None)],
            ..Config::default()
        };
        assert!(prepare(&config, None, Ok(base.clone()), &build, None).is_empty());
    }

    #[test]
    fn prepare_takes_a_build_slot_only_for_builds() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let paths = Paths {
            home: base.join("home"),
        };
        let config = Config {
            max_builds: Some(2),
            ..Config::default()
        };
        let vars = prepare(
            &config,
            Some(&paths),
            Ok(base.clone()),
            &os(&["build"]),
            None,
        );
        let slot = PathBuf::from(var(&vars, gate::SLOT_VAR).unwrap());
        assert_eq!(slot.parent(), Some(base.join("home/slots").as_path()));
        assert!(var(&vars, gate::FD_VAR).is_some());
        // Not a build, no paths, or a zero limit: no slot.
        let meta = prepare(
            &config,
            Some(&paths),
            Ok(base.clone()),
            &os(&["metadata"]),
            None,
        );
        assert!(meta.is_empty());
        assert!(prepare(&config, None, Ok(base.clone()), &os(&["build"]), None).is_empty());
        let zero = Config {
            max_builds: Some(0),
            ..Config::default()
        };
        assert!(prepare(&zero, Some(&paths), Ok(base.clone()), &os(&["build"]), None).is_empty());

        // A slot directory that can't be made: a warning, and no slot.
        fs::write(base.join("file-home"), "").unwrap();
        let broken = Paths {
            home: base.join("file-home"),
        };
        assert!(prepare(&config, Some(&broken), Ok(base), &os(&["build"]), None).is_empty());
    }

    #[test]
    fn slot_sets_and_settings_fall_back_to_defaults() {
        let paths = Paths { home: "/h".into() };
        assert_eq!(slot_dir(&paths, ""), PathBuf::from("/h/slots"));
        assert_eq!(
            slot_dir(&paths, "route-x"),
            PathBuf::from("/h/slots/route-x")
        );

        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: tmp.path().to_path_buf(),
        };
        assert_eq!(settings(None, true), Config::default());
        // Missing settings are the defaults.
        assert_eq!(settings(Some(&paths), true), Config::default());
        fs::write(paths.config(), "max-builds = 3\n").unwrap();
        assert_eq!(settings(Some(&paths), true).max_builds, Some(3));
        // Unreadable settings are the defaults too, warned about or not.
        fs::write(paths.config(), "max-builds = \"many\"\n").unwrap();
        assert_eq!(settings(Some(&paths), true), Config::default());
        assert_eq!(settings(Some(&paths), false), Config::default());
    }

    #[test]
    fn a_configured_real_cargo_is_used() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo = tmp.path().join("my-cargo");
        fs::write(&cargo, "").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
        let config = Config {
            real_cargo: Some(cargo.clone()),
            ..Config::default()
        };
        assert_eq!(real_cargo(&config, None), Some(cargo));
        assert!(this_exe().is_some_and(|p| p.is_absolute()));
    }

    #[test]
    fn a_new_linked_worktree_is_seeded_from_main() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let repo = base.join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        fs::write(repo.join("f"), "x").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);
        let wt = base.join("wt");
        git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);

        // Stands in for cargo-steward: records how `seed` was run.
        let log = base.join("seed.log");
        let me = base.join("fake-steward");
        fs::write(
            &me,
            format!("#!/bin/sh\necho \"$@\" >> '{}'\n", log.display()),
        )
        .unwrap();
        fs::set_permissions(&me, fs::Permissions::from_mode(0o755)).unwrap();

        let config = Config {
            seed: true,
            routes: vec![forced(&base, Some(base.join("targets")), None)],
            ..Config::default()
        };
        let build = os(&["build"]);
        let plan = Plan::new(&config, &wt, &build).unwrap();
        let from = plan.seed_from.clone().unwrap();
        assert!(from.ends_with("main"));

        // Main hasn't built yet: nothing to seed from.
        let vars = prepare(&config, None, Ok(wt.clone()), &build, Some(&me));
        assert!(var(&vars, "CARGO_TARGET_DIR").is_some());
        assert!(!log.exists());

        // Main has built, and the worktree's target dir is new: seeded once.
        fs::create_dir_all(&from).unwrap();
        let target = plan.target.clone().unwrap();
        fs::remove_dir(&target).unwrap();
        prepare(&config, None, Ok(wt.clone()), &build, Some(&me));
        let logged = fs::read_to_string(&log).unwrap();
        assert!(logged.starts_with("seed --target-dir "), "{logged}");
        assert!(logged.contains(&format!("--from {}", from.display())));
        assert!(logged.contains(&format!("--worktree {}", wt.display())));
        assert!(logged.trim_end().ends_with("--quiet"));

        // A target dir with files isn't seeded again.
        fs::write(target.join("keep"), "").unwrap();
        prepare(&config, None, Ok(wt), &build, Some(&me));
        assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 1);
    }
}
