//! End to end through the binary: `shim install|uninstall|status|explain`,
//! `ci start|trim` argument handling, and the rustc wrapper's exit codes,
//! inner wrapper and ledger. Every test runs with its own home directories;
//! nothing touches the real shell profiles.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_cargo-steward");

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    /// `CARGO_STEWARD_HOME`.
    home: PathBuf,
    /// `HOME`, holding the shell startup files.
    user: PathBuf,
}

impl Env {
    fn new() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("steward-home");
        let user = root.join("user");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&user).unwrap();
        Env {
            _tmp: tmp,
            root,
            home,
            user,
        }
    }

    /// `cargo-steward args...` in `dir`, isolated from this machine's setup.
    fn cmd(&self, dir: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .current_dir(dir)
            .env("CARGO_STEWARD_HOME", &self.home)
            .env("HOME", &self.user)
            .env("CARGO_HOME", self.root.join("cargo-home"))
            .env("SHELL", "/bin/zsh");
        for var in [
            "ZDOTDIR",
            "XDG_CONFIG_HOME",
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_TARGET_DIR",
            "CARGO_STEWARD_INNER",
            "CARGO_STEWARD_SLOT",
            "CARGO_STEWARD_SLOT_FD",
            "CARGO_PRIMARY_PACKAGE",
            "CARGO_MANIFEST_DIR",
            "RUSTC_WRAPPER",
            "GITHUB_STEP_SUMMARY",
        ] {
            cmd.env_remove(var);
        }
        cmd
    }

    fn ok(&self, cmd: &mut Command) -> String {
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{cmd:?}: {}", text(&out));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn fails(&self, cmd: &mut Command) -> String {
        let out = cmd.output().unwrap();
        assert!(!out.status.success(), "{cmd:?} succeeded: {}", text(&out));
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    fn config(&self, toml: &str) {
        fs::write(self.home.join("config.toml"), toml).unwrap();
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn path_with(first: &Path) -> OsString {
    let mut path = OsString::from(first);
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    path
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
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

// ---- shim install / uninstall ----

#[test]
fn install_refuses_to_replace_what_isnt_ours() {
    let env = Env::new();
    let bin = env.root.join("bin");
    let bin_arg = bin.to_str().unwrap();
    let install = || {
        let mut cmd = env.cmd(&env.root, &["shim", "install", "--bin-dir", bin_arg]);
        cmd.env("PATH", std::env::var_os("PATH").unwrap_or_default());
        cmd
    };
    let out = env.ok(&mut install());
    assert!(out.contains("Created"), "{out}");
    // Not first on PATH: says how to put it there.
    assert!(out.contains("first on PATH: add this"), "{out}");
    assert!(
        out.contains(&format!("export PATH=\"{bin_arg}:$PATH\"")),
        "{out}"
    );
    let out = env.ok(&mut install());
    assert!(out.contains("already points at"), "{out}");

    // Another cargo-steward install's link is repointed at this one.
    let link = bin.join("cargo");
    fs::remove_file(&link).unwrap();
    symlink("/old/place/cargo-steward", &link).unwrap();
    let out = env.ok(&mut install());
    assert!(out.contains("Repointed"), "{out}");
    assert_eq!(
        fs::canonicalize(&link).unwrap(),
        fs::canonicalize(BIN).unwrap()
    );

    // Someone else's `cargo` link or file is left alone.
    fs::remove_file(&link).unwrap();
    symlink("/usr/bin/true", &link).unwrap();
    let err = env.fails(&mut install());
    assert!(
        err.contains("points at /usr/bin/true; remove it first"),
        "{err}"
    );
    assert_eq!(fs::read_link(&link).unwrap(), Path::new("/usr/bin/true"));
    fs::remove_file(&link).unwrap();
    fs::write(&link, "mine").unwrap();
    let err = env.fails(&mut install());
    assert!(err.contains("isn't a symlink"), "{err}");
    assert_eq!(fs::read_to_string(&link).unwrap(), "mine");

    // Uninstall leaves it alone too, and says so.
    let out = env.ok(&mut env.cmd(&env.root, &["shim", "uninstall", "--bin-dir", bin_arg]));
    assert!(out.contains("Left") && out.contains("alone"), "{out}");
    assert!(link.exists());
    fs::remove_file(&link).unwrap();
    let out = env.ok(&mut env.cmd(&env.root, &["shim", "uninstall", "--bin-dir", bin_arg]));
    assert!(out.contains("No proxy at"), "{out}");
}

#[test]
fn install_when_already_first_on_path_needs_no_path_hint() {
    let env = Env::new();
    let bin = env.home.join("bin");
    let mut cmd = env.cmd(&env.root, &["shim", "install"]);
    cmd.env("PATH", path_with(&bin));
    let out = env.ok(&mut cmd);
    assert!(out.contains("Created"), "{out}");
    assert!(!out.contains("first on PATH"), "{out}");
    assert!(out.contains("Routing is set by target-root"), "{out}");
    assert!(bin.join("cargo").is_symlink());
}

#[test]
fn install_appends_to_and_uninstall_strips_each_shells_startup_file() {
    let env = Env::new();
    let bin = env.root.join("bin");
    let bin_arg = bin.to_str().unwrap();
    let xdg = env.root.join("xdg");
    let zdot = env.root.join("zdot");
    let run = |args: &[&str]| {
        let mut cmd = env.cmd(&env.root, args);
        cmd.env("XDG_CONFIG_HOME", &xdg).env("ZDOTDIR", &zdot);
        env.ok(&mut cmd)
    };
    let bashrc = env.user.join(".bashrc");
    fs::write(&bashrc, "alias ll='ls -l'\n").unwrap();
    for shell in ["bash", "fish", "zsh"] {
        let out = run(&["shim", "install", "--bin-dir", bin_arg, "--shell", shell]);
        assert!(out.contains("Open a new shell"), "{out}");
    }
    // Rerunning rewrites the block in place rather than adding another.
    run(&["shim", "install", "--bin-dir", bin_arg, "--shell", "bash"]);

    let bash = fs::read_to_string(&bashrc).unwrap();
    assert!(bash.starts_with("alias ll='ls -l'\n"), "{bash}");
    assert_eq!(bash.matches("# >>> cargo-steward shim >>>").count(), 1);
    assert!(bash.contains(&format!("export PATH=\"{bin_arg}:$PATH\"")));
    let fish = xdg.join("fish/config.fish");
    let fish_text = fs::read_to_string(&fish).unwrap();
    assert!(fish_text.contains(&format!("set -gx PATH '{bin_arg}' $PATH")));
    let zshenv = zdot.join(".zshenv");
    assert!(zshenv.is_file());
    assert!(!env.user.join(".zshenv").exists(), "ZDOTDIR wins over HOME");

    let mut status = env.cmd(&env.root, &["shim", "status"]);
    status
        .env("XDG_CONFIG_HOME", &xdg)
        .env("ZDOTDIR", &zdot)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default());
    let out = env.ok(&mut status);
    assert!(out.contains("PATH set in:"), "{out}");
    assert!(
        out.contains(".bashrc") && out.contains("config.fish"),
        "{out}"
    );

    let out = run(&["shim", "uninstall", "--bin-dir", bin_arg]);
    assert!(out.contains(&format!("Removed {}", bin.join("cargo").display())));
    assert_eq!(out.matches("Removed the PATH line").count(), 3, "{out}");
    assert_eq!(fs::read_to_string(&bashrc).unwrap(), "alias ll='ls -l'\n");
    assert!(!fs::read_to_string(&fish).unwrap().contains("cargo-steward"));
    assert!(
        !fs::read_to_string(&zshenv)
            .unwrap()
            .contains("cargo-steward")
    );
}

#[test]
fn install_needs_a_home() {
    let env = Env::new();
    let bin = env.root.join("bin");
    let mut cmd = env.cmd(
        &env.root,
        &[
            "shim",
            "install",
            "--bin-dir",
            bin.to_str().unwrap(),
            "--shell",
            "bash",
        ],
    );
    cmd.env_remove("HOME");
    let err = env.fails(&mut cmd);
    assert!(err.contains("HOME isn't set"), "{err}");

    // No --bin-dir and nowhere to find the steward home.
    let mut cmd = env.cmd(&env.root, &["shim", "install"]);
    cmd.env_remove("HOME")
        .env_remove("CARGO_STEWARD_HOME")
        .env_remove("CARGO_HOME");
    let err = env.fails(&mut cmd);
    assert!(err.contains("can't find the steward home"), "{err}");
}

// ---- shim status / explain ----

#[test]
fn status_reports_each_install_state() {
    let env = Env::new();
    let bin = env.home.join("bin");
    let empty = env.root.join("empty-path");
    fs::create_dir_all(&empty).unwrap();

    // Not installed, and no cargo on PATH at all.
    let mut cmd = env.cmd(&env.root, &["shim", "status"]);
    cmd.env("PATH", &empty);
    let out = env.ok(&mut cmd);
    assert!(out.contains("proxy:        not installed in"), "{out}");
    assert!(out.contains("first cargo:  none on PATH"), "{out}");
    assert!(
        out.contains("real cargo:   none found: the proxy can't run cargo"),
        "{out}"
    );

    // Installed and first on PATH.
    fs::create_dir_all(&bin).unwrap();
    symlink(BIN, bin.join("cargo")).unwrap();
    let mut cmd = env.cmd(&env.root, &["shim", "status"]);
    cmd.env("PATH", path_with(&bin));
    let out = env.ok(&mut cmd);
    assert!(
        out.contains("proxy:        ") && out.contains(" -> "),
        "{out}"
    );
    assert!(out.contains("(the proxy)"), "{out}");

    // Another cargo first on PATH.
    let other = env.root.join("other");
    fs::create_dir_all(&other).unwrap();
    script(&other.join("cargo"), "exit 0");
    let mut cmd = env.cmd(&env.root, &["shim", "status"]);
    cmd.env("PATH", &other);
    let out = env.ok(&mut cmd);
    assert!(out.contains("(not the proxy: put"), "{out}");

    // A link to another cargo-steward install.
    fs::remove_file(bin.join("cargo")).unwrap();
    symlink("/gone/cargo-steward", bin.join("cargo")).unwrap();
    let out = env.ok(&mut env.cmd(&env.root, &["shim", "status"]));
    assert!(
        out.contains("points at another cargo-steward (/gone/cargo-steward)"),
        "{out}"
    );
}

#[test]
fn explain_describes_settings_and_build_slots() {
    let env = Env::new();
    let work = env.root.join("work");
    fs::create_dir_all(&work).unwrap();
    let explain = |args: &[&str]| {
        let mut full = vec!["shim", "explain"];
        full.extend_from_slice(args);
        env.ok(&mut env.cmd(&work, &full))
    };

    let out = explain(&[]);
    assert!(out.contains("(missing: defaults, nothing routed)"), "{out}");
    assert!(out.contains("command:      cargo build"), "{out}");
    assert!(out.contains("no limit (max-builds isn't set)"), "{out}");

    env.config("max-builds = \"lots\"\n");
    let out = explain(&[]);
    assert!(
        out.contains("settings:     unusable, the proxy uses the defaults"),
        "{out}"
    );

    let targets = env.root.join("targets");
    env.config(&format!(
        "max-builds = 2\nseed = false\n\n[[routes]]\npath = {:?}\ntarget-root = {:?}\nforce = true\nmax-builds = 1\n",
        work.display().to_string(),
        targets.display().to_string(),
    ));
    let out = explain(&["build"]);
    assert!(out.contains(&format!(
        "settings:     {}",
        env.home.join("config.toml").display()
    )));
    assert!(
        out.contains("checkout:     not in git (standalone)"),
        "{out}"
    );
    assert!(
        out.contains(&format!("route:        {}", work.display())),
        "{out}"
    );
    assert!(out.contains("build slot:   takes one of 1 (in "), "{out}");
    assert!(
        out.contains(&format!("target dir:   {}", targets.display())),
        "{out}"
    );
    assert!(
        out.contains("tmp dir:      not routed: tmp-root isn't set"),
        "{out}"
    );
    assert!(out.contains("seed:         off"), "{out}");
    let out = explain(&["metadata"]);
    assert!(
        out.contains("build slot:   none needed: `metadata` doesn't build"),
        "{out}"
    );

    // A relative root can't be routed.
    env.config("target-root = \"relative\"\n");
    let out = explain(&[]);
    assert!(out.contains("routing:      not routed:"), "{out}");
}

#[test]
fn explain_describes_worktrees_and_seeding() {
    let env = Env::new();
    let repo = env.root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    fs::write(repo.join("f"), "x").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    let wt = env.root.join("wt");
    git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    let targets = env.root.join("targets");
    env.config(&format!(
        "seed = true\n\n[[routes]]\npath = {:?}\ntarget-root = {:?}\nforce = true\n",
        env.root.display().to_string(),
        targets.display().to_string(),
    ));
    let explain = |dir: &Path, args: &[&str]| {
        let mut full = vec!["shim", "explain"];
        full.extend_from_slice(args);
        env.ok(&mut env.cmd(dir, &full))
    };

    let out = explain(&repo, &[]);
    assert!(
        out.contains(&format!("checkout:     {} (main)", repo.display())),
        "{out}"
    );
    assert!(
        out.contains("seed:         no: only builds in a linked worktree are seeded"),
        "{out}"
    );
    let out = explain(&repo.join(".git"), &[]);
    assert!(out.contains("inside the git directory"), "{out}");

    let out = explain(&wt, &[]);
    assert!(out.contains("(linked worktree; git dir "), "{out}");
    assert!(out.contains("doesn't exist yet"), "{out}");
    let seed_line = out.lines().find(|l| l.starts_with("seed:")).unwrap();
    let from = seed_line
        .trim_start_matches("seed:         no: ")
        .trim_end_matches(" doesn't exist yet");
    fs::create_dir_all(from).unwrap();
    let out = explain(&wt, &[]);
    assert!(
        out.contains(&format!("seed:         yes, from {from}")),
        "{out}"
    );
    let target_line = out.lines().find(|l| l.starts_with("target dir:")).unwrap();
    let target = target_line.trim_start_matches("target dir:   ");
    fs::create_dir_all(target).unwrap();
    fs::write(Path::new(target).join("x"), "").unwrap();
    let out = explain(&wt, &[]);
    assert!(
        out.contains("no: the target dir already has files"),
        "{out}"
    );

    // A target dir on the command line wins over routing.
    let out = explain(&wt, &["build", "--target-dir", "/x"]);
    assert!(out.contains("target dir:   not routed: "), "{out}");
    assert!(out.contains("seed:         no: nothing routed"), "{out}");
}

// ---- ci ----

#[test]
fn ci_reports_metadata_failures_and_bad_budgets() {
    let env = Env::new();
    let plain = env.root.join("not-cargo");
    fs::create_dir_all(&plain).unwrap();
    let cargo = env!("CARGO");

    let mut cmd = env.cmd(&plain, &["ci", "start"]);
    cmd.env("CARGO", cargo);
    let err = env.fails(&mut cmd);
    assert!(err.contains("`cargo metadata` failed"), "{err}");

    let mut cmd = env.cmd(&plain, &["ci", "start"]);
    cmd.env("CARGO", env.root.join("no-such-cargo"));
    let err = env.fails(&mut cmd);
    assert!(err.contains("metadata"), "{err}");

    let mut cmd = env.cmd(&plain, &["ci", "trim", "--budget", "lots"]);
    cmd.env("CARGO", cargo);
    let err = env.fails(&mut cmd);
    assert!(err.contains("--budget \"lots\""), "{err}");

    // An explicit build dir works without metadata, with a warning that
    // workspace crates can't be told apart.
    let target = env.root.join("target");
    let out = env.ok(env
        .cmd(
            &plain,
            &["ci", "start", "--target-dir", target.to_str().unwrap()],
        )
        .env("CARGO", cargo));
    assert!(out.contains("marked the job's start"), "{out}");
    assert!(
        out.contains("fingerprint files will show") || out.contains("noatime"),
        "{out}"
    );
    let out = env
        .cmd(
            &plain,
            &["ci", "trim", "--target-dir", target.to_str().unwrap()],
        )
        .env("CARGO", cargo)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("can't be told apart"),
        "{}",
        text(&out)
    );
}

#[test]
fn ci_start_finds_the_build_dir_with_cargo_metadata() {
    let env = Env::new();
    let project = env.root.join("proj");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"proj\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(project.join("src/lib.rs"), "").unwrap();
    let out = env.ok(env
        .cmd(&project, &["ci", "start"])
        .env("CARGO", env!("CARGO")));
    assert!(
        out.contains(&format!("start in {}", project.join("target").display())),
        "{out}"
    );
    assert!(project.join("target").is_dir());
    let out = env.ok(env
        .cmd(&project, &["ci", "trim", "--dry-run"])
        .env("CARGO", env!("CARGO")));
    assert!(out.contains("cargo-steward ci trim:"), "{out}");
}

// ---- the rustc wrapper ----

/// A fake rustc in `dir` running `body`.
fn fake_rustc(dir: &Path, body: &str) -> PathBuf {
    let rustc = dir.join("rustc");
    script(&rustc, body);
    rustc
}

#[test]
fn the_wrapper_passes_rustcs_exit_status_through() {
    let env = Env::new();
    let out_dir = env.root.join("out");
    fs::create_dir_all(&out_dir).unwrap();
    let out_arg = out_dir.to_str().unwrap();

    let failing = fake_rustc(&env.root, "exit 3");
    let out = env
        .cmd(
            &env.root,
            &[failing.to_str().unwrap(), "--out-dir", out_arg],
        )
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));

    // Killed by a signal: the shell's 128 + signal.
    let killed = env.root.join("killed");
    fs::create_dir_all(&killed).unwrap();
    let killed = fake_rustc(&killed, "kill -9 $$");
    let out = env
        .cmd(&env.root, &[killed.to_str().unwrap(), "--out-dir", out_arg])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(137), "{}", text(&out));

    // A rustc that can't run at all.
    let missing = env.root.join("nowhere/rustc");
    let out = env
        .cmd(
            &env.root,
            &[missing.to_str().unwrap(), "--out-dir", out_arg],
        )
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&out.stderr).contains("failed to run"));

    // Nothing compiled: rustc is run as is (exec'd), output untouched.
    let probe = env.root.join("probe");
    fs::create_dir_all(&probe).unwrap();
    let probe = fake_rustc(&probe, "echo \"rustc $*\"");
    let out = env
        .cmd(&env.root, &[probe.to_str().unwrap(), "-vV"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "rustc -vV\n");
    let out = env
        .cmd(&env.root, &[missing.to_str().unwrap(), "-vV"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127));
}

#[test]
fn the_wrapper_runs_rustc_through_the_inner_wrapper() {
    let env = Env::new();
    let out_dir = env.root.join("out");
    fs::create_dir_all(&out_dir).unwrap();
    let log = env.root.join("inner.log");
    let tools = env.root.join("tools");
    fs::create_dir_all(&tools).unwrap();
    script(
        &tools.join("inner"),
        &format!("echo \"$@\" >> '{}'\nexec \"$@\"", log.display()),
    );
    let rustc = fake_rustc(&env.root, "exit 0");
    let args = [
        rustc.to_str().unwrap(),
        "--out-dir",
        out_dir.to_str().unwrap(),
    ];

    // From the settings, found on PATH.
    env.config("inner-wrapper = \"inner\"\n");
    env.ok(env.cmd(&env.root, &args).env("PATH", path_with(&tools)));
    let logged = fs::read_to_string(&log).unwrap();
    assert_eq!(
        logged,
        format!("{} --out-dir {}\n", rustc.display(), out_dir.display())
    );

    // `CARGO_STEWARD_INNER` overrides it; empty means none.
    env.ok(env
        .cmd(&env.root, &args)
        .env("PATH", path_with(&tools))
        .env("CARGO_STEWARD_INNER", ""));
    assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 1);
    env.ok(env
        .cmd(&env.root, &args)
        .env("CARGO_STEWARD_INNER", tools.join("inner")));
    assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 2);
    // A missing inner wrapper never breaks the build.
    env.ok(env
        .cmd(&env.root, &args)
        .env("CARGO_STEWARD_INNER", "no-such-sccache"));
    assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 2);
}

#[test]
fn the_wrapper_records_workspace_compiles_in_the_ledger() {
    let env = Env::new();
    env.config("auto-gc = false\n");
    let target = env.root.join("target");
    let deps = target.join("debug/deps");
    fs::create_dir_all(&deps).unwrap();
    fs::write(target.join(".rustc_info.json"), "{}").unwrap();
    let rustc = fake_rustc(&env.root, "exit 0");
    let args = [
        rustc.to_str().unwrap(),
        "--crate-name",
        "app",
        "--emit=link",
        "-C",
        "extra-filename=-0123456789abcdef",
        "--out-dir",
        deps.to_str().unwrap(),
    ];
    let ledger = env.home.join("ledger.jsonl");

    // A dependency's compile isn't recorded.
    env.ok(&mut env.cmd(&env.root, &args));
    assert!(!ledger.exists());

    env.ok(env
        .cmd(&env.root, &args)
        .env("CARGO_PRIMARY_PACKAGE", "1")
        .env("CARGO_MANIFEST_DIR", &env.root));
    let text = fs::read_to_string(&ledger).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.contains(&target.display().to_string()), "{text}");

    // Outside any build directory: nothing to record.
    let loose = env.root.join("loose");
    fs::create_dir_all(&loose).unwrap();
    let mut loose_args = args;
    loose_args[7] = loose.to_str().unwrap();
    env.ok(env
        .cmd(&env.root, &loose_args)
        .env("CARGO_PRIMARY_PACKAGE", "1"));
    assert_eq!(fs::read_to_string(&ledger).unwrap().lines().count(), 1);
}
