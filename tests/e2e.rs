//! End to end: real cargo builds through the wrapper, and cleanups of real
//! build directories.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread::sleep;
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_cargo-steward");

/// The wrapper keeps objects modified within this long before a compile, so
/// rebuilds in these tests are spaced further apart.
const BETWEEN_BUILDS: Duration = Duration::from_millis(2200);

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    project: PathBuf,
    target: PathBuf,
    home: PathBuf,
}

impl Fixture {
    /// A binary crate split into modules, so incremental builds produce
    /// several codegen units.
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let project = root.join("leakfx");
        fs::create_dir_all(project.join("src")).unwrap();
        fs::write(
            project.join("Cargo.toml"),
            "[package]\nname = \"leakfx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let mut main = String::new();
        for m in 1..=4 {
            fs::write(
                project.join(format!("src/m{m}.rs")),
                format!("pub fn f(x: u64) -> u64 {{ (0..x).map(|i| i * {m} + 1).sum() }}\n"),
            )
            .unwrap();
            main.push_str(&format!("mod m{m};\n"));
        }
        main.push_str(
            "fn main() { println!(\"{}\", m1::f(3) + m2::f(3) + m3::f(3) + m4::f(3)); }\n",
        );
        fs::write(project.join("src/main.rs"), main).unwrap();
        let home = root.join("steward-home");
        fs::create_dir_all(&home).unwrap();
        // These tests rebuild seconds apart and expect each replaced
        // generation gone at once; `tests/safety.rs` covers the grace.
        fs::write(
            home.join("config.toml"),
            format!(
                "scan-roots = [{:?}]\nobject-grace = \"0s\"\n",
                root.display().to_string()
            ),
        )
        .unwrap();
        Fixture {
            target: root.join("target"),
            _tmp: tmp,
            root,
            project,
            home,
        }
    }

    /// Builds with the cargo running the tests, or `STEWARD_TEST_CARGO` (with
    /// `RUSTC` set to its rustc) to check other versions and layouts.
    fn cargo(&self, args: &[&str], wrapped: bool) -> Output {
        let cargo = std::env::var_os("STEWARD_TEST_CARGO").unwrap_or_else(|| env!("CARGO").into());
        let mut cmd = Command::new(cargo);
        cmd.args(args)
            .current_dir(&self.project)
            .env("CARGO_TARGET_DIR", &self.target)
            .env("CARGO_STEWARD_HOME", &self.home)
            .env("CARGO_STEWARD_INNER", "")
            .env("CARGO_INCREMENTAL", "1")
            .env("CARGO_TERM_COLOR", "never")
            .env("RUSTC_WRAPPER", if wrapped { BIN } else { "" })
            .env_remove("CARGO_BUILD_RUSTC_WRAPPER");
        if !cfg!(target_os = "macos") {
            // Only macOS keeps per-compile debug files by default.
            cmd.env("CARGO_PROFILE_DEV_SPLIT_DEBUGINFO", "unpacked");
        }
        cmd.output().unwrap()
    }

    fn build(&self, wrapped: bool) {
        let out = self.cargo(&["build"], wrapped);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn edit(&self, n: u32) {
        let file = self.project.join(format!("src/m{}.rs", n % 4 + 1));
        let mut text = fs::read_to_string(&file).unwrap();
        text.push_str(&format!("pub fn extra{n}() -> u32 {{ {n} }}\n"));
        fs::write(file, text).unwrap();
    }

    fn steward(&self, args: &[&str]) -> String {
        let out = Command::new(BIN)
            .args(args)
            .env("CARGO_STEWARD_HOME", &self.home)
            .env_remove("CARGO")
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.status.success(),
            "cargo-steward {args:?} failed:\n{text}"
        );
        text
    }

    fn objects(&self) -> usize {
        walk(&self.target)
            .iter()
            .filter(|p| {
                let name = p.file_name().unwrap().to_string_lossy();
                name.starts_with("leakfx")
                    && (name.ends_with(".rcgu.o") || name.ends_with(".rcgu.dwo"))
            })
            .count()
    }

    /// Asserts nothing would be rebuilt.
    fn assert_fresh(&self, wrapped: bool) {
        let out = self.cargo(&["build", "-v"], wrapped);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("Fresh leakfx"),
            "expected a fresh build:\n{stderr}"
        );
    }

    fn binary(&self) -> PathBuf {
        self.target.join("debug/leakfx")
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                stack.push(path.clone());
            }
            out.push(path);
        }
    }
    out
}

/// On macOS, the object files a binary's debug map points at must exist, or
/// debuggers and backtraces lose its symbols.
fn assert_debug_map_intact(binary: &Path) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let out = Command::new("nm").arg("-pa").arg(binary).output().unwrap();
    let listing = String::from_utf8_lossy(&out.stdout);
    let referenced: Vec<&str> = listing
        .lines()
        .filter(|l| l.contains(" OSO "))
        .filter_map(|l| l.split_whitespace().last())
        // `lib.rlib(member.o)` points into an archive.
        .map(|p| p.split_once('(').map_or(p, |(archive, _)| archive))
        .collect();
    assert!(
        !referenced.is_empty(),
        "no debug map in {}",
        binary.display()
    );
    let missing: Vec<&&str> = referenced
        .iter()
        .filter(|p| !Path::new(p).exists())
        .collect();
    assert!(
        missing.is_empty(),
        "debug map points at missing files: {missing:#?}"
    );
}

#[test]
fn the_wrapper_keeps_one_generation_of_objects() {
    let fx = Fixture::new();
    fx.build(true);
    let generation = fx.objects();
    assert!(generation > 0, "the fixture should keep debug objects");
    for n in 0..3 {
        sleep(BETWEEN_BUILDS);
        fx.edit(n);
        fx.build(true);
        assert_eq!(fx.objects(), generation, "after rebuild {n}");
    }
    fx.assert_fresh(true);
    assert_debug_map_intact(&fx.binary());
    let run = Command::new(fx.binary()).output().unwrap();
    // Each m::f(3) is 3 + 3m: 6 + 9 + 12 + 15.
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "42");

    // The compile was recorded against the build directory and its package.
    let ledger = fs::read_to_string(fx.home.join("ledger.jsonl")).unwrap();
    assert!(ledger.contains(&*fx.target.to_string_lossy()), "{ledger}");
    assert!(ledger.contains(&*fx.project.to_string_lossy()), "{ledger}");
}

/// Whether the cargo under test clears each unit's output before compiling
/// it, which ended the leak (rust-lang/cargo#17675, from 1.101 nightlies of
/// 2026-11-04).
fn cargo_clears_outputs() -> bool {
    let cargo = std::env::var_os("STEWARD_TEST_CARGO").unwrap_or_else(|| env!("CARGO").into());
    let out = Command::new(cargo).arg("-V").output().unwrap();
    // `cargo 1.101.0-nightly (111443f3d 2026-11-05)`
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut words = text.split_whitespace();
    let version = words.nth(1).unwrap_or_default();
    let minor: u32 = version
        .split('.')
        .nth(1)
        .and_then(|m| m.parse().ok())
        .unwrap_or(0);
    let date = text
        .trim()
        .trim_end_matches(')')
        .rsplit(' ')
        .next()
        .unwrap_or("");
    minor > 101 || (minor == 101 && (!version.contains("nightly") || date >= "2026-11-04"))
}

#[test]
fn gc_removes_an_existing_leak_without_forcing_rebuilds() {
    let fx = Fixture::new();
    fx.build(false);
    let generation = fx.objects();
    for n in 0..2 {
        fx.edit(n);
        fx.build(false);
    }
    if cargo_clears_outputs() {
        // Nothing leaks, so there's nothing for cleanup to find.
        assert_eq!(fx.objects(), generation, "this cargo shouldn't leak");
        assert!(!fx.steward(&["gc"]).contains("stale object files"));
        return;
    }
    assert_eq!(fx.objects(), 3 * generation, "unwrapped builds leak");

    let preview = fx.steward(&["gc"]);
    assert!(preview.contains("stale object files"), "{preview}");
    assert_eq!(fx.objects(), 3 * generation, "a preview removes nothing");

    let applied = fx.steward(&["gc", "--apply"]);
    assert!(applied.contains("Removed"), "{applied}");
    assert_eq!(fx.objects(), generation);
    fx.assert_fresh(false);
    assert_debug_map_intact(&fx.binary());
}

#[test]
fn failed_compiles_pass_through_and_keep_their_objects() {
    let fx = Fixture::new();
    fx.build(true);
    let before = fx.objects();
    sleep(BETWEEN_BUILDS);
    fs::write(
        fx.project.join("src/m1.rs"),
        "pub fn f(x: u64) -> u64 { x + }\n",
    )
    .unwrap();
    let out = fx.cargo(&["build"], true);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("expected expression"), "{stderr}");
    // The wrapper removes nothing after a failure. (Cargo 1.101 clears the
    // unit's output itself before compiling.)
    let expected = if cargo_clears_outputs() { 0 } else { before };
    assert_eq!(fx.objects(), expected);
}

#[test]
fn status_lists_build_dirs_found_under_scan_roots() {
    let fx = Fixture::new();
    fx.build(false);
    let status = fx.steward(&["status", "--sizes"]);
    assert!(status.contains(&*fx.target.to_string_lossy()), "{status}");
    // Cargo's `cargo steward` form works too.
    let via_cargo = fx.steward(&["steward", "status"]);
    assert!(
        via_cargo.contains(&*fx.target.to_string_lossy()),
        "{via_cargo}"
    );
}

#[test]
fn setup_swaps_the_wrapper_in_and_undo_restores_it() {
    let fx = Fixture::new();
    let cargo_home = fx.root.join("cargo-home");
    fs::create_dir_all(&cargo_home).unwrap();
    let config = cargo_home.join("config.toml");
    fs::write(
        &config,
        "# mine\n[build]\nrustc-wrapper = \"sccache\"\njobs = 4\n",
    )
    .unwrap();
    let run = |args: &[&str]| {
        let out = Command::new(BIN)
            .args(args)
            .env("CARGO_HOME", &cargo_home)
            .env("CARGO_STEWARD_HOME", &fx.home)
            .env_remove("RUSTC_WRAPPER")
            .env_remove("CARGO_BUILD_RUSTC_WRAPPER")
            .env_remove("CARGO")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };

    run(&["setup", "--scan-root", fx.root.to_str().unwrap()]);
    let text = fs::read_to_string(&config).unwrap();
    assert!(text.contains("cargo-steward"), "{text}");
    assert!(
        text.contains("# mine") && text.contains("jobs = 4"),
        "{text}"
    );
    let settings = fs::read_to_string(fx.home.join("config.toml")).unwrap();
    assert!(
        settings.contains("inner-wrapper = \"sccache\""),
        "{settings}"
    );
    assert!(settings.contains("auto-gc = true"), "{settings}");
    let backups = fs::read_dir(&cargo_home)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("steward-backup"))
        .count();
    assert_eq!(backups, 1);

    // Running setup again keeps sccache as the inner wrapper.
    run(&["setup"]);
    let settings = fs::read_to_string(fx.home.join("config.toml")).unwrap();
    assert!(
        settings.contains("inner-wrapper = \"sccache\""),
        "{settings}"
    );

    run(&["setup", "--undo"]);
    let text = fs::read_to_string(&config).unwrap();
    assert!(text.contains("rustc-wrapper = \"sccache\""), "{text}");
    assert!(!text.contains("cargo-steward"), "{text}");
}

#[test]
fn remove_takes_only_build_dirs_and_previews_first() {
    let fx = Fixture::new();
    fx.build(false);

    // Something that isn't a build directory is refused, and left alone.
    let src = fx.project.join("src");
    let out = Command::new(BIN)
        .args(["remove", "--apply"])
        .arg(&src)
        .env("CARGO_STEWARD_HOME", &fx.home)
        .env_remove("CARGO")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("refusing"));
    assert!(src.join("main.rs").exists());

    let target = fx.target.to_str().unwrap().to_owned();
    let preview = fx.steward(&["remove", &target]);
    assert!(preview.contains("Would remove"), "{preview}");
    assert!(fx.target.exists());

    // A pinned one needs --force.
    fx.steward(&["pin", &target]);
    let pinned = fx.steward(&["remove", "--apply", &target]);
    assert!(pinned.contains("pinned"), "{pinned}");
    assert!(fx.target.exists());

    fx.steward(&["remove", "--apply", "--force", &target]);
    assert!(!fx.target.exists());
}

/// Two worktrees' builds started at once through the wrapper, sharing one
/// cargo-steward home whose settings add `settings`. The inner wrapper is a
/// script that marks each compile as running while it runs, and records an
/// overlap when one starts while another runs. With `wait_for_peer`, each
/// compile waits (up to a few seconds) for the other to start. Returns the
/// steward home and the recorded log directory.
fn concurrent_builds(settings: &str, wait_for_peer: bool) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("steward-home");
    let log = root.join("log");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&log).unwrap();
    fs::write(home.join("config.toml"), settings).unwrap();
    let pause = if wait_for_peer {
        "i=0; while [ \"$(ls \"$log\" | grep -c '^started')\" -lt 2 ] && [ $i -lt 100 ]; do \
         sleep 0.1; i=$((i+1)); done"
    } else {
        "sleep 1"
    };
    let script = root.join("inner.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             case \" $* \" in *\" --out-dir \"*) ;; *) exec \"$@\" ;; esac\n\
             log={log:?}\n\
             mkdir \"$log/busy\" 2>/dev/null || echo overlap >> \"$log/overlaps\"\n\
             : > \"$log/started.$$\"\n\
             {pause}\n\
             \"$@\"; status=$?\n\
             rmdir \"$log/busy\" 2>/dev/null\n\
             exit $status\n",
            log = log.display().to_string()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let cargo = std::env::var_os("STEWARD_TEST_CARGO").unwrap_or_else(|| env!("CARGO").into());
    let builds: Vec<_> = ["one", "two"]
        .iter()
        .map(|name| {
            let project = root.join(name);
            fs::create_dir_all(project.join("src")).unwrap();
            fs::write(
                project.join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
            )
            .unwrap();
            fs::write(project.join("src/main.rs"), "fn main() {}\n").unwrap();
            Command::new(&cargo)
                .arg("build")
                .current_dir(&project)
                .env("CARGO_TARGET_DIR", project.join("target"))
                .env("CARGO_STEWARD_HOME", &home)
                .env("CARGO_STEWARD_INNER", &script)
                .env_remove("CARGO_STEWARD_MAX_COMPILES")
                .env("CARGO_TERM_COLOR", "never")
                .env("RUSTC_WRAPPER", BIN)
                .env_remove("CARGO_BUILD_RUSTC_WRAPPER")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for build in builds {
        let out = build.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let started = fs::read_dir(&log)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("started"))
        .count();
    assert_eq!(started, 2, "each build compiles once");
    (tmp, home)
}

#[test]
fn max_compiles_runs_one_compile_at_a_time_across_builds() {
    let (tmp, home) = concurrent_builds("max-compiles = 1\n", false);
    let log = tmp.path().canonicalize().unwrap().join("log");
    assert!(
        !log.join("overlaps").exists(),
        "two compiles ran at once despite max-compiles = 1"
    );
    // One slot, released: nothing holds it now.
    let slot = home.join("compile-slots/0.lock");
    let file = fs::File::options().write(true).open(&slot).unwrap();
    file.try_lock().unwrap();
}

#[test]
fn without_max_compiles_compiles_run_at_once() {
    let (tmp, home) = concurrent_builds("", true);
    let log = tmp.path().canonicalize().unwrap().join("log");
    assert!(
        log.join("overlaps").exists(),
        "the compiles should have run at the same time"
    );
    assert!(!home.join("compile-slots").exists());
}
