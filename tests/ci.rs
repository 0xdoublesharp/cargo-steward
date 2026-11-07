//! End to end: `ci start`, real cargo builds, and `ci trim` of what the
//! job didn't use, in whichever build-dir layout the cargo under test writes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread::sleep;
use std::time::{Duration, SystemTime};

const BIN: &str = env!("CARGO_BIN_EXE_cargo-steward");

/// `ci start` records whole seconds and counts anything touched in that
/// second as used, so builds before it are spaced further apart.
const BEFORE_START: Duration = Duration::from_millis(1100);

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    project: PathBuf,
    target: PathBuf,
}

impl Fixture {
    /// `app`, depending on path crates `a` and `b`; `b` has a build script.
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for (name, lib) in [
            ("a", "pub fn a() -> u32 { 1 }\n"),
            ("b", "pub fn b() -> u32 { 2 }\n"),
        ] {
            let dir = root.join(name);
            fs::create_dir_all(dir.join("src")).unwrap();
            fs::write(
                dir.join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
            )
            .unwrap();
            fs::write(dir.join("src/lib.rs"), lib).unwrap();
        }
        fs::write(
            root.join("b/build.rs"),
            "fn main() { println!(\"cargo::rerun-if-changed=build.rs\"); }\n",
        )
        .unwrap();
        let project = root.join("app");
        fs::create_dir_all(project.join("src")).unwrap();
        let fx = Fixture {
            target: root.join("target"),
            project,
            root,
            _tmp: tmp,
        };
        fx.depend_on(&["a", "b"]);
        fx
    }

    fn depend_on(&self, deps: &[&str]) {
        let mut manifest =
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n"
                .to_owned();
        let mut sum = String::from("0");
        for dep in deps {
            manifest.push_str(&format!("{dep} = {{ path = \"../{dep}\" }}\n"));
            sum.push_str(&format!(" + {dep}::{dep}()"));
        }
        fs::write(self.project.join("Cargo.toml"), manifest).unwrap();
        fs::write(
            self.project.join("src/main.rs"),
            format!("fn main() {{ println!(\"{{}}\", {sum}); }}\n"),
        )
        .unwrap();
    }

    /// The cargo running the tests, or `STEWARD_TEST_CARGO` (with `RUSTC`
    /// set to its rustc) to check other versions and layouts.
    fn cargo_bin() -> std::ffi::OsString {
        std::env::var_os("STEWARD_TEST_CARGO").unwrap_or_else(|| env!("CARGO").into())
    }

    /// Environment shared by builds and `cargo steward`: everything in the
    /// temporary directory, whatever the user's cargo config says.
    fn env(&self, cmd: &mut Command) {
        cmd.current_dir(&self.project)
            .env("CARGO_TARGET_DIR", &self.target)
            .env("CARGO_BUILD_BUILD_DIR", &self.target)
            .env("CARGO_STEWARD_HOME", self.root.join("steward-home"))
            .env("CARGO_STEWARD_INNER", "")
            .env("CARGO_INCREMENTAL", "1")
            .env("CARGO_TERM_COLOR", "never")
            .env("RUSTC_WRAPPER", "")
            .env_remove("CARGO_BUILD_RUSTC_WRAPPER");
    }

    fn cargo(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(Self::cargo_bin());
        cmd.args(args);
        self.env(&mut cmd);
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "cargo {args:?} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn steward(&self, args: &[&str]) -> String {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .env("CARGO", Self::cargo_bin())
            .env("GITHUB_STEP_SUMMARY", self.summary());
        self.env(&mut cmd);
        let out = cmd.output().unwrap();
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

    fn summary(&self) -> PathBuf {
        self.root.join("step-summary.md")
    }

    /// Whether `ci start` found that reads update access times here.
    fn start(&self) -> bool {
        sleep(BEFORE_START);
        self.steward(&["ci", "start"]);
        let marker = fs::read_to_string(self.target.join(".cargo-steward-ci-start")).unwrap();
        marker.contains("atime=updates")
    }

    /// Asserts `cargo build -v` compiles none of `packages`.
    fn assert_fresh(&self, packages: &[&str]) {
        let out = self.cargo(&["build", "-v"]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        for package in packages {
            assert!(
                stderr.contains(&format!("Fresh {package} v")),
                "expected {package} to be fresh:\n{stderr}"
            );
        }
    }

    /// Everything in the build directory belonging to `package`'s units, in
    /// either layout.
    fn units_of(&self, package: &str) -> Vec<PathBuf> {
        let profile = self.target.join("debug");
        let is_hash = |s: &str| s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit());
        let mut found = Vec::new();
        // Old layout: `.fingerprint/<pkg>-<hash>`, `build/<pkg>-<hash>`,
        // `deps/[lib]<crate>-<hash>*`.
        for dir in [".fingerprint", "build", "deps"] {
            for entry in fs::read_dir(profile.join(dir)).into_iter().flatten() {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                let Some((stem, rest)) = name.split_once('-') else {
                    continue;
                };
                if (stem == package || stem == format!("lib{package}"))
                    && rest.get(..16).is_some_and(is_hash)
                {
                    found.push(path);
                }
            }
        }
        // New layout: `build/<pkg>/<hash>/`.
        for entry in fs::read_dir(profile.join("build").join(package))
            .into_iter()
            .flatten()
        {
            found.push(entry.unwrap().path());
        }
        found
    }

    /// Fingerprint files whose access time `ci start` reset and nothing
    /// restored.
    fn reset_fingerprints(&self) -> Vec<PathBuf> {
        let cutoff = SystemTime::UNIX_EPOCH + Duration::from_secs(86_400);
        walk(&self.target.join("debug"))
            .into_iter()
            .filter(|p| {
                let parent = p.parent().unwrap();
                parent.file_name().is_some_and(|n| n == "fingerprint")
                    || parent
                        .parent()
                        .and_then(|g| g.file_name())
                        .is_some_and(|n| n == ".fingerprint")
            })
            .filter(|p| {
                let meta = fs::symlink_metadata(p).unwrap();
                meta.is_file() && meta.accessed().unwrap() < cutoff
            })
            .collect()
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

#[test]
fn trim_removes_what_the_job_did_not_build_and_keeps_the_rest_fresh() {
    let fx = Fixture::new();
    fx.cargo(&["build"]);
    assert!(!fx.units_of("a").is_empty());
    let b_units = fx.units_of("b");
    // The library, its build script, and the build script's run.
    assert!(b_units.len() >= 3, "{b_units:#?}");

    let atime = fx.start();
    // The job no longer uses `b`.
    fx.depend_on(&["a"]);
    fx.cargo(&["build"]);

    let preview = fx.steward(&["ci", "trim", "--dry-run"]);
    assert!(preview.contains("Would remove"), "{preview}");
    assert_eq!(fx.units_of("b"), b_units, "a dry run removes nothing");

    let trimmed = fx.steward(&["ci", "trim", "-v"]);
    if atime {
        assert!(
            fx.units_of("b").is_empty(),
            "{trimmed}\n{:#?}",
            fx.units_of("b")
        );
        assert!(trimmed.contains("not used in this job"), "{trimmed}");
        let summary = fs::read_to_string(fx.summary()).unwrap();
        assert!(
            summary.contains("| Removed: not used in this job |"),
            "{summary}"
        );
        assert!(
            fx.reset_fingerprints().is_empty(),
            "{:#?}",
            fx.reset_fingerprints()
        );
    } else {
        // `noatime`: without a record of reads, only the budget applies.
        assert!(trimmed.contains("only the budget"), "{trimmed}");
        assert_eq!(fx.units_of("b"), b_units);
    }
    assert!(!fx.units_of("a").is_empty(), "{trimmed}");
    // The workspace's own crate and the incremental caches went by default.
    assert!(trimmed.contains("workspace members"), "{trimmed}");
    assert!(!fx.target.join(".cargo-steward-ci-start").exists());
    fx.assert_fresh(&["a"]);

    // A job that keeps the workspace's crates and incremental caches, and
    // uses everything that's there, removes nothing.
    fx.start();
    fx.cargo(&["build"]);
    let kept = fx.steward(&["ci", "trim", "--keep-workspace", "--keep-incremental"]);
    assert!(kept.contains("nothing"), "{kept}");
    fx.assert_fresh(&["a", "app"]);
    let run = Command::new(fx.target.join("debug/app")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "1");
}

#[test]
fn without_start_trim_applies_only_the_budget() {
    let fx = Fixture::new();
    fx.cargo(&["build"]);
    let trimmed = fx.steward(&[
        "ci",
        "trim",
        "--budget",
        "1",
        "--keep-workspace",
        "--keep-incremental",
    ]);
    assert!(trimmed.contains("only the budget"), "{trimmed}");
    assert!(trimmed.contains("over the 1 B budget"), "{trimmed}");
    for package in ["a", "b", "app"] {
        assert!(fx.units_of(package).is_empty(), "{package}: {trimmed}");
    }
    // Cargo rebuilds what's gone.
    fx.cargo(&["build"]);
}
