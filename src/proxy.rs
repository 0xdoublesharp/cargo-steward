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

    let plan = match env::current_dir() {
        Ok(cwd) => Plan::new(&config, &cwd, &cargo_args),
        Err(e) => Err(e.into()),
    };
    let mut vars = Vec::new();
    match &plan {
        Ok(plan) => match route_env(plan, me.as_deref()) {
            Ok(routed) => vars = routed,
            Err(e) => warn(&format!("not routing this build: {e:#}")),
        },
        Err(e) => warn(&format!("not routing this build: {e:#}")),
    }
    if let Ok(plan) = &plan
        && let Some(limit) = plan.max_builds.filter(|&n| n > 0)
        && let Some(paths) = &paths
        && gate::is_build(&cargo_args)
        && let Err(e) = gate::acquire(&slot_dir(paths, &plan.slots), limit, &mut vars)
    {
        warn(&format!("running without a build slot: {e}"));
    }

    let mut command = Command::new(&real);
    command.arg0("cargo").args(&cargo_args).envs(vars);
    let err = command.exec();
    eprintln!(
        "error: cargo-steward: failed to run {}: {err}",
        real.display()
    );
    process::exit(127);
}

/// One warning line on stderr.
fn warn(message: &str) {
    eprintln!("warning: cargo-steward: {}", message.replace('\n', " "));
}

/// The settings; the defaults when there are none or they can't be read.
fn settings(paths: Option<&Paths>, warn_if_broken: bool) -> Config {
    match paths.map(config::load) {
        Some(Ok(config)) => config,
        Some(Err(e)) => {
            if warn_if_broken {
                warn(&format!("ignoring its settings ({e}); using the defaults"));
            }
            Config::default()
        }
        None => Config::default(),
    }
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
    }
    if let Some(tmp) = &plan.tmp {
        fs::create_dir_all(tmp)?;
    }
    Ok(plan.vars())
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
        if let Some(home) = env::var_os("HOME") {
            assert_eq!(
                root(Path::new("~/x")).unwrap(),
                PathBuf::from(home).join("x")
            );
        }
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
}
