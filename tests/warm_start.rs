//! End to end: a new git worktree's build directory is seeded from the main
//! worktree's by the post-checkout hook, and cargo finds its dependencies
//! fresh there.

mod common;

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_cargo-steward");

/// The cargo running the tests, or `STEWARD_TEST_CARGO` (with `RUSTC` set
/// to its rustc) to check other versions and layouts.
fn test_cargo() -> String {
    std::env::var("STEWARD_TEST_CARGO").unwrap_or_else(|_| env!("CARGO").to_owned())
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    home: PathBuf,
    cargo_home: PathBuf,
}

impl Fixture {
    /// A git repository holding a binary crate that depends on a crate
    /// outside the repository, as registry dependencies are: both worktrees
    /// build the same dependency sources.
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let helper = root.join("shared/helper");
        fs::create_dir_all(helper.join("src")).unwrap();
        fs::write(
            helper.join("Cargo.toml"),
            "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            helper.join("src/lib.rs"),
            "pub fn answer() -> u32 { 6 * 7 }\n",
        )
        .unwrap();

        let repo = root.join("repo");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [dependencies]\nhelper = { path = \"../shared/helper\" }\n",
        )
        .unwrap();
        fs::write(
            repo.join("src/main.rs"),
            "fn main() { println!(\"{}\", helper::answer()); }\n",
        )
        .unwrap();
        fs::write(repo.join(".gitignore"), "/target\n").unwrap();

        let home = root.join("steward-home");
        let cargo_home = root.join("cargo-home");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&cargo_home).unwrap();
        let fx = Fixture {
            _tmp: tmp,
            root,
            repo,
            home,
            cargo_home,
        };
        fx.git(&fx.repo, &["init", "-q", "-b", "main"]);
        fx.git(&fx.repo, &["add", "."]);
        fx.git(
            &fx.repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "init",
            ],
        );
        fx
    }

    /// The environment every command runs in: no user git or cargo config,
    /// no wrapper, cargo-steward's state in the fixture, and each worktree's
    /// build output in its own `target/`.
    fn env(&self, cmd: &mut Command) {
        cmd.env("CARGO_STEWARD_HOME", &self.home)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO", test_cargo())
            .env("RUSTC_WRAPPER", "")
            .env("CARGO_TERM_COLOR", "never")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        for var in [
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_TARGET_DIR",
            "CARGO_BUILD_BUILD_DIR",
            "CARGO_BUILD_RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "CARGO_STEWARD_INNER",
        ] {
            cmd.env_remove(var);
        }
        // Commands here, cargo-steward included, run git.
        common::isolate_from_git(cmd);
    }

    fn run(&self, program: &str, dir: &Path, args: &[&str]) -> Output {
        let mut cmd = Command::new(program);
        cmd.args(args).current_dir(dir);
        self.env(&mut cmd);
        cmd.output().unwrap()
    }

    fn ok(&self, program: &str, dir: &Path, args: &[&str]) -> String {
        let out = self.run(program, dir, args);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "{program} {args:?} failed:\n{text}");
        text
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        self.ok("git", dir, args)
    }

    fn cargo(&self, dir: &Path, args: &[&str]) -> String {
        self.ok(&test_cargo(), dir, args)
    }

    fn steward(&self, dir: &Path, args: &[&str]) -> String {
        self.ok(BIN, dir, args)
    }
}

#[test]
fn a_new_worktree_starts_with_the_main_build() {
    let fx = Fixture::new();
    fx.cargo(&fx.repo, &["build"]);
    assert!(fx.repo.join("target/debug").is_dir());

    let installed = fx.steward(&fx.repo, &["hooks", "install"]);
    assert!(installed.contains("Installed"), "{installed}");
    let status = fx.steward(&fx.repo, &["hooks", "status"]);
    assert!(status.starts_with("Installed"), "{status}");

    let wt = fx.root.join("wt");
    fx.git(&fx.repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    let target = wt.join("target");
    if !target.exists() {
        if cfg!(target_os = "macos") {
            panic!("APFS can clone: expected a seed");
        }
        // The hook ran but this filesystem can't clone; seeding says so.
        let said = fx.steward(&wt, &["seed"]);
        assert!(said.contains("can't clone"), "{said}");
        return;
    }
    assert!(target.join("CACHEDIR.TAG").exists());
    assert!(target.join("debug").is_dir());
    // Nothing is left under a temporary name.
    let stray: Vec<String> = fs::read_dir(&wt)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("steward"))
        .collect();
    assert!(stray.is_empty(), "{stray:?}");

    let build = fx.cargo(&wt, &["build", "-v"]);
    assert!(build.contains("Fresh helper"), "{build}");
    let run = fx.run(wt.join("target/debug/app").to_str().unwrap(), &wt, &[]);
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "42");

    // Seeding again is a no-op that says why.
    let again = fx.steward(&wt, &["seed"]);
    assert!(again.contains("already holds a build"), "{again}");

    let removed = fx.steward(&fx.repo, &["hooks", "uninstall"]);
    assert!(removed.contains("Removed"), "{removed}");
    let hooks = PathBuf::from(
        fx.git(&fx.repo, &["rev-parse", "--git-path", "hooks"])
            .trim(),
    );
    assert!(!fx.repo.join(hooks).join("post-checkout").exists());
}

#[test]
fn seed_with_explicit_dirs_and_nothing_to_do() {
    let fx = Fixture::new();
    // No baseline yet: exits 0 and says so.
    let none = fx.steward(
        &fx.repo,
        &[
            "seed",
            "--target-dir",
            fx.root.join("new-target").to_str().unwrap(),
            "--from",
            fx.repo.join("target").to_str().unwrap(),
            "--worktree",
            fx.repo.to_str().unwrap(),
        ],
    );
    assert!(none.contains("No build to seed from"), "{none}");
    assert!(!fx.root.join("new-target").exists());

    fx.cargo(&fx.repo, &["build"]);
    // The interface the cargo proxy uses: quiet, explicit directories.
    let quiet = fx.steward(
        &fx.repo,
        &[
            "seed",
            "--target-dir",
            fx.root.join("new-target").to_str().unwrap(),
            "--from",
            fx.repo.join("target").to_str().unwrap(),
            "--worktree",
            fx.repo.to_str().unwrap(),
            "--quiet",
        ],
    );
    assert_eq!(quiet, "");

    // Turned off in the settings: nothing happens.
    fs::write(fx.home.join("config.toml"), "seed = false\n").unwrap();
    let off = fx.steward(
        &fx.repo,
        &[
            "seed",
            "--target-dir",
            fx.root.join("other-target").to_str().unwrap(),
        ],
    );
    assert!(off.contains("turned off"), "{off}");
    assert!(!fx.root.join("other-target").exists());
}

/// Agents often build only in worktrees. A new worktree is seeded from
/// another worktree's build when the main checkout has none, or when a
/// build is using the main checkout's; `seed` says which it used.
#[test]
fn a_new_worktree_starts_from_another_worktrees_build() {
    let fx = Fixture::new();
    let a = fx.root.join("wt-a");
    fx.git(&fx.repo, &["worktree", "add", "-q", a.to_str().unwrap()]);
    fx.cargo(&a, &["build"]);
    fx.steward(&fx.repo, &["hooks", "install"]);

    // The main checkout has never built: the hook seeds from wt-a.
    let b = fx.root.join("wt-b");
    fx.git(&fx.repo, &["worktree", "add", "-q", b.to_str().unwrap()]);
    if !b.join("target").exists() {
        if cfg!(target_os = "macos") {
            panic!("APFS can clone: expected a seed");
        }
        let said = fx.steward(&b, &["seed"]);
        assert!(said.contains("can't clone"), "{said}");
        return;
    }
    let build = fx.cargo(&b, &["build", "-v"]);
    assert!(build.contains("Fresh helper"), "{build}");
    fx.steward(&fx.repo, &["hooks", "uninstall"]);

    // The main checkout has built, but a build is using it: the most
    // recently used other worktree's build (wt-b's) is used instead.
    fx.cargo(&fx.repo, &["build"]);
    let lock = File::create(fx.repo.join("target/debug/.cargo-lock")).unwrap();
    lock.lock().unwrap();
    let c = fx.root.join("wt-c");
    fx.git(&fx.repo, &["worktree", "add", "-q", c.to_str().unwrap()]);
    let said = fx.steward(&c, &["seed"]);
    assert!(
        said.starts_with(&format!(
            "Seeded {} from {} (worktree wt-b, last used ",
            c.join("target").display(),
            b.join("target").display()
        )),
        "{said}"
    );
    assert!(said.contains("; busy: the main checkout; "), "{said}");
    let build = fx.cargo(&c, &["build", "-v"]);
    assert!(build.contains("Fresh helper"), "{build}");

    // Once no build is using it, the main checkout's is preferred again.
    drop(lock);
    let d = fx.root.join("wt-d");
    fx.git(&fx.repo, &["worktree", "add", "-q", d.to_str().unwrap()]);
    let said = fx.steward(&d, &["seed"]);
    assert!(
        said.starts_with(&format!(
            "Seeded {} from {} (the main checkout; ",
            d.join("target").display(),
            fx.repo.join("target").display()
        )),
        "{said}"
    );
}
