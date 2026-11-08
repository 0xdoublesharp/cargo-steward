//! End to end: what cleanup and the wrapper hold back for programs that may
//! still be running, and how sizes are reported.

mod common;

use std::fs::{self, File, FileTimes};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, SystemTime};

const BIN: &str = env!("CARGO_BIN_EXE_cargo-steward");

const DAY: Duration = Duration::from_secs(86_400);

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}

impl Fixture {
    /// A scan root and a cargo-steward home whose settings add `settings`.
    fn new(settings: &str) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("steward-home");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("config.toml"),
            format!(
                "scan-roots = [{:?}]\n{settings}",
                root.display().to_string()
            ),
        )
        .unwrap();
        Fixture {
            _tmp: tmp,
            root,
            home,
        }
    }

    /// An old-layout build directory nothing has used for `idle`.
    fn build_dir(&self, name: &str, idle: Duration) -> PathBuf {
        let dir = self.root.join(name);
        fs::create_dir_all(dir.join("debug/.fingerprint/a-0123456789abcdef")).unwrap();
        fs::create_dir_all(dir.join("debug/deps")).unwrap();
        fs::write(
            dir.join("CACHEDIR.TAG"),
            "Signature: x\n# created by cargo.\n",
        )
        .unwrap();
        fs::write(dir.join("debug/.fingerprint/a-0123456789abcdef/lib-a"), "x").unwrap();
        fs::write(
            dir.join("debug/deps/liba-0123456789abcdef.rlib"),
            vec![1u8; 1 << 20],
        )
        .unwrap();
        age(&dir, idle);
        dir
    }

    fn steward(&self, args: &[&str]) -> String {
        let out = Command::new(BIN)
            .args(args)
            .env("CARGO_STEWARD_HOME", &self.home)
            .env_remove("CARGO")
            .output()
            .unwrap();
        let text = text(&out);
        assert!(
            out.status.success(),
            "cargo-steward {args:?} failed:\n{text}"
        );
        text
    }

    /// Runs cargo-steward expecting it to fail; returns its output.
    fn steward_fails(&self, args: &[&str]) -> String {
        let out = Command::new(BIN)
            .args(args)
            .env("CARGO_STEWARD_HOME", &self.home)
            .env_remove("CARGO")
            .output()
            .unwrap();
        let text = text(&out);
        assert!(
            !out.status.success(),
            "cargo-steward {args:?} succeeded:\n{text}"
        );
        text
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Backdates everything under `path`, files before their directories.
fn age(path: &Path, by: Duration) {
    let when = SystemTime::now() - by;
    let times = FileTimes::new().set_accessed(when).set_modified(when);
    let mut all = Vec::new();
    let mut stack = vec![path.to_path_buf()];
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

/// Set for a copy of this test binary started as [`sleeper`]'s program.
const SLEEP_VAR: &str = "CARGO_STEWARD_TEST_SLEEP";

/// Does nothing in a normal run. Started with [`SLEEP_VAR`] set, it is a
/// program that keeps running until killed.
#[test]
fn sleeper() {
    if std::env::var_os(SLEEP_VAR).is_some() {
        sleep(Duration::from_secs(120));
    }
}

/// Kills the child when dropped, so a failed assertion leaves nothing running.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts a copy of this test binary from `dir`, as a test binary or a
/// server started from a build directory would run. (A copied system binary
/// such as `sleep` can be killed on start by code-signing policy.)
fn run_from(dir: &Path) -> Running {
    let exe = dir.join("sleeper");
    fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
    let child = common::spawn_retrying(
        Command::new(&exe)
            .args(["--exact", "sleeper", "--test-threads=1", "-q"])
            .env(SLEEP_VAR, "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );
    let mut running = Running(child);
    sleep(Duration::from_millis(200));
    assert!(
        running.0.try_wait().unwrap().is_none(),
        "the copy didn't start"
    );
    running
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn gc_keeps_build_dirs_running_programs_were_started_from() {
    let fx = Fixture::new("");
    let busy = fx.build_dir("busy", 40 * DAY);
    let idle = fx.build_dir("idle", 40 * DAY);
    let program = run_from(&busy.join("debug"));
    age(&busy, 40 * DAY);

    let preview = fx.steward(&["gc"]);
    assert!(preview.contains("Kept"), "{preview}");
    assert!(
        preview.contains(&format!("pid {}", program.0.id())),
        "{preview}"
    );

    let applied = fx.steward(&["gc", "--apply"]);
    assert!(busy.exists(), "{applied}");
    assert!(!idle.exists(), "{applied}");
    assert!(applied.contains("Freed on disk"), "{applied}");

    drop(program);
    let applied = fx.steward(&["gc", "--apply"]);
    assert!(!busy.exists(), "{applied}");
    assert!(!applied.contains("Kept"), "{applied}");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn the_process_guard_can_be_turned_off() {
    let fx = Fixture::new("process-guard = false\n");
    let busy = fx.build_dir("busy", 40 * DAY);
    let _program = run_from(&busy.join("debug"));
    age(&busy, 40 * DAY);
    let applied = fx.steward(&["gc", "--apply"]);
    assert!(!busy.exists(), "{applied}");
    assert!(!applied.contains("Kept"), "{applied}");
}

#[test]
fn status_shows_both_sizes() {
    let fx = Fixture::new("");
    let dir = fx.build_dir("target", Duration::ZERO);
    let status = fx.steward(&["status", "--sizes"]);
    let header = status.lines().next().unwrap_or_default();
    assert!(
        header.contains("SIZE") && header.contains("UNSHARED"),
        "{status}"
    );
    let row = status
        .lines()
        .find(|l| l.contains(&*dir.to_string_lossy()))
        .unwrap_or_else(|| panic!("{status}"));
    assert!(row.contains("MiB"), "{row}");
}

#[test]
fn gc_preview_hint_names_gc() {
    let fx = Fixture::new("");
    fx.build_dir("old", 40 * DAY);
    let preview = fx.steward(&["gc"]);
    assert!(
        preview.contains("Run `cargo steward gc --apply`"),
        "{preview}"
    );
}

#[test]
fn remove_preview_hint_names_remove_and_keeps_the_dir() {
    let fx = Fixture::new("");
    let dir = fx.build_dir("target", Duration::ZERO);
    let preview = fx.steward(&["remove", dir.to_str().unwrap()]);
    assert!(dir.exists(), "{preview}");
    assert!(
        preview.contains("Run `cargo steward remove --apply`"),
        "{preview}"
    );
    assert!(!preview.contains("steward gc --apply"), "{preview}");
}

#[test]
fn remove_skips_non_build_dirs_and_removes_the_rest() {
    let fx = Fixture::new("");
    let dir = fx.build_dir("target", Duration::ZERO);
    let other = fx.root.join("src");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("lib.rs"), "").unwrap();

    let applied = fx.steward(&[
        "remove",
        "--apply",
        other.to_str().unwrap(),
        dir.to_str().unwrap(),
    ]);
    assert!(!dir.exists(), "{applied}");
    assert!(other.join("lib.rs").exists(), "{applied}");
    assert!(
        applied.contains(&format!("refusing to remove {}", other.display())),
        "{applied}"
    );
}

#[test]
fn remove_fails_when_nothing_is_a_build_dir() {
    let fx = Fixture::new("");
    let a = fx.root.join("a");
    let b = fx.root.join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    let out = fx.steward_fails(&[
        "remove",
        "--apply",
        a.to_str().unwrap(),
        b.to_str().unwrap(),
    ]);
    assert!(a.exists() && b.exists(), "{out}");
    assert!(out.contains("nothing to remove"), "{out}");
    assert!(
        out.contains(&format!("refusing to remove {}", a.display())),
        "{out}"
    );
    assert!(
        out.contains(&format!("refusing to remove {}", b.display())),
        "{out}"
    );
}

#[test]
fn remove_skips_a_missing_path_and_removes_the_rest() {
    let fx = Fixture::new("");
    let dir = fx.build_dir("target", Duration::ZERO);
    let missing = fx.root.join("missing");
    let applied = fx.steward(&[
        "remove",
        "--apply",
        dir.to_str().unwrap(),
        missing.to_str().unwrap(),
    ]);
    assert!(!dir.exists(), "{applied}");
    assert!(
        applied.contains(&format!(
            "refusing to remove {}: it doesn't exist",
            missing.display()
        )),
        "{applied}"
    );
}

#[test]
fn remove_fails_when_every_path_is_missing() {
    let fx = Fixture::new("");
    let missing = fx.root.join("missing");
    let out = fx.steward_fails(&["remove", "--apply", missing.to_str().unwrap()]);
    assert!(out.contains("nothing to remove"), "{out}");
}

#[test]
fn remove_keeps_pinned_dirs_unless_forced() {
    let fx = Fixture::new("");
    let dir = fx.build_dir("target", Duration::ZERO);
    fx.steward(&["pin", dir.to_str().unwrap()]);

    let applied = fx.steward(&["remove", "--apply", dir.to_str().unwrap()]);
    assert!(dir.exists(), "{applied}");
    assert!(applied.contains("pinned"), "{applied}");

    let forced = fx.steward(&["remove", "--apply", "--force", dir.to_str().unwrap()]);
    assert!(!dir.exists(), "{forced}");
}

/// A binary crate with a few modules, built through the wrapper.
struct Project {
    dir: PathBuf,
    target: PathBuf,
}

impl Project {
    fn new(fx: &Fixture) -> Project {
        let dir = fx.root.join("graced");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"graced\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let mut main = String::new();
        for m in 1..=3 {
            fs::write(
                dir.join(format!("src/m{m}.rs")),
                format!("pub fn f(x: u64) -> u64 {{ x * {m} }}\n"),
            )
            .unwrap();
            main.push_str(&format!("mod m{m};\n"));
        }
        // Keeps running when asked to, as a server or a slow test would.
        main.push_str(
            "fn main() {\n\
             if std::env::var_os(\"GRACED_SLEEP\").is_some() {\n\
             std::thread::sleep(std::time::Duration::from_secs(120));\n\
             }\n\
             println!(\"{}\", m1::f(1) + m2::f(1) + m3::f(1));\n\
             }\n",
        );
        fs::write(dir.join("src/main.rs"), main).unwrap();
        Project {
            target: fx.root.join("target"),
            dir,
        }
    }

    fn cargo(&self, fx: &Fixture) -> Command {
        let cargo = std::env::var_os("STEWARD_TEST_CARGO").unwrap_or_else(|| env!("CARGO").into());
        let mut cmd = Command::new(cargo);
        cmd.current_dir(&self.dir)
            .env("CARGO_TARGET_DIR", &self.target)
            .env("CARGO_STEWARD_HOME", &fx.home)
            .env("CARGO_STEWARD_INNER", "")
            .env("CARGO_INCREMENTAL", "1")
            .env("RUSTC_WRAPPER", BIN)
            .env_remove("CARGO_BUILD_RUSTC_WRAPPER");
        if !cfg!(target_os = "macos") {
            cmd.env("CARGO_PROFILE_DEV_SPLIT_DEBUGINFO", "unpacked");
        }
        cmd
    }

    fn build(&self, fx: &Fixture) {
        let out = self.cargo(fx).arg("build").output().unwrap();
        assert!(out.status.success(), "{}", text(&out));
    }

    /// Builds without the wrapper, so replaced objects stay for cleanup.
    fn build_unwrapped(&self, fx: &Fixture) {
        let out = self
            .cargo(fx)
            .env("RUSTC_WRAPPER", "")
            .arg("build")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out));
    }

    /// Starts the built binary from the copy cargo uplifted into the
    /// profile directory, kept running until dropped.
    fn run(&self) -> Running {
        let child = Command::new(self.target.join("debug/graced"))
            .env("GRACED_SLEEP", "1")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut running = Running(child);
        sleep(Duration::from_millis(200));
        assert!(running.0.try_wait().unwrap().is_none(), "it didn't start");
        running
    }

    fn edit(&self, n: u32) {
        let file = self.dir.join(format!("src/m{}.rs", n % 3 + 1));
        let mut source = fs::read_to_string(&file).unwrap();
        source.push_str(&format!("pub fn extra{n}() -> u32 {{ {n} }}\n"));
        fs::write(file, source).unwrap();
    }

    fn objects(&self) -> usize {
        let mut count = 0;
        let mut stack = vec![self.target.clone()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if entry.file_type().unwrap().is_dir() {
                    stack.push(entry.path());
                } else if name.starts_with("graced")
                    && (name.ends_with(".rcgu.o") || name.ends_with(".rcgu.dwo"))
                {
                    count += 1;
                }
            }
        }
        count
    }
}

/// Whether the cargo under test leaves replaced objects behind: Cargo 1.101
/// clears each unit's outputs before compiling it (rust-lang/cargo#17675),
/// so nothing is left to keep.
fn leaks(fx: &Fixture, project: &Project, generation: usize) -> bool {
    sleep(Duration::from_millis(2200));
    project.edit(100);
    project.build_unwrapped(fx);
    let leaked = project.objects() > generation;
    project.build(fx);
    leaked
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn the_wrapper_keeps_replaced_objects_while_the_binary_runs() {
    let fx = Fixture::new("");
    let project = Project::new(&fx);
    project.build(&fx);
    let generation = project.objects();
    assert!(generation > 0, "the project should keep debug objects");

    sleep(Duration::from_millis(2200));
    project.edit(0);
    project.build(&fx);
    assert_eq!(
        project.objects(),
        generation,
        "with nothing running, a rebuild within the grace keeps only the current objects"
    );

    let program = project.run();
    sleep(Duration::from_millis(2200));
    project.edit(1);
    project.build(&fx);
    let while_running = project.objects();
    if while_running == generation && !leaks(&fx, &project, generation) {
        return;
    }
    assert_eq!(
        while_running,
        2 * generation,
        "a rebuild while the binary runs keeps the objects it was linked from"
    );

    drop(program);
    sleep(Duration::from_millis(2200));
    project.edit(2);
    project.build(&fx);
    assert_eq!(
        project.objects(),
        generation,
        "once it stops, only the current objects stay"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn gc_keeps_replaced_objects_while_the_binary_runs() {
    let fx = Fixture::new("");
    let project = Project::new(&fx);
    project.build_unwrapped(&fx);
    let generation = project.objects();
    let program = project.run();
    project.edit(0);
    project.build_unwrapped(&fx);
    if project.objects() == generation {
        return; // This cargo leaves nothing behind.
    }
    // The program runs the previous build, from a copy cargo replaced.
    let applied = fx.steward(&["gc", "--apply"]);
    assert_eq!(project.objects(), 2 * generation, "{applied}");

    drop(program);
    let applied = fx.steward(&["gc", "--apply"]);
    assert_eq!(project.objects(), generation, "{applied}");
}
