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

    let mut vars = match route_env(&config, &cargo_args, me.as_deref()) {
        Ok(vars) => vars,
        Err(e) => {
            warn(&format!("not routing this build: {e:#}"));
            Vec::new()
        }
    };
    if let Some(limit) = config.max_builds.filter(|&n| n > 0)
        && let Some(paths) = &paths
        && gate::is_build(&cargo_args)
        && let Err(e) = gate::acquire(&slot_dir(paths), limit, &mut vars)
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

/// Where the build slots' lock files live.
fn slot_dir(paths: &Paths) -> PathBuf {
    paths.home.join("slots")
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
}

impl Plan {
    /// Works out the routing for `args` run in `cwd`. Reads files but
    /// changes nothing.
    pub fn new(config: &Config, cwd: &Path, args: &[OsString]) -> Result<Plan> {
        let dir = route::command_dir(cwd, args);
        let namespace = Namespace::of(&dir);
        let real_cwd = route::resolve(cwd);
        let cargo_home = config::cargo_home();
        let skip = route::target_dir_chosen(
            args,
            |name| env::var_os(name),
            &[&real_cwd, &dir],
            cargo_home.as_deref(),
        );
        let target_root = config.target_root.as_deref().map(root).transpose()?;
        let tmp_root = config.tmp_root.as_deref().map(root).transpose()?;
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
fn route_env(
    config: &Config,
    args: &[OsString],
    me: Option<&Path>,
) -> Result<Vec<(OsString, OsString)>> {
    if config.target_root.is_none() && config.tmp_root.is_none() {
        return Ok(Vec::new());
    }
    let plan = Plan::new(config, &env::current_dir()?, args)?;
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
}
