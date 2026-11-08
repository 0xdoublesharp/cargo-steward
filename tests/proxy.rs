//! End to end: the `cargo` proxy in front of real cargo builds, in a git
//! repository and its worktrees.

mod common;

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::os::fd::AsRawFd;
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use sha2::{Digest, Sha256};

const BIN: &str = env!("CARGO_BIN_EXE_cargo-steward");
const WAITING: &str = "waiting for one of 1 Cargo build slots";

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    /// Holds the `cargo` symlink to the proxy.
    bin: PathBuf,
    /// `CARGO_STEWARD_HOME`.
    home: PathBuf,
    targets: PathBuf,
    tmps: PathBuf,
    repo: PathBuf,
}

fn git(dir: &Path, args: &[&str]) {
    let out = common::isolate_from_git(&mut Command::new("git"))
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
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A binary crate that prints the TMPDIR it runs with.
fn write_crate(dir: &Path, name: &str) {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    )
    .unwrap();
    fs::write(
        dir.join("src/main.rs"),
        "fn main() { println!(\"{}\", std::env::var(\"TMPDIR\").unwrap_or_default()); }\n",
    )
    .unwrap();
}

/// `<label>-<sha256 of the path>[..10]`, as the reference shim names it.
fn repo_name(label: &str, identity: &Path) -> String {
    let digest = Sha256::digest(identity.as_os_str().as_encoded_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{label}-{}", &hex[..10])
}

/// The cargo the proxy runs: the one running the tests, or
/// `STEWARD_TEST_CARGO` (with `RUSTC` set to its rustc) to check other
/// versions and layouts.
fn test_cargo() -> String {
    std::env::var("STEWARD_TEST_CARGO").unwrap_or_else(|_| env!("CARGO").to_owned())
}

impl Fixture {
    /// Seeding is off and the real cargo is the one running the tests,
    /// unless `extra_config` sets them.
    fn new(extra_config: &str) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        symlink(BIN, bin.join("cargo")).unwrap();
        let home = root.join("steward-home");
        fs::create_dir_all(&home).unwrap();
        let targets = root.join("targets");
        let tmps = root.join("tmps");
        let mut config = format!(
            "target-root = {:?}\ntmp-root = {:?}\n",
            targets.display().to_string(),
            tmps.display().to_string(),
        );
        if !extra_config.contains("real-cargo =") {
            config.push_str(&format!("real-cargo = {:?}\n", env!("CARGO")));
        }
        if !extra_config.contains("seed =") {
            config.push_str("seed = false\n");
        }
        config.push_str(extra_config);
        fs::write(home.join("config.toml"), config).unwrap();
        let repo = root.join("repo");
        write_crate(&repo, "pfx");
        fs::write(repo.join(".gitignore"), "/target\n").unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);
        Fixture {
            _tmp: tmp,
            root,
            bin,
            home,
            targets,
            tmps,
            repo,
        }
    }

    /// The repository's namespace directory name.
    fn repo_ns(&self) -> String {
        repo_name("repo", &self.repo.join(".git"))
    }

    /// `<bin>/cargo args...` in `dir`, isolated from this machine's setup.
    fn proxy(&self, dir: &Path, args: &[&str]) -> Command {
        let mut path = OsString::from(self.bin.as_os_str());
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        let mut cmd = Command::new(self.bin.join("cargo"));
        cmd.args(args)
            .current_dir(dir)
            .env("PATH", path)
            .env("CARGO_STEWARD_HOME", &self.home)
            .env("RUSTC_WRAPPER", "")
            .env("CARGO_TERM_COLOR", "never")
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_BUILD_TARGET_DIR")
            .env_remove("CARGO_BUILD_RUSTC_WRAPPER")
            .env_remove("CARGO_STEWARD_SLOT_FD")
            .env_remove("CARGO_STEWARD_SLOT");
        cmd
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        let out = self.proxy(dir, args).output().unwrap();
        assert!(
            out.status.success(),
            "cargo {args:?} in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    /// `cargo metadata` through the proxy, with `extra` arguments.
    fn metadata(&self, dir: &Path, extra: &[&str]) -> Command {
        let mut cmd = self.proxy(
            dir,
            &[
                "metadata",
                "--format-version",
                "1",
                "--no-deps",
                "--offline",
            ],
        );
        cmd.args(extra);
        cmd
    }

    fn steward(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .current_dir(dir)
            .env("CARGO_STEWARD_HOME", &self.home)
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_BUILD_TARGET_DIR")
            .output()
            .unwrap()
    }
}

/// The target directory cargo reports.
fn target_of(cmd: &mut Command) -> PathBuf {
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    PathBuf::from(json["target_directory"].as_str().unwrap())
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn routes_the_main_checkout_and_each_worktree() {
    let fx = Fixture::new("");
    let ns = fx.targets.join(fx.repo_ns());

    // Main checkout: build output and TMPDIR both land in `<repo>/main`.
    fx.run(&fx.repo, &["build", "-q"]);
    assert!(ns.join("main/debug/pfx").is_file());
    assert!(!fx.repo.join("target").exists());
    let printed = stdout(&fx.run(&fx.repo, &["run", "-q"]));
    let tmp = fx.tmps.join(fx.repo_ns()).join("main");
    assert_eq!(printed.trim_end(), format!("{}/", tmp.display()));
    assert!(tmp.is_dir());

    // A linked worktree: `<repo>/<worktree name>`, from any directory in it.
    git(&fx.repo, &["worktree", "add", "-q", "../wt-one"]);
    let wt = fx.root.join("wt-one");
    let explain = stdout(&fx.steward(&wt, &["shim", "explain"]));
    assert!(
        explain.contains(&format!("namespace:    {}/wt-one", fx.repo_ns())),
        "{explain}"
    );
    assert!(explain.contains("seed:         off"), "{explain}");
    fx.run(&wt.join("src"), &["build", "-q"]);
    assert!(ns.join("wt-one/debug/pfx").is_file());

    // `--manifest-path` picks the repository, wherever cargo runs.
    let manifest = fx.repo.join("Cargo.toml");
    assert_eq!(
        target_of(&mut fx.metadata(&fx.root, &["--manifest-path", manifest.to_str().unwrap()])),
        ns.join("main")
    );
    // Outside git: the directory's name and path, under `standalone`.
    let loose = fx.root.join("loose");
    write_crate(&loose, "loose");
    assert_eq!(
        target_of(&mut fx.metadata(&loose, &[])),
        fx.targets
            .join(repo_name("loose", &loose))
            .join("standalone")
    );
}

#[test]
fn leaves_a_chosen_target_dir_alone() {
    let fx = Fixture::new("");
    let mine = fx.root.join("mine");
    assert_eq!(
        target_of(fx.metadata(&fx.repo, &[]).env("CARGO_TARGET_DIR", &mine)),
        mine
    );
    assert_eq!(
        target_of(&mut fx.metadata(
            &fx.repo,
            &[
                "--config",
                &format!("build.target-dir={:?}", mine.display().to_string())
            ]
        )),
        mine
    );
    fs::create_dir_all(fx.repo.join(".cargo")).unwrap();
    fs::write(
        fx.repo.join(".cargo/config.toml"),
        format!("[build]\ntarget-dir = {:?}\n", mine.display().to_string()),
    )
    .unwrap();
    assert_eq!(target_of(&mut fx.metadata(&fx.repo, &[])), mine);
    assert!(!fx.targets.exists());
}

#[test]
fn finds_the_real_cargo_on_path_past_itself() {
    let fx = Fixture::new("");
    fs::write(fx.home.join("config.toml"), "").unwrap();
    let real_dir = Path::new(env!("CARGO")).parent().unwrap();
    let path = std::env::join_paths([fx.bin.as_path(), real_dir]).unwrap();
    let out = fx
        .proxy(&fx.repo, &["--version"])
        .env("PATH", &path)
        .env("CARGO_HOME", fx.root.join("no-cargo-home"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(stdout(&out).starts_with("cargo "));

    // With only the proxy to find, it says so instead of looping.
    let out = fx
        .proxy(&fx.repo, &["--version"])
        .env("PATH", &fx.bin)
        .env("CARGO_HOME", fx.root.join("no-cargo-home"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no real cargo found"));
}

#[test]
fn broken_settings_still_run_cargo() {
    let fx = Fixture::new("");
    fs::write(fx.home.join("config.toml"), "target-root = [oops\n").unwrap();
    // Without its settings it searches PATH: keep that to the test's cargo.
    let real_dir = Path::new(env!("CARGO")).parent().unwrap();
    let path = std::env::join_paths([fx.bin.as_path(), real_dir]).unwrap();
    let out = fx
        .proxy(&fx.repo, &["--version"])
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.contains("warning: cargo-steward"), "{stderr}");
}

fn hold_slot(fx: &Fixture) -> (File, PathBuf) {
    let slot = fx.home.join("slots/0.lock");
    fs::create_dir_all(slot.parent().unwrap()).unwrap();
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&slot)
        .unwrap();
    file.lock().unwrap();
    (file, slot)
}

#[test]
fn waits_for_a_build_slot() {
    let fx = Fixture::new("max-builds = 1\n");
    let (held, _) = hold_slot(&fx);

    let mut child = fx
        .proxy(&fx.repo, &["build", "-q"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let _ = tx.send(line.unwrap());
        }
    });
    let line = rx.recv_timeout(Duration::from_secs(30)).unwrap();
    assert!(line.contains(WAITING), "{line}");
    std::thread::sleep(Duration::from_millis(500));
    assert!(child.try_wait().unwrap().is_none(), "built without a slot");
    assert!(!fx.targets.join(fx.repo_ns()).join("main/debug").exists());

    // Commands that don't build don't wait.
    fx.run(&fx.repo, &["--version"]);
    fx.run(
        &fx.repo,
        &["metadata", "--format-version", "1", "--offline"],
    );

    drop(held);
    assert!(child.wait().unwrap().success());
    reader.join().unwrap();
    // The message is printed once, however long it waited.
    let rest: Vec<String> = rx.try_iter().collect();
    assert!(!rest.iter().any(|l| l.contains(WAITING)), "{rest:?}");
    assert!(
        fx.targets
            .join(fx.repo_ns())
            .join("main/debug/pfx")
            .is_file()
    );
}

#[test]
fn nested_cargo_reuses_the_inherited_slot() {
    let fx = Fixture::new("max-builds = 1\n");
    let (held, slot) = hold_slot(&fx);
    let fd = held.as_raw_fd();
    let mut cmd = fx.proxy(&fx.repo, &["build", "-q"]);
    cmd.env("CARGO_STEWARD_SLOT_FD", fd.to_string())
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
    assert!(out.status.success(), "{out:?}");
    assert!(!String::from_utf8_lossy(&out.stderr).contains(WAITING));

    // The variables alone, without the open descriptor, don't get past.
    let mut stale = fx
        .proxy(&fx.repo, &["build", "-q"])
        .env("CARGO_STEWARD_SLOT_FD", fd.to_string())
        .env("CARGO_STEWARD_SLOT", &slot)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(stale.stderr.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(line.contains(WAITING), "{line}");
    drop(held);
    assert!(stale.wait().unwrap().success());
}

#[test]
fn installs_and_uninstalls_the_shim() {
    let fx = Fixture::new("");
    let user_home = fx.root.join("user-home");
    fs::create_dir_all(&user_home).unwrap();
    fs::write(user_home.join(".zshenv"), "export EDITOR=vi\n").unwrap();
    let shim = |args: &[&str]| {
        let out = Command::new(BIN)
            .arg("shim")
            .args(args)
            .current_dir(&fx.repo)
            .env("CARGO_STEWARD_HOME", &fx.home)
            .env("HOME", &user_home)
            .env_remove("ZDOTDIR")
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .unwrap();
        assert!(out.status.success(), "shim {args:?}: {out:?}");
        stdout(&out)
    };
    let link = fx.home.join("bin/cargo");
    let me = Path::new(BIN).canonicalize().unwrap();

    shim(&["install", "--shell", "zsh"]);
    assert_eq!(fs::read_link(&link).unwrap(), me);
    shim(&["install", "--shell", "zsh"]);
    let rc = fs::read_to_string(user_home.join(".zshenv")).unwrap();
    assert!(rc.starts_with("export EDITOR=vi\n"), "{rc}");
    assert_eq!(
        rc.matches(&format!("{}:$PATH", fx.home.join("bin").display()))
            .count(),
        1,
        "{rc}"
    );

    let status = shim(&["status"]);
    assert!(
        status.contains(&format!("{} -> {}", link.display(), me.display())),
        "{status}"
    );
    assert!(status.contains("namespace:"), "{status}");

    shim(&["uninstall"]);
    assert!(fs::symlink_metadata(&link).is_err());
    assert_eq!(
        fs::read_to_string(user_home.join(".zshenv")).unwrap(),
        "export EDITOR=vi\n"
    );

    // Never replaces a `cargo` that isn't ours.
    let other = fx.root.join("other-bin");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("cargo"), "#!/bin/sh\n").unwrap();
    let out = Command::new(BIN)
        .args(["shim", "install", "--bin-dir"])
        .arg(&other)
        .env("CARGO_STEWARD_HOME", &fx.home)
        .env("HOME", &user_home)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(
        fs::read_to_string(other.join("cargo")).unwrap(),
        "#!/bin/sh\n"
    );
}

#[test]
fn removing_a_deleted_worktrees_build_dir_takes_its_tmpdir_too() {
    let fx = Fixture::new("orphan-grace = \"0s\"\n");
    let ns = fx.repo_ns();
    let wt = fx.root.join("wt");
    git(&fx.repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    for dir in [&fx.repo, &wt] {
        // Through the wrapper, so the build is recorded against the worktree.
        let out = fx
            .proxy(dir, &["build", "-q"])
            .env("RUSTC_WRAPPER", BIN)
            .env("CARGO_STEWARD_INNER", "")
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    }
    let (main_tmp, wt_tmp) = (fx.tmps.join(&ns).join("main"), fx.tmps.join(&ns).join("wt"));
    fs::write(wt_tmp.join("left-by-a-test"), "x").unwrap();
    assert!(main_tmp.is_dir());

    git(
        &fx.repo,
        &["worktree", "remove", "--force", wt.to_str().unwrap()],
    );
    let out = fx.steward(&fx.repo, &["gc", "--apply"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}{out:?}");
    assert!(!fx.targets.join(&ns).join("wt").exists(), "{text}");
    assert!(!wt_tmp.exists(), "{text}");
    assert!(text.contains("temporary dirs the proxy routed"), "{text}");
    assert!(main_tmp.is_dir(), "the main checkout's stays");
    assert!(fx.targets.join(&ns).join("main").is_dir());
}

/// A crate in `dir` whose `main` is `body`, with `build` as its build script.
fn write_program(dir: &Path, name: &str, body: &str, build: Option<&str>) {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    )
    .unwrap();
    fs::write(dir.join("src/main.rs"), format!("fn main() {{ {body} }}\n")).unwrap();
    if let Some(build) = build {
        fs::write(dir.join("build.rs"), format!("fn main() {{ {build} }}\n")).unwrap();
    }
}

/// Builds in the fixture's repository, failing if it has to wait for a slot.
fn builds_without_waiting(fx: &Fixture) {
    fs::write(
        fx.repo.join("src/extra.rs"),
        format!("// {:?}\n", std::time::SystemTime::now()),
    )
    .unwrap();
    let mut child = fx
        .proxy(&fx.repo, &["build", "-q"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = String::new();
    use std::io::Read;
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(child.wait().unwrap().success(), "{stderr}");
    assert!(!stderr.contains(WAITING), "{stderr}");
}

#[test]
fn a_program_cargo_run_started_doesnt_keep_the_slot() {
    let fx = Fixture::new("max-builds = 1\n");
    let app = fx.root.join("server");
    let started = fx.root.join("started");
    write_program(
        &app,
        "server",
        &format!(
            "std::fs::write({:?}, \"\").unwrap(); \
             std::thread::sleep(std::time::Duration::from_secs(60));",
            started.display().to_string()
        ),
        None,
    );
    let mut server = fx.proxy(&app, &["run", "-q"]).spawn().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while !started.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the server never started"
        );
        assert!(server.try_wait().unwrap().is_none(), "the server exited");
        std::thread::sleep(Duration::from_millis(50));
    }
    // Give the proxy time to see cargo became the server.
    std::thread::sleep(Duration::from_millis(500));
    builds_without_waiting(&fx);

    // Stopping the proxy stops what it runs, and it goes the same way.
    // SAFETY: plain syscall sending a signal to our own child.
    unsafe { libc::kill(server.id() as i32, libc::SIGTERM) };
    let status = server.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(libc::SIGTERM), "{status:?}");
}

#[test]
fn a_daemon_a_build_left_running_doesnt_keep_the_slot() {
    let fx = Fixture::new("max-builds = 1\n");
    let app = fx.root.join("spawner");
    let pid_file = fx.root.join("daemon.pid");
    write_program(
        &app,
        "spawner",
        "",
        Some(&format!(
            "let child = std::process::Command::new(\"sleep\").arg(\"60\") \
             .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()) \
             .stderr(std::process::Stdio::null()).spawn().unwrap(); \
             std::fs::write({:?}, child.id().to_string()).unwrap();",
            pid_file.display().to_string()
        )),
    );
    fx.run(&app, &["build", "-q"]);
    let daemon: i32 = fs::read_to_string(&pid_file).unwrap().parse().unwrap();
    // SAFETY: signal 0 only checks the process exists.
    assert_eq!(
        unsafe { libc::kill(daemon, 0) },
        0,
        "the daemon should still run"
    );
    builds_without_waiting(&fx);
    // SAFETY: stopping the daemon this test started.
    unsafe { libc::kill(daemon, libc::SIGKILL) };
}

/// A repository whose crate depends on a crate outside it, as registry
/// dependencies are, with seeding on and `extra` in its files.
fn seeding_fixture(extra: &[(&str, &str)]) -> Fixture {
    let fx = Fixture::new(&format!("seed = true\nreal-cargo = {:?}\n", test_cargo()));
    let helper = fx.root.join("shared/helper");
    fs::create_dir_all(helper.join("src")).unwrap();
    fs::write(
        helper.join("Cargo.toml"),
        "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(helper.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
    let manifest = fx.repo.join("Cargo.toml");
    let mut text = fs::read_to_string(&manifest).unwrap();
    text.push_str("\n[dependencies]\nhelper = { path = \"../shared/helper\" }\n");
    fs::write(&manifest, text).unwrap();
    for (name, contents) in extra {
        let path = fx.repo.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    git(&fx.repo, &["add", "."]);
    git(&fx.repo, &["commit", "-qm", "depend on helper"]);
    fx
}

/// `cargo build -v` through the proxy in `dir`; its output.
fn build_verbose(fx: &Fixture, dir: &Path) -> String {
    let out = fx.run(dir, &["build", "-v"]);
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The proxy's `seed:` line for a build in `dir`.
fn seed_line(fx: &Fixture, dir: &Path) -> String {
    let out = stdout(&fx.steward(dir, &["shim", "explain"]));
    out.lines()
        .find(|l| l.starts_with("seed:"))
        .unwrap_or_default()
        .to_owned()
}

/// Agents often build only in worktrees: a new worktree's target directory
/// starts as a clone of another worktree's when the main checkout has never
/// built, and cargo finds the dependencies fresh there.
#[test]
fn seeds_a_new_worktree_from_another_worktree() {
    let fx = seeding_fixture(&[]);
    let ns = fx.targets.join(fx.repo_ns());
    git(&fx.repo, &["worktree", "add", "-q", "../wt-a"]);
    let a = fx.root.join("wt-a");
    assert!(seed_line(&fx, &a).contains("no: no other worktree has built"));
    let cold = build_verbose(&fx, &a);
    assert!(cold.contains("Compiling helper"), "{cold}");

    git(&fx.repo, &["worktree", "add", "-q", "../wt-b"]);
    let b = fx.root.join("wt-b");
    let line = seed_line(&fx, &b);
    assert!(
        line.starts_with(&format!(
            "seed:         yes, from {} (worktree wt-a, last used ",
            ns.join("wt-a").display()
        )),
        "{line}"
    );
    let warm = build_verbose(&fx, &b);
    if !cfg!(target_os = "macos") && warm.contains("Compiling helper") {
        // This filesystem can't clone: a cold build, as without seeding.
        return;
    }
    assert!(warm.contains("Fresh helper"), "{warm}");
    assert!(ns.join("wt-b/debug").is_dir());
    let names: Vec<String> = fs::read_dir(&ns)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().all(|n| !n.contains("steward")), "{names:?}");
    assert!(seed_line(&fx, &b).contains("no: the target dir already has files"));
}

/// With `build.build-dir` set, intermediate artifacts live outside the
/// target directory the proxy routes: the new worktree's build-dir starts
/// as a clone of the same worktree's that its target directory comes from.
#[test]
fn seeds_a_new_worktrees_build_dir_too() {
    let fx = seeding_fixture(&[
        (".cargo/config.toml", "[build]\nbuild-dir = \"bdir\"\n"),
        (".gitignore", "/target\n/bdir\n"),
    ]);
    git(&fx.repo, &["worktree", "add", "-q", "../wt-a"]);
    let a = fx.root.join("wt-a");
    let out = fx.metadata(&a, &[]).output().unwrap();
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    if json["build_directory"].as_str().map(PathBuf::from) != Some(a.join("bdir")) {
        // A cargo too old to report its build directory.
        return;
    }
    build_verbose(&fx, &a);
    assert!(a.join("bdir").is_dir());

    git(&fx.repo, &["worktree", "add", "-q", "../wt-b"]);
    let b = fx.root.join("wt-b");
    let warm = build_verbose(&fx, &b);
    if !cfg!(target_os = "macos") && warm.contains("Compiling helper") {
        return;
    }
    assert!(warm.contains("Fresh helper"), "{warm}");
    assert!(b.join("bdir/debug").is_dir());
    let out = fx.proxy(&b, &["run", "-q"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
}
