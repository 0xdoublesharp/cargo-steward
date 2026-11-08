//! Cargo's `rustc-wrapper` hook. Cargo runs `cargo-steward <rustc> <args>`
//! for every compile; this runs it (through the inner wrapper, such as
//! sccache), then:
//!
//! - holds one of `max-compiles` machine-wide slots while rustc runs
//!   ([`slots`]);
//! - removes the codegen-unit object files that this compile replaced
//!   (rust-lang/rust#161824, fixed in Cargo 1.101 for its own layout);
//! - records the compile in the ledger, for workspace members;
//! - records per-unit execution, queue time and direct-child resource costs;
//! - starts a background cleanup when one is due.
//!
//! Nothing here may fail or slow a build: every step is best effort, and
//! invocations that compile nothing (`rustc -vV`, `--print`) are handed
//! straight to rustc.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command, ExitStatus};
use std::time::{Duration, Instant, SystemTime};

use crate::config::{self, Config, Paths};
use crate::{costs, gc, ledger, scan};

mod slots;

/// Overrides `max-compiles`.
pub const MAX_COMPILES_VAR: &str = "CARGO_STEWARD_MAX_COMPILES";
pub const MEMORY_BUDGET_VAR: &str = "CARGO_STEWARD_MEMORY_BUDGET";

/// Objects modified within this long before the compile started are kept,
/// so that a file rewritten in place on a filesystem with coarse timestamps
/// is never mistaken for a stale one. Anything it keeps is cleaned up later.
const TIMESTAMP_SLACK: Duration = Duration::from_secs(2);

/// `object-grace` when the setting doesn't parse. A wrapper must never fail
/// a build over a typo; `gc` reports the bad setting.
const DEFAULT_OBJECT_GRACE: Duration = Duration::from_secs(600);

pub fn run(argv: Vec<OsString>) -> ! {
    let paths = Paths::resolve();
    let config = settings(paths.as_ref());

    let inner = inner_wrapper(&config);
    let direct = inner.is_none();
    let mut command: Vec<OsString> = inner.into_iter().collect();
    command.extend(argv.iter().cloned());

    let args = RustcArgs::parse(&argv[1..]);
    let Some(out_dir) = args.out_dir.clone() else {
        exec(&command);
    };
    let fix = if wants_leak_fix(&config, &args) {
        LeakFix::prepare(&args, &out_dir)
    } else {
        None
    };

    let cost_path = costs::path(&out_dir, args.extra_filename.as_deref());
    let queued = Instant::now();
    let memory_budget = memory_budget(&config);
    let slot = match (&paths, compile_limit(&config), memory_budget) {
        (Some(paths), limit, Some(budget)) => {
            let fallback = config::parse_size(&config.compile_memory_estimate)
                .ok()
                .filter(|&n| n > 0)
                .unwrap_or(2 << 30);
            let estimate = cost_path
                .as_deref()
                .and_then(costs::read)
                .and_then(|o| o.max_rss_bytes)
                .map(|n| n.saturating_add(n / 4))
                .unwrap_or(fallback);
            let count = limit.unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map_or(1, |n| n.get().try_into().unwrap_or(u32::MAX))
            });
            slots::acquire_memory(&paths.compile_slots(), count, budget, estimate, fallback)
        }
        (Some(paths), Some(limit), None) => slots::acquire(&paths.compile_slots(), limit),
        _ => None,
    };
    let queue_time = queued.elapsed();
    let started = Instant::now();
    let mut child = Command::new(&command[0]);
    child.args(&command[1..]);
    if let Some(slot) = &slot {
        slot.describe_child(&mut child);
    }
    let status = child.status();
    drop(slot);
    let status = match status {
        Ok(status) => status,
        Err(e) => {
            eprintln!(
                "error: cargo-steward: failed to run {}: {e}",
                command[0].to_string_lossy()
            );
            process::exit(127);
        }
    };
    if status.success() {
        if config.record_costs
            && let (Some(path), Some(name)) = (cost_path, args.crate_name.as_deref())
        {
            let (rss, cpu) = if direct {
                costs::child_usage()
            } else {
                (None, None)
            };
            costs::record(
                &path,
                name,
                &out_dir,
                started.elapsed(),
                queue_time,
                rss,
                cpu,
            );
        }
        if let Some(fix) = fix {
            fix.finish(object_grace(&config), config.process_guard);
        }
        if let Some(paths) = &paths {
            after_compile(paths, &config, &out_dir, started.elapsed());
        }
    }
    process::exit(exit_code(status));
}

/// The settings, each one that can't be used replaced by its default: a
/// typo in one must not drop the others (`inner-wrapper`, `auto-gc`), and a
/// wrapper must never fail a build over one. `doctor` and `gc` report them.
fn settings(paths: Option<&Paths>) -> Config {
    paths.map(|p| config::load_lenient(p).0).unwrap_or_default()
}

/// Whether to look for objects this compile replaces. Only incremental
/// compiles give their objects new names; others rewrite the same files in
/// place, so listing the output directory (all of `deps/` in the old layout)
/// beforehand would be wasted work.
fn wants_leak_fix(config: &Config, args: &RustcArgs) -> bool {
    config.leak_fix && args.incremental && args.writes_objects()
}

/// How many compiles may run at once: `$CARGO_STEWARD_MAX_COMPILES`, else
/// `max-compiles`. `None` for no limit.
pub fn compile_limit(config: &Config) -> Option<u32> {
    limit_from(
        env::var_os(MAX_COMPILES_VAR).as_deref(),
        config.max_compiles,
    )
}

pub fn memory_budget(config: &Config) -> Option<u64> {
    budget_from(
        env::var_os(MEMORY_BUDGET_VAR).as_deref(),
        config.compile_memory_budget.as_deref(),
    )
}

fn budget_from(var: Option<&OsStr>, setting: Option<&str>) -> Option<u64> {
    let setting = setting.and_then(|s| config::parse_size(s).ok());
    match var.and_then(OsStr::to_str).map(str::trim) {
        Some("") | Some("0") => None,
        Some(text) => config::parse_size(text).ok().or(setting).filter(|&n| n > 0),
        None => setting.filter(|&n| n > 0),
    }
}

/// The compile limit from the variable's value and the setting. A value
/// that isn't a number is ignored; an empty one or 0 means no limit.
fn limit_from(var: Option<&OsStr>, setting: Option<u32>) -> Option<u32> {
    let value = match var.map(|v| v.to_str().map(str::trim)) {
        Some(Some("")) => None,
        Some(Some(text)) => text.parse().ok().or(setting),
        Some(None) | None => setting,
    };
    value.filter(|&n| n > 0)
}

/// How long, at most, replaced objects are kept for a binary built from
/// them that is still running (`object-grace`).
fn object_grace(config: &Config) -> Duration {
    humantime::parse_duration(config.object_grace.trim()).unwrap_or(DEFAULT_OBJECT_GRACE)
}

/// The wrapper to run rustc through: `$CARGO_STEWARD_INNER` (empty for none),
/// else `inner-wrapper` from the settings. Skipped if it isn't installed, so a
/// missing sccache never breaks builds, and never ourselves.
fn inner_wrapper(config: &Config) -> Option<OsString> {
    let value: OsString = match env::var_os("CARGO_STEWARD_INNER") {
        Some(value) => value,
        None => config.inner_wrapper.clone()?.into(),
    };
    if value.is_empty() {
        return None;
    }
    let path = find_program(&value)?;
    let name = path.file_stem().and_then(OsStr::to_str).unwrap_or_default();
    (name != "cargo-steward").then(|| path.into_os_string())
}

/// Resolves a program name against `PATH` the way exec would.
pub fn find_program(program: &OsStr) -> Option<PathBuf> {
    let candidate = Path::new(program);
    if candidate.components().count() > 1 {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    env::split_paths(&env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
}

fn exec(command: &[OsString]) -> ! {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = Command::new(&command[0]).args(&command[1..]).exec();
        eprintln!(
            "error: cargo-steward: failed to run {}: {err}",
            command[0].to_string_lossy()
        );
        process::exit(127);
    }
    #[cfg(not(unix))]
    match Command::new(&command[0]).args(&command[1..]).status() {
        Ok(status) => process::exit(exit_code(status)),
        Err(err) => {
            eprintln!(
                "error: cargo-steward: failed to run {}: {err}",
                command[0].to_string_lossy()
            );
            process::exit(127);
        }
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

/// The parts of a rustc command line the wrapper needs.
#[derive(Debug, Default, PartialEq)]
pub struct RustcArgs {
    pub crate_name: Option<String>,
    pub extra_filename: Option<String>,
    pub out_dir: Option<PathBuf>,
    pub emit: Option<String>,
    pub save_temps: bool,
    /// `-C incremental=...` is set.
    pub incremental: bool,
}

impl RustcArgs {
    /// Parses rustc's arguments, expanding `@file` argument files, which
    /// cargo uses for long command lines.
    pub fn parse(args: &[OsString]) -> RustcArgs {
        let mut flat: Vec<String> = Vec::with_capacity(args.len());
        for arg in args {
            let Some(text) = arg.to_str() else {
                flat.push(String::new());
                continue;
            };
            if let Some(file) = text.strip_prefix('@')
                && let Ok(contents) = fs::read_to_string(file)
            {
                flat.extend(contents.lines().map(str::to_owned));
                continue;
            }
            flat.push(text.to_owned());
        }

        let mut parsed = RustcArgs::default();
        let mut iter = flat.iter();
        while let Some(arg) = iter.next() {
            let mut value = |flag: &str| -> Option<String> {
                if arg == flag {
                    iter.next().cloned()
                } else {
                    arg.strip_prefix(flag)
                        .and_then(|rest| rest.strip_prefix('='))
                        .map(str::to_owned)
                }
            };
            if let Some(v) = value("--crate-name") {
                parsed.crate_name = Some(v);
            } else if let Some(v) = value("--out-dir") {
                parsed.out_dir = Some(PathBuf::from(v));
            } else if let Some(v) = value("--emit") {
                parsed.emit = Some(v);
            } else if let Some(option) = codegen_option(arg, &mut iter) {
                let (key, val) = option.split_once('=').unwrap_or((&option, ""));
                match key {
                    "extra-filename" => parsed.extra_filename = Some(val.to_owned()),
                    "incremental" => parsed.incremental = !val.is_empty(),
                    "save-temps" => {
                        parsed.save_temps = !matches!(val, "no" | "n" | "off" | "false")
                    }
                    _ => {}
                }
            }
        }
        parsed
    }

    /// Whether the compile writes object files: it names a crate and emits
    /// `link` or `obj` (the default when `--emit` is absent), and isn't
    /// keeping temporaries on purpose.
    pub fn writes_objects(&self) -> bool {
        self.crate_name.is_some()
            && !self.save_temps
            && self.emit.as_deref().is_none_or(|emit| {
                emit.split(',')
                    .map(|kind| kind.split('=').next().unwrap_or(kind))
                    .any(|kind| kind == "link" || kind == "obj")
            })
    }
}

/// A `-C key=value` or `-Ckey=value` argument's `key=value`.
fn codegen_option<'a>(arg: &'a str, rest: &mut impl Iterator<Item = &'a String>) -> Option<String> {
    if arg == "-C" || arg == "--codegen" {
        return rest.next().cloned();
    }
    arg.strip_prefix("-C")
        .or_else(|| arg.strip_prefix("--codegen="))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The objects a unit had before its compile: any of them the compile didn't
/// rewrite belong to an earlier compile and are no longer used.
struct LeakFix {
    dir: PathBuf,
    before: Vec<OsString>,
    started: SystemTime,
    /// The unit's executable before the compile replaced it, which a
    /// program still running may have been started from (or from a copy).
    exe: gc::UnitExe,
}

impl LeakFix {
    fn prepare(args: &RustcArgs, out_dir: &Path) -> Option<LeakFix> {
        // Legacy-layout units share `deps/`, so match `<crate><-hash>.`; in
        // the new layout the directory is the unit's own.
        let unit = format!(
            "{}{}",
            args.crate_name.as_deref()?,
            args.extra_filename.as_deref().unwrap_or_default()
        );
        let prefix = format!("{unit}.");
        let before: Vec<OsString> = fs::read_dir(out_dir)
            .ok()?
            .flatten()
            .map(|e| e.file_name())
            .filter(|name| {
                name.to_str()
                    .is_some_and(|n| n.starts_with(&prefix) && scan::is_object(n))
            })
            .collect();
        if before.is_empty() {
            return None;
        }
        Some(LeakFix {
            dir: out_dir.to_path_buf(),
            before,
            started: SystemTime::now(),
            exe: gc::UnitExe::new(out_dir, &unit),
        })
    }

    /// Removes the objects the compile didn't write. Each compile writes its
    /// objects under new names (reused ones included), or rewrites them in
    /// place, so what keeps an old name and an old time is stale.
    ///
    /// Objects modified within `grace` are kept while a program started
    /// from the unit's executable runs: it may have been linked from them,
    /// and its backtraces need them. Running programs are looked for only
    /// when there are such objects; with `guard` off (`process-guard`), the
    /// objects are kept for the whole grace. A later compile of the unit, or
    /// cleanup, removes them once they're older.
    fn finish(mut self, grace: Duration, guard: bool) -> usize {
        let before_compile = self
            .started
            .checked_sub(TIMESTAMP_SLACK)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let before_grace = SystemTime::now()
            .checked_sub(grace)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut stale: Vec<PathBuf> = Vec::new();
        let mut recent: Vec<PathBuf> = Vec::new();
        for name in &self.before {
            let path = self.dir.join(name);
            let Ok(modified) = fs::symlink_metadata(&path).and_then(|m| m.modified()) else {
                continue;
            };
            if modified < before_compile.min(before_grace) {
                stale.push(path);
            } else if modified < before_compile {
                recent.push(path);
            }
        }
        if !recent.is_empty() && guard && !self.running() {
            stale.append(&mut recent);
        }
        stale.iter().filter(|p| fs::remove_file(p).is_ok()).count()
    }

    /// Whether a program started from the unit's executable (as it was
    /// before or after the compile, or a copy cargo uplifted) is running.
    /// When that can't be told, as though one were.
    fn running(&mut self) -> bool {
        self.exe.look();
        if !self.exe.exists() {
            return false;
        }
        let running = gc::Running::collect_for(&self.exe);
        !running.listed() || running.started_from(&self.exe).is_some()
    }
}

/// Records a workspace member's compile, and starts a cleanup if one is due.
fn after_compile(paths: &Paths, config: &Config, out_dir: &Path, took: Duration) {
    if env::var_os("CARGO_PRIMARY_PACKAGE").is_none() {
        return;
    }
    let Some(root) = scan::find_root(out_dir) else {
        return;
    };
    let workspace = env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from);
    // A few stat calls up to the worktree's `.git`: lets cleanup ask git
    // whether the worktree was removed, once it's gone.
    let linked = workspace
        .as_deref()
        .and_then(crate::proxy::linked_worktree)
        .map(|(admin, worktree)| ledger::Linked { admin, worktree });
    ledger::append(
        paths,
        &ledger::Record {
            ts: ledger::now_secs(),
            root,
            workspace,
            ms: took.as_millis().try_into().unwrap_or(u64::MAX),
            linked,
        },
    );
    if config.auto_gc && gc_due(paths, config, out_dir) {
        gc::spawn_background(paths);
    }
}

fn gc_due(paths: &Paths, config: &Config, out_dir: &Path) -> bool {
    let Ok(policy) = config.policy() else {
        return false;
    };
    let since = fs::metadata(paths.stamp())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok());
    match since {
        None => true,
        Some(age) if age >= policy.gc_interval => true,
        // Low free space starts one sooner: after 5 minutes, or longer while
        // cleanups have been freeing nothing.
        Some(age) => {
            age >= gc::LOW_SPACE_RECHECK
                && age >= gc::low_space_recheck(paths, &policy)
                && crate::fsutil::free_space(out_dir).is_some_and(|free| free < policy.min_free)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_the_flags_cargo_passes() {
        let parsed = RustcArgs::parse(&args(&[
            "--crate-name",
            "app",
            "--edition=2021",
            "src/main.rs",
            "--emit=dep-info,link",
            "-C",
            "extra-filename=-0123456789abcdef",
            "--out-dir",
            "/t/debug/deps",
            "-Cincremental=/t/debug/incremental",
        ]));
        assert_eq!(
            parsed,
            RustcArgs {
                crate_name: Some("app".into()),
                extra_filename: Some("-0123456789abcdef".into()),
                out_dir: Some("/t/debug/deps".into()),
                emit: Some("dep-info,link".into()),
                save_temps: false,
                incremental: true,
            }
        );
        assert!(parsed.writes_objects());
    }

    #[test]
    fn only_incremental_compiles_look_for_replaced_objects() {
        let config = Config::default();
        let incremental = RustcArgs::parse(&args(&[
            "--crate-name",
            "app",
            "-C",
            "incremental=/t/debug/incremental",
        ]));
        assert!(wants_leak_fix(&config, &incremental));
        // A registry dependency or a release build: objects keep their names.
        let plain = RustcArgs::parse(&args(&["--crate-name", "dep", "-C", "opt-level=3"]));
        assert!(!plain.incremental);
        assert!(!wants_leak_fix(&config, &plain));
        let off = Config {
            leak_fix: false,
            ..Config::default()
        };
        assert!(!wants_leak_fix(&off, &incremental));
    }

    #[test]
    fn a_bad_setting_doesnt_drop_the_inner_wrapper() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: tmp.path().to_path_buf(),
        };
        fs::write(
            paths.config(),
            "inner-wrapper = \"sccache\"\nauto-gc = true\nobject-grace = 10\n",
        )
        .unwrap();
        let config = settings(Some(&paths));
        assert_eq!(config.inner_wrapper.as_deref(), Some("sccache"));
        assert!(config.auto_gc);
        assert_eq!(object_grace(&config), DEFAULT_OBJECT_GRACE);
        assert_eq!(settings(None), Config::default());
    }

    #[test]
    fn reads_argument_files() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("args");
        fs::write(&file, "--crate-name\nlib\n--out-dir=/o\n-Csave-temps\n").unwrap();
        let parsed = RustcArgs::parse(&args(&[&format!("@{}", file.display())]));
        assert_eq!(parsed.crate_name.as_deref(), Some("lib"));
        assert_eq!(parsed.out_dir, Some(PathBuf::from("/o")));
        assert!(parsed.save_temps);
        assert!(!parsed.writes_objects());
    }

    #[test]
    fn skips_arguments_that_arent_utf8() {
        use std::os::unix::ffi::OsStringExt;
        let mut list = args(&["--crate-name"]);
        // Not UTF-8: holds the value's place, so it isn't taken as the name.
        list.push(OsString::from_vec(vec![0xff, 0xfe]));
        list.extend(args(&["--out-dir", "/o"]));
        let parsed = RustcArgs::parse(&list);
        assert_eq!(parsed.crate_name.as_deref(), Some(""));
        assert_eq!(parsed.out_dir, Some(PathBuf::from("/o")));
    }

    #[test]
    fn metadata_only_compiles_write_no_objects() {
        let check = RustcArgs::parse(&args(&["--crate-name", "a", "--emit=dep-info,metadata"]));
        assert!(!check.writes_objects());
        let probe = RustcArgs::parse(&args(&["-vV"]));
        assert!(!probe.writes_objects());
        assert_eq!(probe.out_dir, None);
    }

    /// A unit with objects replaced 5 minutes and an hour ago, compiled
    /// again, with `grace` and `guard`; returns the objects left.
    fn compile_after_replacements(executable: bool, grace: Duration, guard: bool) -> Vec<String> {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let now = SystemTime::now();
        let made = |name: &str, age: Duration| {
            let path = dir.join(name);
            fs::write(&path, "o").unwrap();
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(now - age)
                .unwrap();
        };
        made(
            "app-0123456789abcdef.x.0five.rcgu.o",
            Duration::from_secs(300),
        );
        made(
            "app-0123456789abcdef.x.0hour.rcgu.o",
            Duration::from_secs(3600),
        );
        if executable {
            made("app-0123456789abcdef", Duration::from_secs(300));
        }
        let parsed = RustcArgs::parse(&args(&[
            "--crate-name",
            "app",
            "-C",
            "extra-filename=-0123456789abcdef",
            "--out-dir",
            dir.to_str().unwrap(),
        ]));
        let fix = LeakFix::prepare(&parsed, dir).unwrap();
        fs::write(dir.join("app-0123456789abcdef.x.0new.rcgu.o"), "new").unwrap();
        fix.finish(grace, guard);
        let mut left: Vec<String> = crate::fsutil::entries(dir)
            .map(|(n, _)| n)
            .filter(|n| scan::is_object(n))
            .collect();
        left.sort();
        left
    }

    /// Set for a copy of this test binary started as [`sleeper`]'s program.
    const SLEEP_VAR: &str = "CARGO_STEWARD_TEST_WRAPPER_SLEEP";

    /// Does nothing in a normal run. Started with [`SLEEP_VAR`] set, it is a
    /// program that keeps running until killed.
    #[test]
    fn sleeper() {
        if env::var_os(SLEEP_VAR).is_some() {
            std::thread::sleep(Duration::from_secs(120));
        }
    }

    /// Kills the child when dropped.
    struct Child(std::process::Child);

    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Starts a copy of this test binary at `exe`, as a program run from a
    /// build directory.
    fn run_copy_at(exe: &Path) -> Child {
        fs::copy(env::current_exe().unwrap(), exe).unwrap();
        let mut command = Command::new(exe);
        command
            .args(["--exact", "wrapper::tests::sleeper", "-q"])
            .env(SLEEP_VAR, "1")
            .stdout(process::Stdio::null())
            .stderr(process::Stdio::null());
        // Linux won't run a program while any process has it open for
        // writing, and another test thread that forks just as this one
        // copies the program holds the copy open until its child execs.
        let mut spawned = command.spawn();
        for _ in 0..100 {
            match &spawned {
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(Duration::from_millis(20));
                    spawned = command.spawn();
                }
                _ => break,
            }
        }
        let mut child = Child(spawned.unwrap());
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "the copy didn't start"
        );
        child
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn keeps_replaced_objects_while_a_program_from_the_unit_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().canonicalize().unwrap();
        let deps = profile.join("deps");
        fs::create_dir_all(&deps).unwrap();
        let unit = "app-0123456789abcdef";
        let five_minutes_ago = SystemTime::now() - Duration::from_secs(300);
        let parsed = RustcArgs::parse(&args(&[
            "--crate-name",
            "app",
            "-C",
            "extra-filename=-0123456789abcdef",
            "--out-dir",
            deps.to_str().unwrap(),
        ]));
        let compile = |n: u32| {
            let fix = LeakFix::prepare(&parsed, &deps).unwrap();
            // The compile writes new objects and replaces the executable.
            fs::write(deps.join(format!("{unit}.x.0gen{n}.rcgu.o")), "new").unwrap();
            fs::remove_file(deps.join(unit)).ok();
            fs::write(deps.join(unit), format!("binary {n}")).unwrap();
            fix.finish(Duration::from_secs(600), true);
            let mut left: Vec<String> = crate::fsutil::entries(&deps)
                .map(|(n, _)| n)
                .filter(|n| scan::is_object(n))
                .collect();
            left.sort();
            left
        };
        let replaced = |n: u32| {
            let path = deps.join(format!("{unit}.x.0gen{n}.rcgu.o"));
            fs::write(&path, "o").unwrap();
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(five_minutes_ago)
                .unwrap();
        };

        // A test binary running from the output directory.
        replaced(0);
        let program = run_copy_at(&deps.join(unit));
        assert_eq!(
            compile(1),
            [
                format!("{unit}.x.0gen0.rcgu.o"),
                format!("{unit}.x.0gen1.rcgu.o")
            ]
        );
        drop(program);

        // A binary running from the copy cargo uplifted, which is of the
        // executable this compile replaces.
        for name in ["0gen0", "0gen1"] {
            fs::remove_file(deps.join(format!("{unit}.x.{name}.rcgu.o"))).unwrap();
        }
        replaced(2);
        fs::remove_file(deps.join(unit)).unwrap();
        let program = run_copy_at(&profile.join("app"));
        fs::copy(profile.join("app"), deps.join(unit)).unwrap();
        assert_eq!(
            compile(3),
            [
                format!("{unit}.x.0gen2.rcgu.o"),
                format!("{unit}.x.0gen3.rcgu.o")
            ]
        );
        drop(program);

        // Nothing runs any more: the replaced generation goes.
        fs::File::options()
            .write(true)
            .open(deps.join(format!("{unit}.x.0gen3.rcgu.o")))
            .unwrap()
            .set_modified(five_minutes_ago)
            .unwrap();
        assert_eq!(compile(4), [format!("{unit}.x.0gen4.rcgu.o")]);
    }

    #[test]
    fn keeps_replaced_objects_within_the_grace_only_for_running_programs() {
        let ten_minutes = Duration::from_secs(600);
        let newest = ["app-0123456789abcdef.x.0new.rcgu.o"];
        let with_recent = [
            "app-0123456789abcdef.x.0five.rcgu.o",
            "app-0123456789abcdef.x.0new.rcgu.o",
        ];
        // Nothing runs from the unit's executable (a library has none): only
        // the newest generation stays, however recent the others.
        assert_eq!(compile_after_replacements(false, ten_minutes, true), newest);
        assert_eq!(compile_after_replacements(true, ten_minutes, true), newest);
        // With the process guard off, the grace alone decides.
        assert_eq!(
            compile_after_replacements(true, ten_minutes, false),
            with_recent
        );
        assert_eq!(
            compile_after_replacements(false, Duration::ZERO, false),
            newest
        );
    }

    #[test]
    fn reads_the_compile_limit() {
        let var = |v: &str| Some(OsString::from(v));
        assert_eq!(limit_from(None, None), None);
        assert_eq!(limit_from(None, Some(4)), Some(4));
        assert_eq!(limit_from(None, Some(0)), None);
        assert_eq!(limit_from(var("2").as_deref(), Some(4)), Some(2));
        assert_eq!(limit_from(var(" 3 ").as_deref(), None), Some(3));
        // Turned off for one build.
        assert_eq!(limit_from(var("0").as_deref(), Some(4)), None);
        assert_eq!(limit_from(var("").as_deref(), Some(4)), None);
        // A typo never stops a build: the setting applies.
        assert_eq!(limit_from(var("four").as_deref(), Some(4)), Some(4));
        assert_eq!(limit_from(var("-1").as_deref(), None), None);
    }

    #[test]
    fn memory_budget_can_be_disabled_independently_of_the_count_limit() {
        assert_eq!(budget_from(None, Some("1GiB")), Some(1 << 30));
        assert_eq!(budget_from(Some(OsStr::new("0")), Some("1GiB")), None);
        assert_eq!(budget_from(Some(OsStr::new("")), Some("1GiB")), None);
        assert_eq!(
            budget_from(Some(OsStr::new("2GiB")), Some("1GiB")),
            Some(2 << 30)
        );
        assert_eq!(
            budget_from(Some(OsStr::new("bad")), Some("1GiB")),
            Some(1 << 30)
        );
    }

    #[test]
    fn reads_the_grace_setting() {
        let mut config = Config::default();
        assert_eq!(object_grace(&config), Duration::from_secs(600));
        config.object_grace = "0s".into();
        assert_eq!(object_grace(&config), Duration::ZERO);
        config.object_grace = "soon".into();
        assert_eq!(object_grace(&config), DEFAULT_OBJECT_GRACE);
    }

    #[test]
    fn removes_only_old_objects_of_the_compiled_unit() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let old = SystemTime::now() - Duration::from_secs(60);
        let set_old = |name: &str| {
            fs::File::options()
                .write(true)
                .open(dir.join(name))
                .unwrap()
                .set_modified(old)
                .unwrap();
        };
        for name in [
            "app-0123456789abcdef.x.0old.rcgu.o",
            "app-0123456789abcdef.app.12-cgu.0.rcgu.o",
            "app-fedcba9876543210.x.0old.rcgu.o",
            "libapp-0123456789abcdef.rlib",
        ] {
            fs::write(dir.join(name), "o").unwrap();
            set_old(name);
        }
        let parsed = RustcArgs::parse(&args(&[
            "--crate-name",
            "app",
            "-C",
            "extra-filename=-0123456789abcdef",
            "--out-dir",
            dir.to_str().unwrap(),
        ]));
        let fix = LeakFix::prepare(&parsed, dir).unwrap();
        // The compile rewrites one numbered object in place and adds new ones.
        fs::write(dir.join("app-0123456789abcdef.app.12-cgu.0.rcgu.o"), "new").unwrap();
        fs::write(dir.join("app-0123456789abcdef.x.0new.rcgu.o"), "new").unwrap();
        assert_eq!(fix.finish(Duration::ZERO, true), 1);

        let mut left: Vec<String> = crate::fsutil::entries(dir).map(|(n, _)| n).collect();
        left.sort();
        assert_eq!(
            left,
            [
                "app-0123456789abcdef.app.12-cgu.0.rcgu.o",
                "app-0123456789abcdef.x.0new.rcgu.o",
                "app-fedcba9876543210.x.0old.rcgu.o",
                "libapp-0123456789abcdef.rlib",
            ]
        );
    }

    #[test]
    fn finds_programs_by_path() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tool");
        fs::write(&tool, "").unwrap();
        assert_eq!(find_program(tool.as_os_str()), Some(tool.clone()));
        assert_eq!(find_program(tmp.path().join("missing").as_os_str()), None);
        // A directory isn't a program.
        assert_eq!(find_program(tmp.path().as_os_str()), None);
    }

    #[test]
    fn the_inner_wrapper_must_exist_and_not_be_us() {
        if env::var_os("CARGO_STEWARD_INNER").is_some() {
            return; // The environment decides; covered end to end.
        }
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        assert_eq!(inner_wrapper(&config), None);
        config.inner_wrapper = Some(String::new());
        assert_eq!(inner_wrapper(&config), None);
        // A missing sccache never breaks the build.
        let missing = tmp.path().join("sccache");
        config.inner_wrapper = Some(missing.display().to_string());
        assert_eq!(inner_wrapper(&config), None);
        fs::write(&missing, "").unwrap();
        assert_eq!(inner_wrapper(&config), Some(missing.into_os_string()));
        // Never ourselves: that would recurse.
        let us = tmp.path().join("cargo-steward");
        fs::write(&us, "").unwrap();
        config.inner_wrapper = Some(us.display().to_string());
        assert_eq!(inner_wrapper(&config), None);
    }

    #[test]
    fn exit_codes_carry_signals() {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(exit_code(ExitStatus::from_raw(3 << 8)), 3);
        // Killed by SIGKILL, as a shell reports it.
        assert_eq!(exit_code(ExitStatus::from_raw(9)), 137);
        // Stopped by SIGSTOP: neither an exit code nor a terminating signal.
        assert_eq!(exit_code(ExitStatus::from_raw(0x137f)), 1);
    }

    #[test]
    fn a_cleanup_is_due_after_the_interval_or_when_space_is_low() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: tmp.path().to_path_buf(),
        };
        let mut config = Config {
            gc_interval: "1h".into(),
            min_free: "0".into(),
            ..Config::default()
        };
        // Never cleaned up: due.
        assert!(gc_due(&paths, &config, tmp.path()));
        fs::write(paths.stamp(), "").unwrap();
        assert!(!gc_due(&paths, &config, tmp.path()));
        // Low on space, but cleaned up moments ago: not yet.
        config.min_free = "1000000T".into();
        config.target_free = "1000000T".into();
        assert!(!gc_due(&paths, &config, tmp.path()));
        let set_age = |age: Duration| {
            fs::File::options()
                .write(true)
                .open(paths.stamp())
                .unwrap()
                .set_modified(SystemTime::now() - age)
                .unwrap();
        };
        // Low on space and past the recheck: due.
        set_age(Duration::from_secs(400));
        assert!(gc_due(&paths, &config, tmp.path()));
        // Unless cleanups have been freeing nothing: then it waits longer.
        fs::write(paths.gc_backoff(), "1200\n").unwrap();
        assert!(!gc_due(&paths, &config, tmp.path()));
        set_age(Duration::from_secs(1300));
        assert!(gc_due(&paths, &config, tmp.path()));
        fs::remove_file(paths.gc_backoff()).unwrap();
        set_age(Duration::from_secs(400));
        config.min_free = "0".into();
        assert!(!gc_due(&paths, &config, tmp.path()));
        // Past the interval: due.
        set_age(Duration::from_secs(7200));
        assert!(gc_due(&paths, &config, tmp.path()));
        // A broken policy never starts one.
        config.gc_interval = "often".into();
        assert!(!gc_due(&paths, &config, tmp.path()));
    }
}
