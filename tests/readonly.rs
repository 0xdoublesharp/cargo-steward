//! Commands that only report work where the steward home can't be written,
//! as in an agent's sandbox.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_cargo-steward");

fn run(home: &Path, cargo_home: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("CARGO_STEWARD_HOME", home)
        .env("CARGO_HOME", cargo_home)
        .env_remove("CARGO")
        .env_remove("RUSTC_WRAPPER")
        .output()
        .unwrap()
}

#[test]
fn reports_run_with_a_read_only_home() {
    // SAFETY: plain syscall with no arguments.
    if unsafe { libc::geteuid() } == 0 {
        return; // Permissions don't stop root.
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let target = root.join("ws/target");
    fs::create_dir_all(target.join("debug/.fingerprint/a-0123456789abcdef")).unwrap();
    fs::write(target.join(".rustc_info.json"), "{}").unwrap();
    let home = root.join("steward");
    fs::create_dir_all(&home).unwrap();
    let record = format!(
        "{{\"ts\":1,\"root\":{:?},\"workspace\":{:?},\"ms\":5}}\n",
        target.display().to_string(),
        root.join("ws").display().to_string()
    );
    fs::write(home.join("ledger.jsonl"), &record).unwrap();
    let cargo_home = root.join("cargo-home");
    fs::create_dir_all(&cargo_home).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o555)).unwrap();

    let mut outputs = Vec::new();
    for args in [&["status"][..], &["gc"], &["dedup"], &["doctor"]] {
        outputs.push((args, run(&home, &cargo_home, args)));
    }
    fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();

    for (args, out) in &outputs {
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("Permission denied") && !stderr.contains("claiming the ledger"),
            "{args:?}: {stderr}"
        );
        assert!(!out.stdout.is_empty(), "{args:?} reported nothing");
        if args[0] != "doctor" {
            assert!(out.status.success(), "{args:?}: {stderr}");
        }
    }
    let status = String::from_utf8_lossy(&outputs[0].1.stdout);
    assert!(status.contains(&*target.to_string_lossy()), "{status}");
    // The ledger was read, not compacted.
    assert_eq!(
        fs::read_to_string(home.join("ledger.jsonl")).unwrap(),
        record
    );
}
