//! End to end: cleanup of individual build units in Cargo 1.100's layout,
//! against real cargo builds. With the cargo running the tests on an older
//! layout these return early; run them with
//! `STEWARD_TEST_CARGO=$(rustup +beta which cargo) RUSTC=$(rustup +beta which rustc)`.

use std::fs::{self, File, FileTimes};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

const BIN: &str = env!("CARGO_BIN_EXE_cargo-steward");
const DAY: Duration = Duration::from_secs(86_400);

struct Fixture {
    _tmp: tempfile::TempDir,
    app: PathBuf,
    dep: PathBuf,
    target: PathBuf,
    home: PathBuf,
}

impl Fixture {
    /// `app`, which depends on the path crate `dep`, which has a build script.
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let dep = root.join("dep");
        fs::create_dir_all(dep.join("src")).unwrap();
        fs::write(
            dep.join("Cargo.toml"),
            "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            dep.join("build.rs"),
            "fn main() { println!(\"cargo:rerun-if-changed=build.rs\"); }\n",
        )
        .unwrap();
        fs::write(dep.join("src/lib.rs"), "pub fn one() -> u32 { 1 }\n").unwrap();
        let app = root.join("app");
        fs::create_dir_all(app.join("src")).unwrap();
        let home = root.join("steward-home");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("config.toml"),
            format!("scan-roots = [{:?}]\n", root.display().to_string()),
        )
        .unwrap();
        let fx = Fixture {
            target: root.join("target"),
            _tmp: tmp,
            app,
            dep,
            home,
        };
        fx.use_dep(true);
        fx
    }

    fn use_dep(&self, on: bool) {
        let (dependency, body) = if on {
            (
                "dep = { path = \"../dep\" }\n",
                "println!(\"{}\", dep::one());",
            )
        } else {
            ("", "println!(\"1\");")
        };
        fs::write(
            self.app.join("Cargo.toml"),
            format!(
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                 [dependencies]\n{dependency}"
            ),
        )
        .unwrap();
        fs::write(
            self.app.join("src/main.rs"),
            format!("fn main() {{ {body} }}\n"),
        )
        .unwrap();
    }

    fn cargo(&self, args: &[&str]) -> Output {
        let cargo = std::env::var_os("STEWARD_TEST_CARGO").unwrap_or_else(|| env!("CARGO").into());
        let out = Command::new(cargo)
            .args(args)
            .current_dir(&self.app)
            .env("CARGO_TARGET_DIR", &self.target)
            .env("CARGO_STEWARD_HOME", &self.home)
            .env("RUSTC_WRAPPER", "")
            .env("CARGO_TERM_COLOR", "never")
            .env_remove("CARGO_BUILD_RUSTC_WRAPPER")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "cargo {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn steward(&self, args: &[&str]) -> String {
        let out = Command::new(BIN)
            .args(args)
            .env("CARGO_STEWARD_HOME", &self.home)
            .env_remove("CARGO")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "{text}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        text
    }

    /// `dep`'s units: its library, its build script, and the script's run.
    fn dep_units(&self) -> Vec<PathBuf> {
        fs::read_dir(self.target.join("debug/build/dep"))
            .map(|dir| dir.flatten().map(|e| e.path()).collect())
            .unwrap_or_default()
    }
}

/// Backdates everything under `path` to `when`, as if it was built then and
/// only read since: reading doesn't move a file's times on APFS or
/// `noatime`. One `when` for everything, so cargo's own comparisons of these
/// times come out as they did.
fn age(path: &Path, when: SystemTime) {
    let times = FileTimes::new().set_accessed(when).set_modified(when);
    let mut stack = vec![path.to_path_buf()];
    let mut all = Vec::new();
    while let Some(p) = stack.pop() {
        if p.is_dir() {
            stack.extend(fs::read_dir(&p).unwrap().flatten().map(|e| e.path()));
        }
        all.push(p);
    }
    for p in all.iter().rev().chain(all.iter()) {
        if let Ok(file) = File::open(p) {
            let _ = file.set_times(times);
        }
    }
}

#[test]
fn dependencies_in_use_are_kept_however_old_and_dropped_ones_go() {
    let fx = Fixture::new();
    fx.cargo(&["build"]);
    let units = fx.dep_units();
    if units.is_empty() {
        // The old layout: units aren't removed one by one.
        return;
    }
    assert_eq!(units.len(), 3, "{units:?}");
    // `dep` was built a month ago and every build since only read it.
    let month_ago = SystemTime::now() - 30 * DAY;
    age(&fx.dep, month_ago);
    for unit in &units {
        age(unit, month_ago);
    }
    let preview = fx.steward(&["gc"]);
    assert!(!preview.contains("idle build units"), "{preview}");
    fx.steward(&["gc", "--apply"]);
    assert_eq!(fx.dep_units(), units);
    let rebuilt = fx.cargo(&["build", "-v"]);
    let stderr = String::from_utf8_lossy(&rebuilt.stderr);
    assert!(stderr.contains("Fresh dep"), "{stderr}");

    // Once `app` stops depending on it (and the build of `app` that did is
    // as old), nothing needs `dep`.
    let app_units = fx.target.join("debug/build/app");
    age(&app_units, month_ago);
    fx.use_dep(false);
    fx.cargo(&["build"]);
    let applied = fx.steward(&["gc", "--apply", "-v"]);
    assert!(applied.contains("idle build units"), "{applied}");
    assert!(fx.dep_units().is_empty(), "{:?}", fx.dep_units());
}
