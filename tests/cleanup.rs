//! End to end: status, dedup, automatic cleanup, and removing a worktree's
//! build directory.

mod common;

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

    /// Runs cargo-steward with `--json` added, expecting success: its stdout
    /// parsed as exactly one JSON document, and its stderr.
    fn steward_json(&self, args: &[&str]) -> (serde_json::Value, String) {
        let out = Command::new(BIN)
            .args(args)
            .arg("--json")
            .env("CARGO_STEWARD_HOME", &self.home)
            .env_remove("CARGO")
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_BUILD_TARGET_DIR")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "cargo-steward {args:?} --json failed:\n{}",
            text(&out)
        );
        let doc = json_document(&out.stdout);
        (doc, String::from_utf8_lossy(&out.stderr).into_owned())
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

/// `stdout` parsed as one JSON document of format version 1: anything
/// before or after it fails.
fn json_document(stdout: &[u8]) -> serde_json::Value {
    let text = String::from_utf8_lossy(stdout);
    let doc: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("stdout isn't one JSON document ({e}):\n{text}"));
    assert_eq!(doc["version"], 1, "{doc}");
    doc
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
fn status_uses_records_until_full_discovery_is_requested() {
    let fx = Fixture::new("");
    let recorded = fx.build_dir("recorded", DAY);
    let unrecorded = fx.build_dir("unrecorded", DAY);
    fs::write(fx.home.join("registry.json"), serde_json::to_vec(&serde_json::json!({
        "roots": {recorded.display().to_string(): {"last_used": 1, "workspaces": [], "compile_ms": 0}}
    })).unwrap()).unwrap();
    let (quick, _) = fx.steward_json(&["status"]);
    assert_eq!(quick["build_dirs"].as_array().unwrap().len(), 1, "{quick}");
    assert_eq!(
        quick["build_dirs"][0]["path"],
        recorded.display().to_string()
    );
    assert_eq!(quick["build_dirs"][0]["last_used"], "1970-01-01T00:00:01Z");
    assert_eq!(quick["discovery_performed"], false);
    let (full, _) = fx.steward_json(&["status", "--discover"]);
    assert_eq!(full["build_dirs"].as_array().unwrap().len(), 2, "{full}");
    assert_eq!(full["discovery_performed"], true);
    assert!(
        full["build_dirs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["path"] == unrecorded.display().to_string())
    );
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

// ---- --json ----

#[test]
fn status_json_lists_each_dir_with_its_flags() {
    let fx = Fixture::new("");
    let (doc, _) = fx.steward_json(&["status"]);
    assert_eq!(doc["build_dirs"], serde_json::json!([]), "{doc}");

    let dir = fx.build_dir("target", 3 * DAY);
    fs::write(dir.join(".cargo-steward-keep"), "").unwrap();
    let deleted = fx.root.join("deleted-workspace");
    fs::write(
        fx.home.join("registry.json"),
        format!(
            r#"{{"roots":{{{:?}:{{"last_used":1,"workspaces":[{:?}],"compile_ms":0}}}}}}"#,
            dir.display().to_string(),
            deleted.display().to_string()
        ),
    )
    .unwrap();
    let (doc, _) = fx.steward_json(&["status", "--discover"]);
    assert_eq!(doc["sizes_measured"], false, "{doc}");
    assert_eq!(doc["total_bytes"], serde_json::Value::Null, "{doc}");
    let entry = &doc["build_dirs"][0];
    assert_eq!(entry["path"], dir.display().to_string(), "{doc}");
    assert_eq!(entry["layout"], "legacy", "{doc}");
    assert_eq!(entry["pinned"], true, "{doc}");
    assert_eq!(entry["orphaned"], true, "{doc}");
    assert_eq!(
        entry["workspaces"],
        serde_json::json!([deleted.display().to_string()])
    );
    assert_eq!(entry["size_bytes"], serde_json::Value::Null, "{doc}");
    let ago = entry["last_used_secs_ago"].as_u64().unwrap();
    assert!((3 * 86_400..4 * 86_400).contains(&ago), "{doc}");
    assert!(entry["last_used"].as_str().unwrap().ends_with('Z'), "{doc}");
    assert!(doc["disks"][0]["free_bytes"].as_u64().unwrap() > 0, "{doc}");
}

#[test]
fn status_json_with_sizes_measures_each_dir() {
    let fx = Fixture::new("");
    fx.build_dir("a", DAY);
    fx.build_dir("b", 2 * DAY);
    let (doc, _) = fx.steward_json(&["status", "--sizes", "--discover"]);
    assert_eq!(doc["sizes_measured"], true, "{doc}");
    let dirs = doc["build_dirs"].as_array().unwrap();
    // Most recently used first; each holds a 1 MiB file.
    assert!(dirs[0]["path"].as_str().unwrap().ends_with("/a"), "{doc}");
    let sizes: Vec<u64> = dirs
        .iter()
        .map(|d| d["size_bytes"].as_u64().unwrap())
        .collect();
    assert!(sizes.iter().all(|&s| s >= 1 << 20), "{doc}");
    assert_eq!(
        doc["total_bytes"].as_u64().unwrap(),
        sizes.iter().sum::<u64>()
    );
}

#[test]
fn json_stays_alone_on_stdout_when_there_are_warnings() {
    let fx = Fixture::new("unit-idle = \"soon\"\n");
    fx.build_dir("target", 60 * DAY);
    for args in [&["status"][..], &["status", "--sizes"], &["gc"], &["dedup"]] {
        let (_, stderr) = fx.steward_json(args);
        assert!(
            stderr.contains("warning: cargo-steward: settings"),
            "{stderr}"
        );
    }
}

#[test]
fn gc_json_lists_removals_kept_dirs_and_the_space_freed() {
    let fx = Fixture::new("dedup = false\n");
    let idle = fx.build_dir("idle", 60 * DAY);
    // A program running from a build dir keeps it.
    let busy = fx.build_dir("busy", 60 * DAY);
    // A copy of this test binary, kept running by `sleeper` (a copied system
    // binary such as `sleep` can be killed on start by code-signing policy).
    let program = busy.join("debug/sleeper");
    fs::copy(std::env::current_exe().unwrap(), &program).unwrap();
    age(&busy, 60 * DAY);
    let mut child = common::spawn_retrying(
        Command::new(&program)
            .args(["--exact", "sleeper", "--test-threads=1", "-q"])
            .env(SLEEP_VAR, "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(child.try_wait().unwrap().is_none(), "the copy didn't start");

    let (preview, _) = fx.steward_json(&["gc"]);
    let (applied, _) = fx.steward_json(&["gc", "--apply"]);
    let _ = child.kill();
    let _ = child.wait();

    assert_eq!(preview["command"], "gc", "{preview}");
    assert_eq!(preview["applied"], false, "{preview}");
    let action = &preview["actions"][0];
    assert_eq!(action["kind"], "idle", "{preview}");
    assert_eq!(action["description"], "idle build dirs", "{preview}");
    assert_eq!(action["path"], idle.display().to_string(), "{preview}");
    assert!(action["bytes"].as_u64().unwrap() >= 1 << 20, "{preview}");
    assert_eq!(preview["actions"].as_array().unwrap().len(), 1, "{preview}");
    assert_eq!(preview["totals"][0]["kind"], "idle", "{preview}");
    assert_eq!(preview["totals"][0]["count"], 1, "{preview}");
    assert_eq!(preview["total_bytes"], action["bytes"], "{preview}");
    let kept = &preview["skipped"][0];
    assert_eq!(kept["kind"], "idle", "{preview}");
    assert_eq!(kept["path"], busy.display().to_string(), "{preview}");
    assert!(
        kept["reason"].as_str().unwrap().contains("running from it"),
        "{preview}"
    );
    assert_eq!(preview["freed_on_disk"], serde_json::json!([]), "{preview}");

    assert_eq!(applied["applied"], true, "{applied}");
    assert_eq!(applied["busy"], false, "{applied}");
    assert_eq!(applied["actions"][0]["path"], idle.display().to_string());
    assert!(!idle.exists(), "{applied}");
    assert!(busy.exists(), "{applied}");
    let freed = applied["freed_on_disk"].as_array().unwrap();
    assert_eq!(freed.len(), 1, "{applied}");
    assert!(freed[0]["path"].is_string(), "{applied}");
    assert!(freed[0]["bytes"].is_i64(), "{applied}");
    assert_eq!(applied["shared"], serde_json::Value::Null, "{applied}");
}

#[test]
fn gc_json_says_when_another_cleanup_runs() {
    let fx = Fixture::new("");
    let dir = fx.build_dir("target", 60 * DAY);
    let lock = File::create(fx.home.join("gc.lock")).unwrap();
    lock.lock().unwrap();
    let (doc, stderr) = fx.steward_json(&["gc", "--apply"]);
    assert_eq!(doc["busy"], true, "{doc}");
    assert_eq!(doc["actions"], serde_json::json!([]), "{doc}");
    assert!(stderr.contains("another cleanup is already running"));
    assert!(dir.exists());
}

#[test]
fn remove_json_lists_removed_kept_and_refused_paths() {
    let fx = Fixture::new("");
    let old = fx.build_dir("old", DAY);
    let pinned = fx.build_dir("pinned", DAY);
    fs::write(pinned.join(".cargo-steward-keep"), "").unwrap();
    let plain = fx.root.join("plain");
    fs::create_dir_all(&plain).unwrap();
    let paths = [&old, &pinned, &plain].map(|p| p.to_str().unwrap().to_owned());
    let mut args = vec!["remove"];
    args.extend(paths.iter().map(String::as_str));
    let (doc, stderr) = fx.steward_json(&args);
    assert!(stderr.contains("refusing to remove"), "{stderr}");
    assert_eq!(doc["command"], "remove", "{doc}");
    assert_eq!(doc["applied"], false, "{doc}");
    assert_eq!(doc["actions"][0]["kind"], "requested", "{doc}");
    assert_eq!(doc["actions"][0]["path"], paths[0], "{doc}");
    assert_eq!(doc["skipped"][0]["path"], paths[1], "{doc}");
    assert!(
        doc["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .starts_with("pinned"),
        "{doc}"
    );
    assert_eq!(doc["refused"][0]["path"], paths[2], "{doc}");
    assert!(old.exists() && pinned.exists());

    args.insert(1, "--apply");
    let (doc, _) = fx.steward_json(&args);
    assert_eq!(doc["applied"], true, "{doc}");
    assert!(!old.exists() && pinned.exists(), "{doc}");
    // The change in free space is measured, as for `gc --apply`.
    let freed = doc["freed_on_disk"].as_array().unwrap();
    assert_eq!(freed.len(), 1, "{doc}");
    assert!(freed[0]["bytes"].is_i64(), "{doc}");
}

#[test]
fn remove_worktree_json_reports_where_it_builds() {
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
    let (doc, _) = fx.steward_json(&["remove", "--worktree", worktree.to_str().unwrap()]);
    let entry = &doc["worktrees"][0];
    assert_eq!(entry["worktree"], worktree.display().to_string(), "{doc}");
    let build_dir = entry["build_dir"].as_str().unwrap();
    assert!(build_dir.starts_with(routed.to_str().unwrap()), "{doc}");
    assert_eq!(entry["exists"], false, "{doc}");
    assert_eq!(doc["actions"], serde_json::json!([]), "{doc}");
}

#[test]
fn a_failure_with_json_prints_an_error_document() {
    let fx = Fixture::new("");
    let plain = fx.root.join("plain");
    fs::create_dir_all(&plain).unwrap();
    let out = Command::new(BIN)
        .args(["remove", "--json", plain.to_str().unwrap()])
        .env("CARGO_STEWARD_HOME", &fx.home)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let doc = json_document(&out.stdout);
    assert_eq!(doc["error"], "nothing to remove", "{doc}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("error: nothing to remove"));
}

#[test]
fn dedup_json_lists_each_group_of_identical_files() {
    let fx = Fixture::new("");
    let (doc, _) = fx.steward_json(&["dedup"]);
    assert_eq!(doc["build_dirs"], 0, "{doc}");
    assert_eq!(doc["files"], 0, "{doc}");

    let (a, b) = duplicate_pair(&fx);
    let (doc, _) = fx.steward_json(&["dedup"]);
    assert_eq!(doc["build_dirs"], 2, "{doc}");
    assert_eq!(doc["applied"], false, "{doc}");
    assert_eq!(doc["freed_on_disk_bytes"], serde_json::Value::Null, "{doc}");
    if doc["unsupported_volumes"] != 0 {
        return; // The temp dir's filesystem can't clone files.
    }
    // Both dirs hold the same rlib and the same 1 MiB file.
    assert_eq!(doc["files"], 2, "{doc}");
    let groups = doc["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "{doc}");
    let dup = groups
        .iter()
        .find(|g| {
            g["kept"]
                .as_str()
                .unwrap()
                .ends_with("libdup-0123456789abcdef.rlib")
        })
        .unwrap();
    assert_eq!(dup["size_bytes"], 200_000, "{doc}");
    let pair = [
        dup["kept"].as_str().unwrap(),
        dup["replaced"][0].as_str().unwrap(),
    ];
    let name = "debug/deps/libdup-0123456789abcdef.rlib";
    for path in [a.join(name), b.join(name)] {
        assert!(pair.contains(&path.to_str().unwrap()), "{doc}");
    }

    let (doc, _) = fx.steward_json(&["dedup", "--apply"]);
    assert_eq!(doc["applied"], true, "{doc}");
    assert_eq!(doc["files"], 2, "{doc}");
    assert!(doc["freed_on_disk_bytes"].is_i64(), "{doc}");
}

/// Set for a copy of this test binary started as [`sleeper`]'s program.
const SLEEP_VAR: &str = "CARGO_STEWARD_TEST_SLEEP";

/// Does nothing in a normal run. Started with [`SLEEP_VAR`] set, it is a
/// program that keeps running until killed.
#[test]
fn sleeper() {
    if std::env::var_os(SLEEP_VAR).is_some() {
        std::thread::sleep(Duration::from_secs(120));
    }
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

#[test]
fn gc_resets_low_space_backoff_when_no_disk_is_low() {
    let fx = Fixture::new("dedup = false\nmin-free = \"0B\"\ntarget-free = \"0B\"\n");
    let dir = fx.build_dir("target", Duration::ZERO);
    let backoff = fx.home.join("gc.backoff");
    // Doubling is covered with fixed observations in gc's unit tests;
    // free space on a shared volume can change during any CLI invocation.
    fs::write(&backoff, "1200\n").unwrap();
    fx.steward(&["gc", "--apply"]);
    assert!(dir.exists());
    assert!(!backoff.exists());
}

#[test]
fn stranded_trash_is_visible_without_an_original_build_directory() {
    let f = Fixture::new("");
    let original = f.build_dir("cargo/repo-0123456789/retired", Duration::ZERO);
    let trash = original.with_file_name(".steward-trash-retired-0-0");
    fs::rename(&original, &trash).unwrap();
    // Even the Cargo marker may have been deleted before interruption.
    fs::remove_file(trash.join("CACHEDIR.TAG")).unwrap();
    for args in [vec!["status"], vec!["status", "--discover"]] {
        let (doc, _) = f.steward_json(&args);
        assert!(
            doc["trash_dirs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["path"] == trash.to_str().unwrap()),
            "{doc}"
        );
    }
    let (preview, _) = f.steward_json(&["gc"]);
    assert!(
        preview["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["kind"] == "interrupted_trash" && a["path"] == trash.to_str().unwrap()),
        "{preview}"
    );
    assert!(trash.exists());
    let (applied, _) = f.steward_json(&["gc", "--apply"]);
    assert!(!trash.exists(), "{applied}");
    assert_eq!(
        applied["freed_on_disk"].as_array().unwrap().len(),
        1,
        "{applied}"
    );
}

#[test]
fn stranded_trash_keeps_live_owners_and_rejects_invalid_paths() {
    let f = Fixture::new("");
    let mut live = Command::new("sleep").arg("30").spawn().unwrap();
    let trash = f
        .root
        .join(format!(".steward-trash-retired-{}-0", live.id()));
    fs::create_dir_all(&trash).unwrap();
    let invalid = f.root.join(".steward-trash-unknown");
    fs::create_dir_all(&invalid).unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("keep"), "x").unwrap();
    std::os::unix::fs::symlink(outside.path(), f.root.join(".steward-trash-link-0-0")).unwrap();
    let (status, _) = f.steward_json(&["status"]);
    let (applied, _) = f.steward_json(&["gc", "--apply"]);
    let _ = live.kill();
    let _ = live.wait();
    assert!(
        status["trash_dirs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["path"] == trash.to_str().unwrap() && d["owner_running"] == true),
        "{status}"
    );
    assert!(trash.exists(), "{applied}");
    assert!(invalid.exists());
    assert!(outside.path().join("keep").exists());
    assert!(
        applied["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["path"] == trash.to_str().unwrap()),
        "{applied}"
    );
}

fn retirement_fixture() -> (Fixture, PathBuf, PathBuf, PathBuf) {
    let f = Fixture::new("");
    let config = f.home.join("config.toml");
    let settings = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        format!(
            "{settings}target-root = {:?}\ntmp-root = {:?}\n",
            f.root.join("cargo"),
            f.root.join("tmp")
        ),
    )
    .unwrap();
    let wt = f.root.join("wt");
    fs::create_dir_all(wt.join("src")).unwrap();
    fs::write(
        wt.join("Cargo.toml"),
        "[package]\nname = \"retired\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(wt.join("src/lib.rs"), "").unwrap();
    let (doc, _) = f.steward_json(&["remove", "--worktree", wt.to_str().unwrap()]);
    let build = PathBuf::from(doc["worktrees"][0]["build_dir"].as_str().unwrap());
    let tmp = f
        .root
        .join("tmp")
        .join(build.strip_prefix(f.root.join("cargo")).unwrap());
    fs::create_dir_all(&tmp).unwrap();
    fs::write(tmp.join("scratch"), "x").unwrap();
    (f, wt, build, tmp)
}

#[test]
fn retirement_retries_exact_tmp_when_the_build_is_already_gone() {
    let (f, wt, build, tmp) = retirement_fixture();
    let other = f.build_dir("cargo/other-0123456789/warm", Duration::ZERO);
    let other_tmp = f.root.join("tmp/other-0123456789/warm");
    fs::create_dir_all(&other_tmp).unwrap();
    let (preview, _) = f.steward_json(&["remove", "--worktree", wt.to_str().unwrap()]);
    assert!(
        preview["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["path"] == tmp.to_str().unwrap()),
        "{preview}"
    );
    assert!(!build.exists() && tmp.exists());
    let (doc, _) = f.steward_json(&["remove", "--worktree", wt.to_str().unwrap(), "--apply"]);
    assert!(!tmp.exists(), "{doc}");
    assert!(other.exists() && other_tmp.exists());
    assert_eq!(doc["freed_on_disk"].as_array().unwrap().len(), 1, "{doc}");
}

#[test]
fn retirement_keeps_locked_and_pinned_temporary_builds() {
    for pinned in [false, true] {
        let (f, wt, build, tmp) = retirement_fixture();
        f.build_dir(
            build.strip_prefix(&f.root).unwrap().to_str().unwrap(),
            Duration::ZERO,
        );
        let nested = f.build_dir(
            tmp.join("case/target")
                .strip_prefix(&f.root)
                .unwrap()
                .to_str()
                .unwrap(),
            Duration::ZERO,
        );
        let lock = File::open(nested.join("debug/.cargo-lock"))
            .unwrap_or_else(|_| File::create(nested.join("debug/.cargo-lock")).unwrap());
        if pinned {
            fs::write(nested.join(".cargo-steward-keep"), "").unwrap();
        } else {
            lock.lock().unwrap();
        }
        let (doc, _) = f.steward_json(&["remove", "--worktree", wt.to_str().unwrap(), "--apply"]);
        assert!(tmp.exists(), "{doc}");
        assert!(
            doc["skipped"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["path"] == tmp.to_str().unwrap()),
            "{doc}"
        );
    }
}

#[test]
fn retirement_ignores_another_checkouts_target_environment() {
    let (f, wt, build, tmp) = retirement_fixture();
    let out = Command::new(BIN)
        .args(["remove", "--worktree", wt.to_str().unwrap(), "--json"])
        .env("CARGO_STEWARD_HOME", &f.home)
        .env("CARGO_TARGET_DIR", f.root.join("foreign-target"))
        .env_remove("CARGO")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let doc = json_document(&out.stdout);
    assert_eq!(
        doc["worktrees"][0]["build_dir"],
        build.to_str().unwrap(),
        "{doc}"
    );
    assert!(tmp.exists());
}

#[test]
fn retirement_keeps_output_shared_with_another_worktree() {
    let (f, wt, build, tmp) = retirement_fixture();
    f.build_dir(
        build.strip_prefix(&f.root).unwrap().to_str().unwrap(),
        Duration::ZERO,
    );
    let other = f.root.join("another-worktree");
    fs::create_dir_all(&other).unwrap();
    record(&f, &[(&build, &[wt.clone(), other])]);
    let (doc, _) = f.steward_json(&["remove", "--worktree", wt.to_str().unwrap(), "--apply"]);
    assert!(build.exists() && tmp.exists(), "{doc}");
    assert!(
        doc["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["reason"].as_str().unwrap().contains("another worktree")),
        "{doc}"
    );
}

#[test]
fn retirement_keeps_a_program_running_from_its_tmp() {
    let (f, wt, build, tmp) = retirement_fixture();
    f.build_dir(
        build.strip_prefix(&f.root).unwrap().to_str().unwrap(),
        Duration::ZERO,
    );
    let program = tmp.join("sleeper");
    fs::copy(std::env::current_exe().unwrap(), &program).unwrap();
    let mut child = common::spawn_retrying(
        Command::new(&program)
            .args(["--exact", "sleeper", "--test-threads=1", "-q"])
            .env(SLEEP_VAR, "1")
            .stdout(std::process::Stdio::null()),
    );
    std::thread::sleep(Duration::from_millis(150));
    assert!(child.try_wait().unwrap().is_none());
    let (doc, _) = f.steward_json(&["remove", "--worktree", wt.to_str().unwrap(), "--apply"]);
    let _ = child.kill();
    let _ = child.wait();
    assert!(tmp.exists(), "{doc}");
    assert!(
        doc["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["path"] == tmp.to_str().unwrap()
                && a["reason"].as_str().unwrap().contains("running from it")),
        "{doc}"
    );
}

#[test]
fn automatic_gc_leaves_interrupted_trash_for_explicit_apply() {
    let f = Fixture::new("");
    let trash = f.root.join(".steward-trash-gone-0-0");
    fs::create_dir_all(&trash).unwrap();
    f.steward(&["gc", "--auto"]);
    assert!(trash.exists());
    f.steward(&["gc", "--apply"]);
    assert!(!trash.exists());
}

#[test]
fn trash_is_found_under_routed_tmp_roots_without_scan_roots() {
    let (f, _, _, tmp) = retirement_fixture();
    let config = f.home.join("config.toml");
    let settings = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        settings
            .lines()
            .filter(|l| !l.starts_with("scan-roots"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    let trash = tmp.with_file_name(".steward-trash-retired-0-0");
    fs::rename(&tmp, &trash).unwrap();
    let (doc, _) = f.steward_json(&["gc"]);
    assert!(
        doc["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["path"] == trash.to_str().unwrap()),
        "{doc}"
    );
    assert!(trash.exists());
}

#[test]
fn automatic_gc_cannot_remove_trash_through_its_build_parent() {
    let f = Fixture::new("");
    let build = f.build_dir("idle", 40 * DAY);
    let trash = build.join(".steward-trash-unit-0-0");
    fs::create_dir_all(&trash).unwrap();
    fs::write(trash.join("file"), vec![1u8; 4096]).unwrap();
    age(&build, 40 * DAY);
    f.steward(&["gc", "--auto"]);
    assert!(trash.exists());
    f.steward(&["gc", "--apply"]);
    assert!(!trash.exists() && !build.exists());
}

#[test]
fn temporary_parent_cannot_bypass_interrupted_trash_protection() {
    let (f, _, _, tmp) = retirement_fixture();
    fs::create_dir_all(f.root.join("cargo")).unwrap();
    let trash = tmp.join(".steward-trash-file-0-0");
    fs::create_dir_all(&trash).unwrap();
    age(&tmp, 3 * DAY);
    f.steward(&["gc", "--auto"]);
    assert!(trash.exists());
    f.steward(&["gc", "--apply"]);
    assert!(!trash.exists());
}
