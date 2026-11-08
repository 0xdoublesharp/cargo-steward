//! End to end: status, dedup, automatic cleanup, and removing a worktree's
//! build directory.

use std::fs::{self, File, FileTimes};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
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

/// Two build dirs holding the same dependency, a file big enough to dedup.
fn duplicate_pair(fx: &Fixture) -> (PathBuf, PathBuf) {
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let mut dirs = Vec::new();
    for name in ["a", "b"] {
        let dir = fx.build_dir(name, Duration::ZERO);
        fs::write(dir.join(".rustc_info.json"), "{}").unwrap();
        fs::write(dir.join("debug/deps/libdup-0123456789abcdef.rlib"), &data).unwrap();
        age(&dir, Duration::from_secs(3600));
        dirs.push(dir);
    }
    (dirs.remove(0), dirs.remove(0))
}

#[test]
fn status_with_nothing_found_says_where_dirs_come_from() {
    let fx = Fixture::new("");
    let out = fx.steward(&["status"]);
    assert!(out.contains("No build directories found yet."), "{out}");
    assert!(out.contains("config.toml"), "{out}");
}

#[test]
fn status_marks_pinned_and_orphaned_dirs() {
    let fx = Fixture::new("");
    let dir = fx.build_dir("target", Duration::ZERO);
    fs::write(dir.join(".cargo-steward-keep"), "").unwrap();
    fs::write(
        fx.home.join("registry.json"),
        format!(
            r#"{{"roots":{{{:?}:{{"last_used":1,"workspaces":[{:?}],"compile_ms":0}}}}}}"#,
            dir.display().to_string(),
            fx.root.join("deleted-workspace").display().to_string()
        ),
    )
    .unwrap();
    let out = fx.steward(&["status"]);
    assert!(out.contains("[pinned, workspaces deleted]"), "{out}");
    assert!(out.contains("legacy"), "{out}");
    assert!(out.contains("Run with --sizes"), "{out}");
}

#[test]
fn dedup_with_nothing_found_says_so() {
    let fx = Fixture::new("");
    let out = fx.steward(&["dedup"]);
    assert!(out.contains("No build directories found yet."), "{out}");
}

#[test]
fn dedup_previews_then_replaces_duplicates() {
    let fx = Fixture::new("");
    let (a, b) = duplicate_pair(&fx);
    let preview = fx.steward(&["dedup", "--verbose"]);
    if preview.contains("can't clone") {
        return; // The temp dir's filesystem can't clone files.
    }
    assert!(
        preview.contains("Would replace 2 duplicate files"),
        "{preview}"
    );
    assert!(
        preview.contains("libdup-0123456789abcdef.rlib"),
        "{preview}"
    );
    assert!(preview.contains("dedup --apply"), "{preview}");

    let applied = fx.steward(&["dedup", "--apply"]);
    assert!(applied.contains("Replaced 2 duplicate files"), "{applied}");
    assert!(applied.contains("Free space went"), "{applied}");
    let name = "debug/deps/libdup-0123456789abcdef.rlib";
    assert_eq!(
        fs::read(a.join(name)).unwrap(),
        fs::read(b.join(name)).unwrap()
    );

    // Already shared (APFS tells clones apart), or replaced: nothing left.
    let again = fx.steward(&["dedup"]);
    assert!(again.contains("No duplicate files to replace"), "{again}");
}

#[test]
fn gc_does_nothing_while_another_cleanup_runs() {
    let fx = Fixture::new("");
    let dir = fx.build_dir("target", 60 * DAY);
    let lock = File::create(fx.home.join("gc.lock")).unwrap();
    lock.lock().unwrap();
    let out = fx.steward(&["gc", "--apply"]);
    assert!(out.contains("another cleanup is already running"), "{out}");
    assert!(dir.exists());
    // --auto stays quiet about it.
    let auto = fx.steward(&["gc", "--auto"]);
    assert!(!auto.contains("another cleanup"), "{auto}");
    assert!(dir.exists());
}

#[test]
fn gc_auto_logs_timestamped_removals_and_the_space_freed() {
    let fx = Fixture::new("dedup = false\n");
    let dir = fx.build_dir("target", 60 * DAY);
    let out = fx.steward(&["gc", "--auto"]);
    assert!(!dir.exists(), "{out}");
    let removed = out.lines().find(|l| l.contains("Removed")).unwrap_or("");
    // Prefixed with an RFC 3339 time stamp.
    assert!(removed.starts_with("20") && removed.contains('T'), "{out}");
    assert!(out.contains("Freed"), "{out}");
    assert!(!out.contains("Total"), "{out}");
}

#[test]
fn remove_keeps_a_dir_a_build_is_using() {
    let fx = Fixture::new("");
    let dir = fx.build_dir("target", DAY);
    let lock = File::create(dir.join("debug/.cargo-lock")).unwrap();
    lock.lock().unwrap();
    let out = fx.steward(&["remove", "--apply", dir.to_str().unwrap()]);
    assert!(out.contains("a cargo build is using it"), "{out}");
    assert!(dir.exists());
}

#[test]
fn remove_worktree_finds_its_build_dir_with_cargo_metadata() {
    let fx = Fixture::new("");
    let worktree = fx.root.join("wt");
    fs::create_dir_all(worktree.join("src")).unwrap();
    fs::write(
        worktree.join("Cargo.toml"),
        "[package]\nname = \"wt\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(worktree.join("src/lib.rs"), "").unwrap();
    // Not built yet: there is nothing to remove.
    let out = fx.steward(&["remove", "--worktree", worktree.to_str().unwrap()]);
    assert!(out.contains("builds into"), "{out}");
    assert!(out.contains("Nothing to remove."), "{out}");

    let target = fx.build_dir("wt/target", DAY);
    let out = fx.steward(&[
        "remove",
        "--apply",
        "--worktree",
        worktree.to_str().unwrap(),
    ]);
    assert!(
        out.contains(&format!("builds into {}", target.display())),
        "{out}"
    );
    assert!(!target.exists(), "{out}");
}

#[test]
fn remove_worktree_fails_for_a_missing_or_non_cargo_dir() {
    let fx = Fixture::new("");
    let missing = fx.root.join("nope");
    let out = fx.steward_fails(&["remove", "--worktree", missing.to_str().unwrap()]);
    assert!(out.contains("doesn't exist"), "{out}");
    let plain = fx.root.join("plain");
    fs::create_dir_all(&plain).unwrap();
    let out = fx.steward_fails(&["remove", "--worktree", plain.to_str().unwrap()]);
    assert!(out.contains("cargo metadata"), "{out}");
}

#[cfg(target_os = "macos")]
#[test]
fn gc_apply_shares_duplicates_across_the_dirs_it_keeps() {
    let fx = Fixture::new("");
    let (a, b) = duplicate_pair(&fx);
    let out = fx.steward(&["gc", "--apply"]);
    assert!(
        out.contains("identical files now shared across build dirs (2 files)"),
        "{out}"
    );
    assert!(a.exists() && b.exists(), "{out}");
    assert!(fx.home.join("dedup.stamp").exists());

    // Nothing new: nothing to share. A new worktree's build of the same
    // dependency: shared with the copies already there.
    let out = fx.steward(&["gc", "--apply"]);
    assert!(!out.contains("identical files"), "{out}");
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let c = fx.build_dir("c", Duration::ZERO);
    fs::write(c.join("debug/deps/libdup-0123456789abcdef.rlib"), &data).unwrap();
    let out = fx.steward(&["gc", "--apply"]);
    assert!(
        out.contains("identical files now shared across build dirs (2 files)"),
        "{out}"
    );
}

#[test]
fn gc_verbose_counts_the_stale_object_files() {
    let fx = Fixture::new("");
    let root = fx.root.join("target");
    let unit = root.join("debug/build/app/0123456789abcdef");
    fs::create_dir_all(unit.join("fingerprint")).unwrap();
    fs::create_dir_all(unit.join("out")).unwrap();
    fs::write(
        root.join("CACHEDIR.TAG"),
        "Signature: x\n# created by cargo.\n",
    )
    .unwrap();
    fs::write(unit.join("fingerprint/bin-app"), "x").unwrap();
    let old = unit.join("out/app.x.0old.rcgu.o");
    fs::write(&old, "o").unwrap();
    age(&old, DAY);
    fs::write(unit.join("out/app.x.0new.rcgu.o"), "o").unwrap();
    let out = fx.steward(&["gc", "--verbose"]);
    assert!(out.contains("1 files, "), "{out}");
    assert!(old.exists(), "a dry run removes nothing");
}

#[test]
fn remove_worktree_follows_the_proxy_route_without_the_process_guard() {
    let fx = Fixture::new("process-guard = false\ntarget-root = \"ROUTED\"\n");
    let routed = fx.root.join("routed");
    let config = fx.home.join("config.toml");
    let settings = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        settings.replace("ROUTED", routed.to_str().unwrap()),
    )
    .unwrap();
    let worktree = fx.root.join("wt");
    fs::create_dir_all(&worktree).unwrap();
    let out = Command::new(BIN)
        .args([
            "remove",
            "--apply",
            "--worktree",
            worktree.to_str().unwrap(),
        ])
        .env("CARGO_STEWARD_HOME", &fx.home)
        .env_remove("CARGO")
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .output()
        .unwrap();
    let out = text(&out);
    // No cargo metadata needed: the route says where it builds.
    assert!(
        out.contains(&format!("builds into {}", routed.display())),
        "{out}"
    );

    // Nor does removing a build dir look for programs running from it.
    let dir = fx.build_dir("old", DAY);
    let out = fx.steward(&["remove", "--apply", dir.to_str().unwrap()]);
    assert!(!dir.exists(), "{out}");
}

/// Records `workspaces` as what built into `dir`.
fn record(fx: &Fixture, entries: &[(&Path, &[PathBuf])]) {
    let roots: Vec<String> = entries
        .iter()
        .map(|(dir, workspaces)| {
            let list: Vec<String> = workspaces
                .iter()
                .map(|w| format!("{:?}", w.display().to_string()))
                .collect();
            format!(
                "{:?}:{{\"last_used\":{},\"workspaces\":[{}],\"compile_ms\":0}}",
                dir.display().to_string(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
                list.join(",")
            )
        })
        .collect();
    fs::write(
        fx.home.join("registry.json"),
        format!("{{\"roots\":{{{}}}}}", roots.join(",")),
    )
    .unwrap();
}

#[test]
fn remove_orphaned_takes_every_dir_of_deleted_workspaces_now() {
    let fx = Fixture::new("process-guard = false\n");
    let settings = fs::read_to_string(fx.home.join("config.toml")).unwrap();
    fs::write(
        fx.home.join("config.toml"),
        format!(
            "{settings}target-root = {:?}\ntmp-root = {:?}\n",
            fx.root.join("cargo").display().to_string(),
            fx.root.join("tmp").display().to_string()
        ),
    )
    .unwrap();
    let orphan = fx.build_dir("cargo/repo-0123456789/gone", Duration::ZERO);
    let tmp = fx.root.join("tmp/repo-0123456789/gone");
    fs::create_dir_all(&tmp).unwrap();
    fs::write(tmp.join("scratch"), "x").unwrap();
    let pinned = fx.build_dir("pinned", Duration::ZERO);
    fs::write(pinned.join(".cargo-steward-keep"), "").unwrap();
    let live = fx.build_dir("live", Duration::ZERO);
    let unknown = fx.build_dir("unknown", Duration::ZERO);
    let deleted = [fx.root.join("deleted-worktree")];
    let existing = [fx.root.clone()];
    record(
        &fx,
        &[(&orphan, &deleted), (&pinned, &deleted), (&live, &existing)],
    );

    let preview = fx.steward(&["remove", "--orphaned"]);
    assert!(preview.contains(&orphan.display().to_string()), "{preview}");
    assert!(preview.contains(&tmp.display().to_string()), "{preview}");
    assert!(preview.contains("pinned (use --force"), "{preview}");
    assert!(
        preview.contains("cargo steward remove --orphaned --apply"),
        "{preview}"
    );
    assert!(orphan.exists() && tmp.exists());

    let out = fx.steward(&["remove", "--orphaned", "--apply"]);
    assert!(!orphan.exists() && !tmp.exists(), "{out}");
    assert!(
        pinned.exists() && live.exists() && unknown.exists(),
        "{out}"
    );
    let registry = fs::read_to_string(fx.home.join("registry.json")).unwrap();
    assert!(!registry.contains("gone"), "{registry}");

    let out = fx.steward(&["remove", "--orphaned", "--apply", "--force"]);
    assert!(!pinned.exists(), "{out}");
    let out = fx.steward(&["remove", "--orphaned"]);
    assert!(out.contains("Nothing to clean up."), "{out}");
}
