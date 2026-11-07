//! Cargo's `rustc-wrapper` hook. Cargo runs `cargo-steward <rustc> <args>`
//! for every compile; this runs it (through the inner wrapper, such as
//! sccache), then:
//!
//! - removes the codegen-unit object files that this compile replaced
//!   (rust-lang/rust#161824, fixed in Cargo 1.101 for its own layout);
//! - records the compile in the ledger, for workspace members;
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
use crate::{gc, ledger, scan};

/// Objects modified within this long before the compile started are kept,
/// so that a file rewritten in place on a filesystem with coarse timestamps
/// is never mistaken for a stale one. Anything it keeps is cleaned up later.
const TIMESTAMP_SLACK: Duration = Duration::from_secs(2);

/// After this long since the last cleanup, low free space triggers another
/// even before `gc-interval` has passed.
const LOW_SPACE_RECHECK: Duration = Duration::from_secs(300);

pub fn run(argv: Vec<OsString>) -> ! {
    let paths = Paths::resolve();
    let config = paths
        .as_ref()
        .and_then(|p| config::load(p).ok())
        .unwrap_or_default();

    let mut command: Vec<OsString> = inner_wrapper(&config).into_iter().collect();
    command.extend(argv.iter().cloned());

    let args = RustcArgs::parse(&argv[1..]);
    let Some(out_dir) = args.out_dir.clone() else {
        exec(&command);
    };
    let fix = if config.leak_fix && args.writes_objects() {
        LeakFix::prepare(&args, &out_dir)
    } else {
        None
    };

    let started = Instant::now();
    let status = match Command::new(&command[0]).args(&command[1..]).status() {
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
        if let Some(fix) = fix {
            fix.finish();
        }
        if let Some(paths) = &paths {
            after_compile(paths, &config, &out_dir, started.elapsed());
        }
    }
    process::exit(exit_code(status));
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
}

impl LeakFix {
    fn prepare(args: &RustcArgs, out_dir: &Path) -> Option<LeakFix> {
        // Legacy-layout units share `deps/`, so match `<crate><-hash>.`; in
        // the new layout the directory is the unit's own.
        let prefix = format!(
            "{}{}.",
            args.crate_name.as_deref()?,
            args.extra_filename.as_deref().unwrap_or_default()
        );
        let before: Vec<OsString> = fs::read_dir(out_dir)
            .ok()?
            .flatten()
            .map(|e| e.file_name())
            .filter(|name| {
                name.to_str()
                    .is_some_and(|n| n.starts_with(&prefix) && scan::is_object(n))
            })
            .collect();
        (!before.is_empty()).then(|| LeakFix {
            dir: out_dir.to_path_buf(),
            before,
            started: SystemTime::now(),
        })
    }

    /// Removes the objects the compile didn't write. Each compile writes its
    /// objects under new names (reused ones included), or rewrites them in
    /// place, so what keeps an old name and an old time is stale.
    fn finish(self) -> usize {
        let cutoff = self
            .started
            .checked_sub(TIMESTAMP_SLACK)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut removed = 0;
        for name in &self.before {
            let path = self.dir.join(name);
            let stale = fs::symlink_metadata(&path)
                .and_then(|m| m.modified())
                .is_ok_and(|modified| modified < cutoff);
            if stale && fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        removed
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
    ledger::append(
        paths,
        &ledger::Record {
            ts: ledger::now_secs(),
            root,
            workspace: env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from),
            ms: took.as_millis().try_into().unwrap_or(u64::MAX),
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
        Some(age) => {
            age >= LOW_SPACE_RECHECK
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
            }
        );
        assert!(parsed.writes_objects());
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
    fn metadata_only_compiles_write_no_objects() {
        let check = RustcArgs::parse(&args(&["--crate-name", "a", "--emit=dep-info,metadata"]));
        assert!(!check.writes_objects());
        let probe = RustcArgs::parse(&args(&["-vV"]));
        assert!(!probe.writes_objects());
        assert_eq!(probe.out_dir, None);
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
        assert_eq!(fix.finish(), 1);

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
}
