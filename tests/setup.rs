//! End to end: `setup`, `skill`, `hooks`, `seed`, `pin`/`unpin` and
//! `remove --worktree` as a user runs them, against temporary homes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_cargo-steward");

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    cargo_home: PathBuf,
    steward: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("home");
        let cargo_home = root.join("cargo-home");
        let steward = root.join("steward");
        for dir in [&home, &cargo_home, &steward] {
            fs::create_dir_all(dir).unwrap();
        }
        Fixture {
            _tmp: tmp,
            root,
            home,
            cargo_home,
            steward,
        }
    }

    /// The binary, isolated from the real home, Cargo config, agents and git
    /// config.
    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .current_dir(&self.root)
            .env("HOME", &self.home)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_STEWARD_HOME", &self.steward)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("RUSTC_WRAPPER")
            .env_remove("CARGO_BUILD_RUSTC_WRAPPER")
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME");
        cmd
    }

    fn run(&self, args: &[&str]) -> String {
        let out = self.cmd(args).output().unwrap();
        assert_ok(&out, args);
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Runs a command that must fail, returning its stderr.
    fn fail(&self, args: &[&str]) -> String {
        let out = self.cmd(args).output().unwrap();
        assert!(!out.status.success(), "{args:?} succeeded");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    fn cargo_config(&self) -> String {
        fs::read_to_string(self.cargo_home.join("config.toml")).unwrap_or_default()
    }

    fn settings(&self) -> String {
        fs::read_to_string(self.steward.join("config.toml")).unwrap_or_default()
    }
}

fn assert_ok(out: &Output, args: &[&str]) {
    assert!(
        out.status.success(),
        "{args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert_ok(&out, args);
}

/// A minimal build directory, as cargo leaves one.
fn build_dir(root: &Path) {
    fs::create_dir_all(root.join("debug/deps")).unwrap();
    fs::write(root.join(".rustc_info.json"), "{}").unwrap();
    fs::write(
        root.join("CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n# created by cargo.\n",
    )
    .unwrap();
    fs::write(root.join("debug/deps/libdep-0123456789abcdef.rlib"), "x").unwrap();
}

#[test]
fn setup_dry_run_writes_nothing() {
    let fx = Fixture::new();
    let out = fx.run(&["setup", "--dry-run", "--inner", "sccache"]);
    assert!(out.contains("build.rustc-wrapper: (none) -> "), "{out}");
    assert!(out.contains("inner wrapper: sccache"), "{out}");
    assert!(out.contains("automatic cleanup: on"), "{out}");
    assert!(out.contains("(dry run: nothing written)"), "{out}");
    assert!(!fx.cargo_home.join("config.toml").exists());
    assert!(!fx.steward.join("config.toml").exists());
}

#[test]
fn setup_detects_scan_roots_and_sets_a_relative_build_dir() {
    let fx = Fixture::new();
    let target = fx.root.join("scratch/proj/target");
    fs::create_dir_all(&target).unwrap();
    let out = fx
        .cmd(&["setup", "--build-dir", "builds", "--no-auto-gc"])
        .env("CARGO_TARGET_DIR", &target)
        .output()
        .unwrap();
    assert_ok(&out, &[]);
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("No scan roots were configured"), "{out}");
    assert!(out.contains("parent of CARGO_TARGET_DIR"), "{out}");
    assert!(out.contains("automatic cleanup: off"), "{out}");
    assert!(!out.contains("Backed up"), "nothing to back up: {out}");

    let config = fx.cargo_config();
    let build_dir = format!("{}/builds/{{workspace-path-hash}}", fx.root.display());
    assert!(config.contains(&build_dir), "{config}");
    let settings = fx.settings();
    assert!(settings.contains("managed-build-dir = true"), "{settings}");
    assert!(settings.contains("auto-gc = false"), "{settings}");
    let parent = fx.root.join("scratch/proj");
    assert!(
        settings.contains(&format!("{:?}", parent.display().to_string())),
        "{settings}"
    );

    // Undo removes the build dir it set, and the wrapper with no inner one.
    let out = fx.run(&["setup", "--undo"]);
    assert!(out.contains("setup --undo doesn't remove these"), "{out}");
    assert!(out.contains("Backed up the previous config"), "{out}");
    assert_eq!(fx.cargo_config().trim(), "");
    assert!(fx.settings().contains("managed-build-dir = false"));
}

#[test]
fn setup_uses_the_legacy_config_and_warns_about_overrides() {
    let fx = Fixture::new();
    let legacy = fx.cargo_home.join("config");
    fs::write(&legacy, "[build]\njobs = 2\n").unwrap();
    let out = fx
        .cmd(&["setup", "--scan-root", "."])
        .env("RUSTC_WRAPPER", "sccache")
        .output()
        .unwrap();
    assert_ok(&out, &[]);
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("warning: RUSTC_WRAPPER=sccache"), "{out}");
    assert!(
        out.contains(&format!("scan root: {}", fx.root.display())),
        "{out}"
    );
    let text = fs::read_to_string(&legacy).unwrap();
    assert!(
        text.contains("cargo-steward") && text.contains("jobs = 2"),
        "{text}"
    );
    assert!(!fx.cargo_home.join("config.toml").exists());
}

#[test]
fn setup_rejects_bad_input() {
    let fx = Fixture::new();
    let err = fx.fail(&["setup", "--inner", "/x/cargo-steward"]);
    assert!(err.contains("can't be cargo-steward itself"), "{err}");
    let err = fx.fail(&["setup", "--scan-root", "missing"]);
    assert!(err.contains("scan root missing"), "{err}");
    fs::write(fx.cargo_home.join("config.toml"), "[build\n").unwrap();
    let err = fx.fail(&["setup"]);
    assert!(err.contains("parsing"), "{err}");
    assert!(fx.settings().is_empty());
}

#[cfg(target_os = "macos")]
#[test]
fn setup_volume_dry_run_plans_without_creating() {
    let fx = Fixture::new();
    let name = format!("cargo-steward-test-{}", std::process::id());
    let out = fx.run(&["setup", "--volume", &name, "--dry-run"]);
    assert!(
        out.contains(&format!("Build volume: /Volumes/{name}")),
        "{out}"
    );
    assert!(
        out.contains("create: diskutil apfs addVolume disk"),
        "{out}"
    );
    assert!(out.contains("sudo tmutil addexclusion"), "{out}");
    assert!(out.contains("cargo steward setup --build-dir"), "{out}");
    assert!(!Path::new("/Volumes").join(&name).exists());
    // A volume that's already mounted (when the temporary directory is on
    // one) is reused.
    if let Ok(rest) = fx.root.strip_prefix("/Volumes")
        && let Some(existing) = rest.iter().next().and_then(|n| n.to_str())
    {
        let out = fx.run(&["setup", "--volume", existing, "--dry-run"]);
        assert!(out.contains("already exists; using it"), "{out}");
        assert!(
            out.contains(&format!("scan root: /Volumes/{existing}")),
            "{out}"
        );
    }
    let err = fx.fail(&["setup", "--volume", ".bad", "--dry-run"]);
    assert!(err.contains("usable volume name"), "{err}");
}

#[test]
fn skill_installs_for_detected_agents_and_reports_status() {
    let fx = Fixture::new();
    let err = fx.fail(&["skill", "install"]);
    assert!(err.contains("found neither"), "{err}");
    let out = fx.run(&["skill", "status"]);
    assert!(out.contains("No agent skills directory found"), "{out}");

    let claude = fx.home.join(".claude");
    let codex = fx.root.join("codex");
    fs::create_dir_all(&claude).unwrap();
    fs::create_dir_all(&codex).unwrap();
    let skill_cmd = |args: &[&str]| {
        let out = fx
            .cmd(args)
            // Pointing at ~/.claude again doesn't install twice.
            .env("CLAUDE_CONFIG_DIR", &claude)
            .env("CODEX_HOME", &codex)
            .output()
            .unwrap();
        assert_ok(&out, args);
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let claude_file = claude.join("skills/cargo-steward/SKILL.md");
    let codex_file = codex.join("skills/cargo-steward/SKILL.md");
    let out = skill_cmd(&["skill", "status"]);
    assert!(
        out.contains(&format!("{}: not installed", claude_file.display())),
        "{out}"
    );
    let out = skill_cmd(&["skill", "install"]);
    assert_eq!(out.matches("Installed ").count(), 2, "{out}");
    assert!(
        out.contains("Agents load it in their next session."),
        "{out}"
    );
    let shown = fx.run(&["skill", "show"]);
    assert_eq!(fs::read_to_string(&claude_file).unwrap(), shown);
    assert_eq!(fs::read_to_string(&codex_file).unwrap(), shown);

    fs::write(&codex_file, "---\nname: cargo-steward\n---\nold\n").unwrap();
    let out = skill_cmd(&["skill", "status"]);
    assert!(out.contains("installed, current"), "{out}");
    assert!(out.contains("installed, out of date"), "{out}");

    let out = skill_cmd(&["skill", "uninstall"]);
    assert_eq!(out.matches("Removed ").count(), 2, "{out}");
    assert!(!claude_file.exists() && !codex_file.exists());
}

#[test]
fn skill_installs_into_given_dirs() {
    let fx = Fixture::new();
    let dir = fx.root.join("skills");
    let out = fx.run(&["skill", "install", "--dir", dir.to_str().unwrap()]);
    assert!(out.contains("Installed"), "{out}");
    assert!(dir.join("cargo-steward/SKILL.md").is_file());
    let out = fx.run(&["skill", "uninstall", "--dir", dir.to_str().unwrap()]);
    assert!(out.contains("Removed"), "{out}");
    let out = fx.run(&["skill", "uninstall", "--dir", dir.to_str().unwrap()]);
    assert!(out.is_empty(), "{out}");
}

#[test]
fn hooks_install_status_and_uninstall() {
    let fx = Fixture::new();
    let repo = fx.root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    let hooks = repo.join(".git/hooks");
    fs::create_dir_all(&hooks).unwrap();
    fs::write(hooks.join("post-checkout"), "#!/bin/sh\necho mine\n").unwrap();
    fs::write(fx.steward.join("config.toml"), "seed = false\n").unwrap();
    let r = repo.to_str().unwrap();

    let out = fx.run(&["hooks", "install", "--repo", r]);
    assert!(
        out.contains("It runs the hook that was there first"),
        "{out}"
    );
    assert!(out.contains("seeding is turned off"), "{out}");
    let out = fx.run(&["hooks", "status", "--repo", r]);
    assert!(out.contains("Installed: "), "{out}");
    assert!(out.contains("so the hook does nothing"), "{out}");

    // Without --repo, the current directory's repository.
    let out = fx
        .cmd(&["hooks", "uninstall"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert_ok(&out, &[]);
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("Removed the cargo-steward hook"), "{out}");
    assert!(out.contains("Restored the post-checkout hook"), "{out}");
    assert_eq!(
        fs::read_to_string(hooks.join("post-checkout")).unwrap(),
        "#!/bin/sh\necho mine\n"
    );
    let out = fx.run(&["hooks", "uninstall", "--repo", r]);
    assert!(out.contains("No cargo-steward hook"), "{out}");

    let err = fx.fail(&["hooks", "status", "--repo", fx.home.to_str().unwrap()]);
    assert!(err.contains("isn't in a git repository"), "{err}");
}

#[test]
fn seed_reports_what_it_did() {
    let fx = Fixture::new();
    let base = fx.root.join("main/target");
    build_dir(&base);
    let b = base.to_str().unwrap();

    let occupied = fx.root.join("occupied");
    fs::create_dir_all(&occupied).unwrap();
    fs::write(occupied.join("notes"), "x").unwrap();
    let out = fx.run(&[
        "seed",
        "--target-dir",
        occupied.to_str().unwrap(),
        "--from",
        b,
    ]);
    assert!(out.contains("it exists and isn't empty"), "{out}");
    let out = fx.run(&["seed", "--target-dir", b, "--from", b]);
    assert!(out.contains("is the main build directory"), "{out}");
    let missing = fx.root.join("nothing");
    let out = fx.run(&[
        "seed",
        "--target-dir",
        occupied.to_str().unwrap(),
        "--from",
        missing.to_str().unwrap(),
    ]);
    assert!(out.contains("No build to seed from"), "{out}");
    let quiet = fx.run(&["seed", "--target-dir", b, "--from", b, "--quiet"]);
    assert!(quiet.is_empty(), "{quiet}");

    // Outside a git repository there's no main worktree to seed from.
    let out = fx.run(&["seed", "--worktree", fx.home.to_str().unwrap()]);
    assert!(out.contains("Nothing to seed"), "{out}");

    fs::write(fx.steward.join("config.toml"), "seed = false\n").unwrap();
    let out = fx.run(&["seed", "--target-dir", b, "--from", b]);
    assert!(out.contains("Seeding is turned off"), "{out}");
}

#[test]
fn pins_and_unpins_a_build_dir() {
    let fx = Fixture::new();
    let target = fx.root.join("target");
    build_dir(&target);
    let inside = target.join("debug");
    let pin = target.join(".cargo-steward-keep");
    let out = fx.run(&["pin", inside.to_str().unwrap()]);
    assert!(
        out.contains(&format!("Pinned {}", target.display())),
        "{out}"
    );
    assert!(pin.is_file(), "the pin is at the build dir's root");
    let out = fx.run(&["unpin", target.to_str().unwrap()]);
    assert!(out.contains("Unpinned"), "{out}");
    assert!(!pin.exists());
    let out = fx.run(&["unpin", target.to_str().unwrap()]);
    assert!(out.contains("wasn't pinned"), "{out}");

    let err = fx.fail(&["pin", fx.home.to_str().unwrap()]);
    assert!(err.starts_with("error: "), "{err}");
    assert!(
        err.contains("isn't inside a cargo build directory"),
        "{err}"
    );
    let err = fx.fail(&["pin", "does-not-exist"]);
    assert!(err.contains("does-not-exist"), "{err}");

    // A pin that can't be removed is an error, and stays.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fx.run(&["pin", target.to_str().unwrap()]);
        fs::set_permissions(&target, fs::Permissions::from_mode(0o555)).unwrap();
        let out = fx
            .cmd(&["unpin", target.to_str().unwrap()])
            .output()
            .unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!out.status.success());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("ermission denied"), "{err}");
        assert!(pin.is_file());
    }
}

/// `setup --volume` with a fake `diskutil` first on PATH, so no volume is
/// ever created.
#[cfg(target_os = "macos")]
#[test]
fn setup_volume_reports_what_diskutil_did() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new();
    let bin = fx.root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let diskutil = bin.join("diskutil");
    let fake = |body: &str| {
        fs::write(&diskutil, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&diskutil, fs::Permissions::from_mode(0o755)).unwrap();
    };
    let name = format!("cargo-steward-test-{}", std::process::id());
    let setup = |dry_run: bool| {
        let mut args = vec!["setup", "--volume", &name];
        if dry_run {
            args.push("--dry-run");
        }
        let out = fx.cmd(&args).env("PATH", &bin).output().unwrap();
        assert!(!out.status.success(), "{args:?} succeeded");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let plist = "<key>APFSContainerReference</key><string>disk9</string>";

    fake("echo 'no such disk' >&2; exit 1");
    let err = setup(true);
    assert!(
        err.contains("diskutil info -plist / failed: no such disk"),
        "{err}"
    );

    fake(&format!(
        "[ \"$1\" = info ] && {{ echo '{plist}'; exit 0; }}; exit 3"
    ));
    let err = setup(false);
    assert!(
        err.contains(&format!("diskutil apfs addVolume disk9 APFS {name} failed")),
        "{err}"
    );

    // Claims success but mounts nothing.
    fake(&format!("[ \"$1\" = info ] && echo '{plist}'; exit 0"));
    let err = setup(false);
    assert!(err.contains("it isn't mounted at /Volumes/"), "{err}");
    assert!(!Path::new("/Volumes").join(&name).exists());
    assert!(!fx.cargo_home.join("config.toml").exists());
    assert!(!fx.steward.join("config.toml").exists());
}

/// Without CARGO_HOME, Cargo's config is the one in ~/.cargo.
#[test]
fn setup_finds_cargo_home_under_home() {
    let fx = Fixture::new();
    let out = fx
        .cmd(&["setup", "--dry-run"])
        .env_remove("CARGO_HOME")
        .output()
        .unwrap();
    assert_ok(&out, &[]);
    let out = String::from_utf8_lossy(&out.stdout);
    let expected = fx.home.join(".cargo/config.toml");
    assert!(out.contains(&expected.display().to_string()), "{out}");
    assert!(!expected.exists());
}

#[test]
fn remove_with_nothing_built_removes_nothing() {
    let fx = Fixture::new();
    let out = fx.run(&["remove"]);
    assert!(out.contains("Nothing to remove."), "{out}");

    let ws = fx.root.join("ws");
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::write(
        ws.join("Cargo.toml"),
        "[package]\nname = \"ws\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(ws.join("src/lib.rs"), "").unwrap();
    let out = fx.run(&["remove", "--worktree", ws.to_str().unwrap()]);
    assert!(
        out.contains(&format!("{} builds into ", ws.display())),
        "{out}"
    );
    assert!(out.contains("Nothing to remove."), "{out}");
}

#[test]
fn background_gc_runs_quietly() {
    let fx = Fixture::new();
    let out = fx.run(&["gc", "--auto"]);
    assert!(out.is_empty(), "{out}");
}

/// The script's `cargo install`s run without the configured rustc-wrapper,
/// which may be a cargo-steward that has gone missing.
#[cfg(unix)]
#[test]
fn install_script_builds_without_the_configured_wrapper() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new();
    let bin = fx.root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let log = fx.root.join("cargo.log");
    let cargo = bin.join("cargo");
    fs::write(
        &cargo,
        "#!/bin/sh\n\
         printf '%s|%s|%s\\n' \"${RUSTC_WRAPPER-unset}\" \"${CARGO_BUILD_RUSTC_WRAPPER-unset}\" \"$*\" >>\"$FAKE_CARGO_LOG\"\n\
         mkdir -p \"$CARGO_HOME/bin\"\n\
         printf '#!/bin/sh\\necho cargo-steward 0.0.0\\n' >\"$CARGO_HOME/bin/cargo-steward\"\n\
         chmod 755 \"$CARGO_HOME/bin/cargo-steward\"\n",
    )
    .unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install.sh");
    let missing = fx.cargo_home.join("bin/cargo-steward");
    let install = |args: &[&str]| {
        let out = Command::new("sh")
            .arg(&script)
            .args(args)
            .arg("--no-setup")
            .env_clear()
            .env("HOME", &fx.home)
            .env("CARGO_HOME", &fx.cargo_home)
            .env("PATH", format!("{}:/bin:/usr/bin", bin.display()))
            .env("FAKE_CARGO_LOG", &log)
            .env("RUSTC_WRAPPER", &missing)
            .env("CARGO_BUILD_RUSTC_WRAPPER", &missing)
            .output()
            .unwrap();
        assert_ok(&out, args);
    };
    install(&["--from-source", env!("CARGO_MANIFEST_DIR")]);
    install(&["--git", "https://example.invalid/x.git"]);
    install(&["--git", "https://example.invalid/x.git", "--rev", "abc"]);
    let lines: Vec<String> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(lines.len(), 3, "{lines:?}");
    for line in &lines {
        assert!(line.starts_with("||install --locked "), "{line}");
    }
}
