//! End to end through the binary: `shim install|uninstall|status|explain`,
//! `ci start|trim` argument handling, and the rustc wrapper's exit codes,
//! inner wrapper and ledger. Every test runs with its own home directories;
//! nothing touches the real shell profiles.

mod common;

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

    /// `shim explain --json args...` in `dir`, parsed: stdout must be
    /// exactly one JSON document.
    fn explain_json(&self, dir: &Path, args: &[&str]) -> serde_json::Value {
        let mut full = vec!["shim", "explain", "--json"];
        full.extend_from_slice(args);
        let out = self.ok(&mut self.cmd(dir, &full));
        let doc: serde_json::Value = serde_json::from_str(&out)
            .unwrap_or_else(|e| panic!("stdout isn't one JSON document ({e}):\n{out}"));
        assert_eq!(doc["version"], 1, "{doc}");
        doc
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
    let status = common::isolate_from_git(&mut Command::new("git"))
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

#[test]
fn install_reports_a_bin_dir_or_startup_file_it_cant_read() {
    let env = Env::new();
    // A bin dir it can't look into.
    let locked = env.root.join("locked");
    fs::create_dir_all(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let mut cmd = env.cmd(
        &env.root,
        &["shim", "install", "--bin-dir", locked.to_str().unwrap()],
    );
    let err = env.fails(&mut cmd);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        err.contains(&locked.join("cargo").display().to_string()),
        "{err}"
    );
    assert!(fs::read_dir(&locked).unwrap().next().is_none());

    // A startup file that is a directory.
    let bin = env.root.join("bin");
    fs::create_dir_all(env.user.join(".bashrc")).unwrap();
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
    let err = env.fails(&mut cmd);
    assert!(err.contains(".bashrc"), "{err}");
    assert!(env.user.join(".bashrc").is_dir());
}

#[test]
fn uninstall_leaves_startup_files_without_the_block_alone() {
    let env = Env::new();
    let bin = env.root.join("bin");
    let bashrc = env.user.join(".bashrc");
    fs::write(&bashrc, "alias ll='ls -l'\n").unwrap();
    let out = env.ok(&mut env.cmd(
        &env.root,
        &["shim", "uninstall", "--bin-dir", bin.to_str().unwrap()],
    ));
    assert!(!out.contains("Removed the PATH line"), "{out}");
    assert_eq!(fs::read_to_string(&bashrc).unwrap(), "alias ll='ls -l'\n");
}

#[test]
fn explain_without_any_home_uses_the_defaults() {
    let env = Env::new();
    let mut cmd = env.cmd(&env.root, &["shim", "explain"]);
    cmd.env_remove("HOME")
        .env_remove("CARGO_STEWARD_HOME")
        .env_remove("CARGO_HOME");
    let out = env.ok(&mut cmd);
    assert!(
        out.contains("settings:     none (no HOME): defaults"),
        "{out}"
    );
}

// ---- the proxy, up to running cargo ----

/// The proxy as `cargo` in `bin`, with a "real cargo" that can't be run: a
/// script whose interpreter doesn't exist. The proxy does all its work, then
/// fails to exec, so coverage of that work is recorded.
fn broken_cargo(env: &Env) -> (PathBuf, PathBuf) {
    let bin = env.root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    symlink(BIN, bin.join("cargo")).unwrap();
    let bogus = env.root.join("real/cargo");
    fs::create_dir_all(bogus.parent().unwrap()).unwrap();
    fs::write(&bogus, "#!/nonexistent/interpreter\n").unwrap();
    fs::set_permissions(&bogus, fs::Permissions::from_mode(0o755)).unwrap();
    (bin, bogus)
}

#[test]
fn the_proxy_reports_a_real_cargo_it_cant_run() {
    let env = Env::new();
    let (bin, bogus) = broken_cargo(&env);
    let work = env.root.join("work");
    fs::create_dir_all(&work).unwrap();
    env.config(&format!(
        "real-cargo = {:?}\ntarget-root = \"~/targets\"\nseed = false\n",
        bogus.display().to_string()
    ));
    // `~/` needs HOME: the build isn't routed, but cargo is still run.
    let mut cmd = Command::new(bin.join("cargo"));
    cmd.arg("build")
        .current_dir(&work)
        .env("CARGO_STEWARD_HOME", &env.home)
        .env_remove("HOME")
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR");
    let out = cmd.output().unwrap();
    assert_eq!(out.status.code(), Some(127), "{}", text(&out));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("not routing this build") && err.contains("needs HOME"),
        "{err}"
    );
    assert!(
        err.contains(&format!("failed to run {}", bogus.display())),
        "{err}"
    );
}

#[test]
fn the_proxy_reuses_a_slot_its_parent_holds() {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let env = Env::new();
    let (bin, bogus) = broken_cargo(&env);
    env.config(&format!(
        "real-cargo = {:?}\nmax-builds = 1\nseed = false\n",
        bogus.display().to_string()
    ));
    let slots = env.home.join("slots");
    fs::create_dir_all(&slots).unwrap();
    let slot = slots.join("0.lock");
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&slot)
        .unwrap();
    held.lock().unwrap();
    let fd = held.as_raw_fd();
    let mut cmd = Command::new(bin.join("cargo"));
    cmd.arg("build")
        .current_dir(&env.root)
        .env("CARGO_STEWARD_HOME", &env.home)
        .env("HOME", &env.user)
        .env_remove("CARGO_TARGET_DIR")
        .env("CARGO_STEWARD_SLOT_FD", fd.to_string())
        .env("CARGO_STEWARD_SLOT", &slot);
    // SAFETY: fcntl is async-signal-safe; this clears close-on-exec on the
    // slot descriptor in the child only, as an outer proxy leaves it.
    unsafe {
        cmd.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = cmd.output().unwrap();
    // It didn't wait for the slot this process holds, and got as far as
    // running cargo.
    assert_eq!(out.status.code(), Some(127), "{}", text(&out));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("waiting for one of"), "{err}");
    assert!(err.contains("failed to run"), "{err}");
    drop(held);
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
    // The bad setting alone falls back to its default.
    assert!(
        out.contains("max-builds: invalid type: string \"lots\"")
            && out.contains("using the default"),
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
    let seed_line = out.lines().find(|l| l.starts_with("seed:")).unwrap();
    let namespace = seed_line
        .strip_prefix("seed:         no: no other worktree has built in ")
        .and_then(|rest| rest.strip_suffix(" yet"))
        .unwrap_or_else(|| panic!("{out}"));
    let target_line = out.lines().find(|l| l.starts_with("target dir:")).unwrap();
    let target = target_line.trim_start_matches("target dir:   ");
    assert_eq!(Path::new(target).parent(), Some(Path::new(namespace)));

    // A build directory in the namespace, as cargo leaves one.
    let built = |key: &str| {
        let dir = Path::new(namespace).join(key);
        fs::create_dir_all(dir.join("debug/.fingerprint")).unwrap();
        fs::write(dir.join(".rustc_info.json"), "{}").unwrap();
        dir
    };
    let main = built("main");
    let out = explain(&wt, &[]);
    assert!(
        out.contains(&format!(
            "seed:         yes, from {} (the main checkout)",
            main.display()
        )),
        "{out}"
    );
    // A build is using the main checkout's: another worktree's is used.
    let other = built("other");
    let lock = fs::File::create(main.join("debug/.cargo-lock")).unwrap();
    lock.lock().unwrap();
    let out = explain(&wt, &[]);
    assert!(
        out.contains(&format!(
            "seed:         yes, from {} (worktree other, last used 0m ago; busy: the main checkout)",
            other.display()
        )),
        "{out}"
    );
    let lock_other = fs::File::create(other.join("debug/.cargo-lock")).unwrap();
    lock_other.lock().unwrap();
    let out = explain(&wt, &[]);
    assert!(
        out.contains("seed:         not now: builds are using every build to seed from"),
        "{out}"
    );
    drop((lock, lock_other));
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

#[test]
fn explain_json_describes_settings_routing_and_build_slots() {
    let env = Env::new();
    let work = env.root.join("work");
    fs::create_dir_all(&work).unwrap();

    let doc = env.explain_json(&work, &[]);
    assert_eq!(doc["settings"]["exists"], false, "{doc}");
    assert_eq!(
        doc["settings"]["path"],
        env.home.join("config.toml").display().to_string()
    );
    assert_eq!(doc["command"], serde_json::json!(["build"]), "{doc}");
    assert_eq!(doc["build_slot"]["status"], "no_limit", "{doc}");
    assert_eq!(doc["target_dir"]["routed"], false, "{doc}");

    // A setting it can't use is reported in the document, not on stdout.
    env.config("max-builds = \"lots\"\n");
    let doc = env.explain_json(&work, &[]);
    let problems = doc["settings"]["problems"].as_array().unwrap();
    assert!(
        problems[0]
            .as_str()
            .unwrap()
            .contains("max-builds: invalid type"),
        "{doc}"
    );

    let targets = env.root.join("targets");
    env.config(&format!(
        "max-builds = 2\nseed = false\n\n[[routes]]\npath = {:?}\ntarget-root = {:?}\nforce = true\nmax-builds = 1\n",
        work.display().to_string(),
        targets.display().to_string(),
    ));
    let doc = env.explain_json(&work, &["build"]);
    assert_eq!(doc["routing_error"], serde_json::Value::Null, "{doc}");
    assert_eq!(doc["directory"], work.display().to_string(), "{doc}");
    assert_eq!(doc["checkout"]["in_git"], false, "{doc}");
    assert_eq!(doc["namespace"]["key"], "standalone", "{doc}");
    assert_eq!(doc["route"], work.display().to_string(), "{doc}");
    assert_eq!(doc["target_dir"]["routed"], true, "{doc}");
    let target = doc["target_dir"]["path"].as_str().unwrap();
    assert!(target.starts_with(targets.to_str().unwrap()), "{doc}");
    assert!(
        target.ends_with(doc["namespace"]["path"].as_str().unwrap()),
        "{doc}"
    );
    // Not created yet: the space is that of the disk it will be on.
    assert!(
        doc["target_dir"]["free_bytes"].as_u64().unwrap() > 0,
        "{doc}"
    );
    assert_eq!(doc["tmp_dir"]["routed"], false, "{doc}");
    assert_eq!(doc["tmp_dir"]["reason"], "tmp-root isn't set", "{doc}");
    assert_eq!(doc["tmp_dir"]["free_bytes"], serde_json::Value::Null);
    assert_eq!(doc["seed"]["status"], "off", "{doc}");
    assert_eq!(doc["build_slot"]["status"], "takes_slot", "{doc}");
    assert_eq!(doc["build_slot"]["limit"], 1, "{doc}");
    assert!(doc["build_slot"]["dir"].is_string(), "{doc}");
    let doc = env.explain_json(&work, &["metadata"]);
    assert_eq!(doc["command"], serde_json::json!(["metadata"]), "{doc}");
    assert_eq!(doc["build_slot"]["status"], "not_a_build", "{doc}");

    // A relative root can't be routed: no plan, and why.
    env.config("target-root = \"relative\"\n");
    let doc = env.explain_json(&work, &[]);
    assert!(doc["routing_error"].is_string(), "{doc}");
    assert_eq!(doc["target_dir"], serde_json::Value::Null, "{doc}");
    assert_eq!(doc["seed"], serde_json::Value::Null, "{doc}");
}

#[test]
fn explain_json_says_whether_a_worktree_seeds() {
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

    let doc = env.explain_json(&repo, &[]);
    assert_eq!(doc["checkout"]["linked"], false, "{doc}");
    assert_eq!(doc["checkout"]["worktree"], repo.display().to_string());
    assert_eq!(doc["namespace"]["key"], "main", "{doc}");
    assert_eq!(doc["seed"]["status"], "not_linked_worktree", "{doc}");

    let doc = env.explain_json(&wt, &[]);
    assert_eq!(doc["checkout"]["linked"], true, "{doc}");
    assert_eq!(doc["seed"]["status"], "source_missing", "{doc}");
    assert_eq!(doc["seed"]["will_seed"], false, "{doc}");
    assert_eq!(doc["seed"]["from"], serde_json::Value::Null, "{doc}");
    // Once the main checkout has built, a new worktree starts from it.
    let repo_ns = doc["namespace"]["repo"].as_str().unwrap().to_owned();
    let main = targets.join(&repo_ns).join("main");
    fs::create_dir_all(main.join("debug/.fingerprint")).unwrap();
    let from = main.display().to_string();
    let doc = env.explain_json(&wt, &[]);
    assert_eq!(doc["seed"]["status"], "will_seed", "{doc}");
    assert_eq!(doc["seed"]["will_seed"], true, "{doc}");
    assert_eq!(doc["seed"]["from"], from, "{doc}");
    let message = doc["seed"]["message"].as_str().unwrap();
    assert!(message.starts_with(&format!("yes, from {from} (")), "{doc}");

    // A target dir on the command line wins over routing.
    let doc = env.explain_json(&wt, &["build", "--target-dir", "/x"]);
    assert_eq!(doc["target_dir"]["routed"], false, "{doc}");
    assert!(doc["target_dir"]["reason"].is_string(), "{doc}");
    assert_eq!(doc["seed"]["status"], "nothing_routed", "{doc}");
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
    fs::create_dir_all(&target).unwrap();
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
    // Cargo creates (and tags) the build directory, not `ci start`.
    assert!(
        out.contains(&format!(
            "{} doesn't exist yet",
            project.join("target").display()
        )),
        "{out}"
    );
    assert!(!project.join("target").exists());
    let out = env.ok(env
        .cmd(&project, &["ci", "trim", "--dry-run"])
        .env("CARGO", env!("CARGO")));
    assert!(
        out.contains(&format!(
            "nothing to trim, {} doesn't exist",
            project.join("target").display()
        )),
        "{out}"
    );
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

#[test]
fn test_commands_find_their_repository_from_their_directory_alone() {
    let mut command = Command::new("git");
    common::isolate_from_git(&mut command);
    let removed: Vec<_> = command
        .get_envs()
        .filter(|(_, v)| v.is_none())
        .map(|(k, _)| k)
        .collect();
    for var in common::GIT_REPO_VARS {
        assert!(removed.contains(&std::ffi::OsStr::new(var)), "{var}");
    }
}

#[test]
fn a_build_dir_goes_as_soon_as_git_removes_its_worktree() {
    let env = Env::new();
    env.config("auto-gc = false\ndedup = false\n");
    let repo = env.root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("README"), "x").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    let wt = env.root.join("wts/feature");
    git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    fs::create_dir_all(wt.join("crate")).unwrap();

    // Built outside the worktree, as the cargo proxy routes it.
    let target = env.root.join("build/feature");
    let deps = target.join("debug/deps");
    fs::create_dir_all(&deps).unwrap();
    fs::write(target.join(".rustc_info.json"), "{}").unwrap();
    let rustc = fake_rustc(&env.root, "exit 0");
    let compile = |manifest: &Path| {
        env.ok(env
            .cmd(
                &env.root,
                &[
                    rustc.to_str().unwrap(),
                    "--crate-name",
                    "app",
                    "--out-dir",
                    deps.to_str().unwrap(),
                ],
            )
            .env("CARGO_PRIMARY_PACKAGE", "1")
            .env("CARGO_MANIFEST_DIR", manifest));
    };
    compile(&wt.join("crate"));
    let ledger = fs::read_to_string(env.home.join("ledger.jsonl")).unwrap();
    assert!(
        ledger.contains(&format!(
            "\"worktree\":\"{}\"",
            wt.canonicalize().unwrap().display()
        )),
        "{ledger}"
    );
    assert!(ledger.contains("worktrees/feature"), "{ledger}");

    // While the worktree exists, nothing happens.
    let out = env.ok(&mut env.cmd(&env.root, &["gc", "--apply"]));
    assert!(target.exists(), "{out}");

    git(
        &repo,
        &["worktree", "remove", "--force", wt.to_str().unwrap()],
    );
    let out = env.ok(&mut env.cmd(&env.root, &["gc", "--apply"]));
    assert!(out.contains("worktrees removed from git"), "{out}");
    assert!(!target.exists(), "{out}");
}

#[test]
fn a_deleted_main_checkout_keeps_the_grace() {
    let env = Env::new();
    env.config("auto-gc = false\ndedup = false\n");
    let repo = env.root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    let target = env.root.join("build/main");
    let deps = target.join("debug/deps");
    fs::create_dir_all(&deps).unwrap();
    fs::write(target.join(".rustc_info.json"), "{}").unwrap();
    let rustc = fake_rustc(&env.root, "exit 0");
    env.ok(env
        .cmd(
            &env.root,
            &[
                rustc.to_str().unwrap(),
                "--crate-name",
                "app",
                "--out-dir",
                deps.to_str().unwrap(),
            ],
        )
        .env("CARGO_PRIMARY_PACKAGE", "1")
        .env("CARGO_MANIFEST_DIR", &repo));
    let ledger = fs::read_to_string(env.home.join("ledger.jsonl")).unwrap();
    assert!(!ledger.contains("linked"), "{ledger}");
    fs::remove_dir_all(&repo).unwrap();
    let out = env.ok(&mut env.cmd(&env.root, &["gc", "--apply"]));
    assert!(target.exists(), "{out}");
}

#[test]
fn explain_names_the_checkout_and_whether_the_proxy_is_in_use() {
    let env = Env::new();
    let repo = env.root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    // Nothing routed: the checkout is still found.
    let doc = env.explain_json(&repo, &[]);
    assert_eq!(doc["checkout"]["in_git"], true, "{doc}");
    assert_eq!(doc["namespace"]["key"], "main", "{doc}");

    env.config(&format!(
        "target-root = {:?}\n",
        env.root.join("targets").display().to_string()
    ));
    let bin = env.root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let explain = |json: bool| {
        let mut args = vec!["shim", "explain"];
        if json {
            args.push("--json");
        }
        let mut cmd = env.cmd(&repo, &args);
        cmd.env("PATH", &path);
        env.ok(&mut cmd)
    };
    // The real cargo first on PATH: what's planned doesn't happen.
    script(&bin.join("cargo"), "exit 0");
    let doc: serde_json::Value = serde_json::from_str(&explain(true)).unwrap();
    assert_eq!(doc["proxy"]["active"], false, "{doc}");
    assert_eq!(
        doc["proxy"]["first_cargo"],
        bin.join("cargo").display().to_string()
    );
    let out = explain(false);
    assert!(out.contains("proxy:        not in use here"), "{out}");

    // The proxy first.
    fs::remove_file(bin.join("cargo")).unwrap();
    symlink(BIN, bin.join("cargo")).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&explain(true)).unwrap();
    assert_eq!(doc["proxy"]["active"], true, "{doc}");
    assert!(!explain(false).contains("not in use here"));
}

#[test]
fn remove_worktree_names_a_relative_worktree_in_full() {
    let env = Env::new();
    let repo = env.root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    env.config(&format!(
        "target-root = {:?}\n",
        env.root.join("targets").display().to_string()
    ));
    let out = env.ok(&mut env.cmd(&repo, &["remove", "--worktree", ".", "--json"]));
    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        doc["worktrees"][0]["worktree"],
        repo.display().to_string(),
        "{doc}"
    );
}

#[test]
fn finds_build_dirs_under_the_proxys_target_roots() {
    let env = Env::new();
    let targets = env.root.join("s");
    env.config(&format!(
        "target-root = {:?}\n",
        targets.join("{label}/cargo").display().to_string()
    ));
    // A worktree's build dir, seeded but never compiled in, so nothing has
    // recorded it, and no scan root covers it.
    let dir = targets.join("app/cargo/app-0123456789/wt");
    fs::create_dir_all(dir.join("debug/.fingerprint/a-0123456789abcdef")).unwrap();
    fs::write(dir.join(".rustc_info.json"), "{}").unwrap();
    let out = env.ok(&mut env.cmd(&env.root, &["status", "--json"]));
    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    let found: Vec<&str> = doc["build_dirs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["path"].as_str().unwrap())
        .collect();
    assert_eq!(found, [dir.display().to_string()], "{doc}");
}
