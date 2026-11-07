//! `cargo steward doctor` against a fake home: fake `rustup`, `sccache`,
//! `tmutil` and `mdutil` on PATH, and recorded projects in a temp dir.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_cargo-steward");
const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    cargo_home: PathBuf,
    steward: PathBuf,
    bin: PathBuf,
    env: Vec<(String, String)>,
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn script(path: &Path, body: &str) {
    write(path, &format!("#!/bin/sh\n{body}\n"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

impl Fixture {
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("home");
        let cargo_home = root.join("cargo");
        let steward = root.join("steward");
        let bin = root.join("bin");
        for d in [&home, &cargo_home, &steward, &bin] {
            fs::create_dir_all(d).unwrap();
        }
        Fixture {
            _tmp: tmp,
            root,
            home,
            cargo_home,
            steward,
            bin,
            env: Vec::new(),
        }
    }

    fn set(&mut self, key: &str, value: impl AsRef<Path>) {
        self.env
            .push((key.to_owned(), value.as_ref().display().to_string()));
    }

    fn doctor(&self) -> String {
        let mut command = Command::new(BIN);
        command.arg("doctor").env_clear();
        // Keep coverage instrumentation's output location, if any.
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        let out = command
            .env("HOME", &self.home)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_STEWARD_HOME", &self.steward)
            .env("RUSTUP_HOME", self.root.join("rustup"))
            .env("PATH", format!("{}:/bin:/usr/bin", self.bin.display()))
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "doctor failed: {stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        stdout
    }

    /// A recorded package that built into `build`.
    fn record(&self, packages: &[&Path], build: &Path) {
        let registry = serde_json::json!({
            "roots": {
                build.display().to_string(): {
                    "last_used": 1,
                    "workspaces": packages.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                    "compile_ms": 1,
                }
            }
        });
        write(&self.steward.join("registry.json"), &registry.to_string());
    }
}

fn build_dir(dir: &Path) {
    write(
        &dir.join("CACHEDIR.TAG"),
        "Signature: x\n# This file is a cache directory tag created by cargo.\n",
    );
    write(&dir.join(".rustc_info.json"), "{}");
    fs::create_dir_all(dir.join("debug/.fingerprint")).unwrap();
}

#[test]
fn bare_machine_reports_what_is_missing() {
    let fx = Fixture::new();
    // Nothing on PATH but the system's tools; hide the system's tmutil and
    // mdutil behind failing stubs so nothing real is asked.
    script(&fx.bin.join("tmutil"), "exit 1");
    script(&fx.bin.join("mdutil"), "exit 1");
    let out = fx.doctor();
    assert!(out.contains("No rustc-wrapper is set"), "{out}");
    assert!(out.contains("cargo-steward isn't on PATH"), "{out}");
    assert!(out.contains("No inner wrapper"), "{out}");
    assert!(out.contains("rustup isn't installed"), "{out}");
    assert!(out.contains("No builds recorded yet"), "{out}");
    assert!(out.contains("sccache isn't installed"), "{out}");
    assert!(out.contains("No VS Code or Cursor settings found"), "{out}");
    assert!(out.contains("No scan roots"), "{out}");
    assert!(out.contains("Automatic cleanup is off"), "{out}");
    assert!(out.contains("No cleanup has run yet"), "{out}");
}

#[test]
fn configured_machine_reports_each_finding() {
    let mut fx = Fixture::new();
    let root = fx.root.clone();

    // A workspace that pins a leaking toolchain, overrides the wrapper and
    // sccache dir, keeps full debug info and gives rust-analyzer its own
    // target dir.
    let project = root.join("projects/p");
    write(
        &project.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/a\"]\n",
    );
    write(
        &project.join("crates/a/Cargo.toml"),
        "[package]\nname = \"a\"\n",
    );
    write(
        &project.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.95.0\"\n",
    );
    write(
        &project.join(".cargo/config.toml"),
        "[build]\nrustc-wrapper = \"sccache\"\n[env]\nSCCACHE_DIR = \"/cache/p\"\n",
    );
    write(
        &project.join(".vscode/settings.json"),
        "{ // comment\n  \"rust-analyzer.cargo.targetDir\": true, }\n",
    );
    // A second project pinning a fixed toolchain, with reduced debug info.
    let other = root.join("projects/q");
    write(
        &other.join("Cargo.toml"),
        "[package]\nname = \"q\"\n[profile.dev]\ndebug = \"line-tables-only\"\n",
    );
    write(&other.join("rust-toolchain"), "1.101.0\n");
    let build = root.join("projects/p/target");
    build_dir(&build);
    fx.record(&[&project.join("crates/a"), &other], &build);

    let scan = root.join("scratch");
    build_dir(&scan.join("w/target"));
    write(
        &fx.steward.join("config.toml"),
        &format!(
            "scan-roots = [{:?}, {:?}]\ninner-wrapper = \"sccache\"\nauto-gc = true\nmin-free = \"1000TiB\"\ntarget-free = \"1000TiB\"\n",
            scan.display().to_string(),
            root.join("unmounted").display().to_string(),
        ),
    );
    write(&fx.steward.join("gc.stamp"), "");

    // An older cargo-steward is the configured wrapper.
    let wrapper = fx.bin.join("cargo-steward");
    script(&wrapper, "echo 'cargo steward 0.0.1'");
    write(
        &fx.cargo_home.join("config.toml"),
        &format!(
            "[build]\nrustc-wrapper = {:?}\n[env]\nSCCACHE_DIR = \"/cache/shared\"\n",
            wrapper.display().to_string()
        ),
    );

    script(
        &fx.bin.join("rustup"),
        r#"case "$1 $2" in
"toolchain list") printf 'stable-aarch64-apple-darwin (active, default)\n1.95.0-aarch64-apple-darwin\n1.80.0-aarch64-apple-darwin\nnightly-aarch64-apple-darwin\nmine\n' ;;
"override list") echo 'no overrides' ;;
"run stable-aarch64-apple-darwin") echo 'rustc 1.101.0 (aaaaaaaaa 2026-12-20)' ;;
"run 1.95.0-aarch64-apple-darwin") echo 'rustc 1.95.0 (59807616e 2026-04-14)' ;;
"run 1.80.0-aarch64-apple-darwin") echo 'rustc 1.80.0 (051478957 2024-07-21)' ;;
*) exit 1 ;;
esac"#,
    );
    let toolchains = root.join("rustup/toolchains");
    write(
        &toolchains.join("1.80.0-aarch64-apple-darwin/lib/x"),
        "12345",
    );
    fs::create_dir_all(toolchains.join("nightly-aarch64-apple-darwin")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&root, toolchains.join("mine")).unwrap();

    script(
        &fx.bin.join("sccache"),
        "printf 'Cache location                  Local disk: \"/cache/server\"\\n\
Cache size                           3 GiB\\nMax cache size                      10 GiB\\n'",
    );
    script(
        &fx.bin.join("tmutil"),
        r#"shift; for p in "$@"; do echo "[Included]  $p"; done"#,
    );
    script(&fx.bin.join("mdutil"), "echo 'Indexing enabled.'");

    fx.set("RUSTC_WRAPPER", "sccache");
    fx.set("CARGO_BUILD_RUSTC_WRAPPER", "/x/cargo-steward");
    fx.set("SCCACHE_DIR", "/cache/env");
    fx.set("SCCACHE_CACHE_SIZE", "20G");
    fx.set("XDG_CONFIG_HOME", root.join("xdg"));

    let out = fx.doctor();
    // Wrapper
    assert!(
        out.contains(&format!("is 0.0.1, but this binary is {VERSION}")),
        "{out}"
    );
    assert!(
        out.contains("Cargo's rustc-wrapper is cargo-steward"),
        "{out}"
    );
    assert!(out.contains("RUSTC_WRAPPER=sccache overrides"), "{out}");
    assert!(
        out.contains("CARGO_BUILD_RUSTC_WRAPPER points at cargo-steward"),
        "{out}"
    );
    assert!(out.contains("Inner wrapper sccache found at"), "{out}");
    // Toolchains
    assert!(
        out.contains(
            "1 of 5 installed toolchains leave stale object files behind (Rust 1.88 to 1.100):\n     1.95.0-aarch64-apple-darwin"
        ),
        "{out}"
    );
    assert!(
        out.contains("Not affected: stable-aarch64-apple-darwin, 1.80.0-aarch64-apple-darwin"),
        "{out}"
    );
    assert!(out.contains("3 of 5 installed toolchains"), "{out}");
    assert!(out.contains("linked"), "{out}");
    assert!(
        out.contains(
            "rustup toolchain uninstall 1.80.0-aarch64-apple-darwin nightly-aarch64-apple-darwin\n"
        ),
        "{out}"
    );
    // Projects
    assert!(
        out.contains("1 project(s) pin 1.95.0 (leaks without cargo-steward"),
        "{out}"
    );
    assert!(out.contains("1 project(s) pin 1.101.0:"), "{out}");
    assert!(
        out.contains(&format!(
            "{} sets build.rustc-wrapper",
            project.join(".cargo/config.toml").display()
        )),
        "{out}"
    );
    assert!(
        out.contains("1 of 2 recorded workspaces build with full debug info"),
        "{out}"
    );
    // Compiler cache
    assert!(out.contains("3 GiB used of 10 GiB"), "{out}");
    assert!(out.contains("SCCACHE_CACHE_SIZE here: 20G"), "{out}");
    assert!(
        out.contains(
            "SCCACHE_DIR here is /cache/env, but the running sccache server uses /cache/server"
        ),
        "{out}"
    );
    assert!(
        out.contains("2 sccache directories are in use") && out.contains("such as /cache/env"),
        "{out}"
    );
    assert!(out.contains("about 20.0 GiB"), "{out}");
    // Editors
    assert!(
        out.contains("rust-analyzer.cargo.targetDir = true"),
        "{out}"
    );
    // Disks
    assert!(
        out.contains("doesn't exist (is the volume mounted?)"),
        "{out}"
    );
    assert!(
        out.contains("!! ") && out.contains("free (cleanup frees space below 1000TiB)"),
        "{out}"
    );
    if cfg!(target_os = "macos") {
        assert!(out.contains("is backed up by Time Machine"), "{out}");
        assert!(out.contains("Spotlight indexes"), "{out}");
        assert!(out.contains("Time Machine backs up 2 of them"), "{out}");
    }
    // Cleanup
    assert!(
        out.contains("Automatic cleanup is on, at most every 1h"),
        "{out}"
    );
    assert!(out.contains("Last cleanup started"), "{out}");
}

#[test]
fn healthy_settings_are_reported_ok() {
    let mut fx = Fixture::new();
    let root = fx.root.clone();
    let project = root.join("p");
    write(
        &project.join("Cargo.toml"),
        "[package]\nname = \"p\"\n[profile.dev]\ndebug = 1\n",
    );
    write(&project.join(".vscode/settings.json"), "{}");
    let build = project.join("target");
    build_dir(&build);
    fx.record(&[&project], &build);
    let scan = root.join("scratch");
    fs::create_dir_all(&scan).unwrap();
    write(
        &fx.steward.join("config.toml"),
        &format!(
            "scan-roots = [{:?}]\ninner-wrapper = \"not-installed-wrapper\"\nmin-free = \"0B\"\n",
            scan.display().to_string()
        ),
    );
    // The wrapper is this binary.
    let link = fx.bin.join("cargo-steward");
    #[cfg(unix)]
    std::os::unix::fs::symlink(BIN, &link).unwrap();
    write(
        &fx.cargo_home.join("config.toml"),
        &format!(
            "[build]\nrustc-wrapper = {:?}\n",
            link.display().to_string()
        ),
    );
    script(
        &fx.bin.join("rustup"),
        r#"case "$1 $2" in
"toolchain list") echo 'stable-aarch64-apple-darwin (active, default)' ;;
"override list") echo 'no overrides' ;;
*) exit 1 ;;
esac"#,
    );
    // sccache that can't report stats.
    script(&fx.bin.join("sccache"), "exit 1");
    script(
        &fx.bin.join("tmutil"),
        r#"shift; for p in "$@"; do echo "[Excluded]  $p"; done"#,
    );
    script(&fx.bin.join("mdutil"), "echo 'Indexing disabled.'");
    fx.set("CARGO_PROFILE_DEV_DEBUG", "full");

    let out = fx.doctor();
    assert!(out.contains("The wrapper is this binary"), "{out}");
    assert!(
        out.contains("Inner wrapper not-installed-wrapper isn't installed"),
        "{out}"
    );
    assert!(
        out.contains("Every installed toolchain is the default"),
        "{out}"
    );
    assert!(
        out.contains("1 recorded project(s) build through cargo-steward"),
        "{out}"
    );
    assert!(
        out.contains("Every recorded workspace reduces debug info"),
        "{out}"
    );
    assert!(
        out.contains("`sccache --show-stats` failed or timed out"),
        "{out}"
    );
    assert!(
        out.contains("rust-analyzer shares each project's target directory"),
        "{out}"
    );
    assert!(
        out.contains("  ok ") && out.contains("free (cleanup frees space below 0B)"),
        "{out}"
    );
    if cfg!(target_os = "macos") {
        assert!(out.contains("is excluded from Time Machine"), "{out}");
        assert!(out.contains("Spotlight doesn't index"), "{out}");
        assert!(out.contains("Time Machine excludes all of them"), "{out}");
    }
}

#[test]
fn reports_broken_and_foreign_wrappers() {
    let fx = Fixture::new();
    script(&fx.bin.join("tmutil"), "exit 1");
    script(&fx.bin.join("mdutil"), "exit 1");
    let cases = [
        (
            "/nonexistent/cargo-steward".to_owned(),
            "/nonexistent/cargo-steward doesn't exist",
        ),
        (
            "sccache".to_owned(),
            "Cargo's rustc-wrapper is sccache, so builds bypass",
        ),
    ];
    for (wrapper, expected) in cases {
        write(
            &fx.cargo_home.join("config.toml"),
            &format!("[build]\nrustc-wrapper = {wrapper:?}\n"),
        );
        let out = fx.doctor();
        assert!(out.contains(expected), "{out}");
    }
    // Not executable.
    let plain = fx.root.join("plain/cargo-steward");
    write(&plain, "");
    write(
        &fx.cargo_home.join("config.toml"),
        &format!(
            "[build]\nrustc-wrapper = {:?}\n",
            plain.display().to_string()
        ),
    );
    let out = fx.doctor();
    assert!(
        out.contains("isn't executable, so every build fails"),
        "{out}"
    );
    // Same version.
    let same = fx.root.join("same/cargo-steward");
    script(&same, &format!("echo 'cargo steward {VERSION}'"));
    write(
        &fx.cargo_home.join("config.toml"),
        &format!(
            "[build]\nrustc-wrapper = {:?}\n",
            same.display().to_string()
        ),
    );
    let out = fx.doctor();
    assert!(
        out.contains(&format!("is {VERSION}, the same as this one")),
        "{out}"
    );
}

#[test]
fn reduced_debug_info_everywhere_is_ok() {
    let mut fx = Fixture::new();
    let project = fx.root.join("p");
    write(&project.join("Cargo.toml"), "[package]\nname = \"p\"\n");
    let build = project.join("target");
    build_dir(&build);
    fx.record(&[&project], &build);
    script(&fx.bin.join("tmutil"), "exit 1");
    script(&fx.bin.join("mdutil"), "exit 1");
    write(
        &fx.cargo_home.join("config.toml"),
        "[profile.dev]\ndebug = false\n",
    );
    let out = fx.doctor();
    assert!(
        out.contains("Debug info in dev builds is reduced for every project"),
        "{out}"
    );
    // The environment outranks the user config.
    fx.set("CARGO_PROFILE_DEV_DEBUG", "true");
    let out = fx.doctor();
    assert!(
        out.contains("1 of 1 recorded workspaces build with full debug info"),
        "{out}"
    );
}

#[test]
fn a_sparse_setup_is_reported_without_guessing() {
    let mut fx = Fixture::new();
    let root = fx.root.clone();
    // HOME doesn't exist, so no device to compare build directories with.
    fx.set("HOME", root.join("no-such-home"));
    // CARGO_HOME is `<w>/.cargo`: its config sits where a project under `w`
    // would have its own, and isn't taken for one.
    let cargo_home = root.join("w/.cargo");
    fx.set("CARGO_HOME", &cargo_home);
    write(
        &cargo_home.join("config.toml"),
        "[build]\nrustc-wrapper = \"/x/cargo-steward\"\n",
    );
    // Nine workspaces keeping full debug info; one with its own sccache dir.
    let mut projects = Vec::new();
    for i in 0..9 {
        let project = root.join(format!("w/p{i}"));
        write(
            &project.join("Cargo.toml"),
            &format!("[package]\nname = \"p{i}\"\n"),
        );
        projects.push(project);
    }
    write(
        &projects[0].join(".cargo/config.toml"),
        "[env]\nSCCACHE_DIR = \"/cache/p\"\n",
    );
    let build = root.join("w/p0/target");
    build_dir(&build);
    fx.record(
        &projects.iter().map(PathBuf::as_path).collect::<Vec<_>>(),
        &build,
    );
    // sccache that doesn't report its maximum size.
    script(
        &fx.bin.join("sccache"),
        "printf 'Cache location                  Local disk: \"/cache/server\"\\n'",
    );
    fx.set("SCCACHE_DIR", "/cache/env");
    // A scan root that tmutil and mdutil can't say anything about.
    let scan = root.join("scratch");
    fs::create_dir_all(&scan).unwrap();
    write(
        &fx.steward.join("config.toml"),
        &format!("scan-roots = [{:?}]\n", scan.display().to_string()),
    );
    script(&fx.bin.join("tmutil"), "exit 1");
    script(&fx.bin.join("mdutil"), "exit 1");

    let out = fx.doctor();
    assert!(!out.contains("sets build.rustc-wrapper"), "{out}");
    assert!(
        out.contains("9 recorded project(s) build through cargo-steward"),
        "{out}"
    );
    assert!(
        out.contains("9 of 9 recorded workspaces build with full debug info")
            && out.contains("and 1 more"),
        "{out}"
    );
    assert!(out.contains("sccache directories are in use"), "{out}");
    assert!(out.contains("a larger size"), "{out}");
    let scan_text = scan.display().to_string();
    assert!(
        !out.lines()
            .any(|l| l.contains(&scan_text)
                && (l.contains("Time Machine") || l.contains("Spotlight"))),
        "{out}"
    );
}

#[test]
fn toolchains_are_summed_before_any_project_has_built() {
    let fx = Fixture::new();
    script(&fx.bin.join("tmutil"), "exit 1");
    script(&fx.bin.join("mdutil"), "exit 1");
    script(
        &fx.bin.join("rustup"),
        r#"case "$1 $2" in
"toolchain list") printf 'stable-aarch64-apple-darwin (active, default)\nnightly-aarch64-apple-darwin\n' ;;
"override list") echo 'no overrides' ;;
"run stable-aarch64-apple-darwin") echo 'rustc 1.101.0 (aaaaaaaaa 2026-12-20)' ;;
*) exit 1 ;;
esac"#,
    );
    let toolchains = fx.root.join("rustup/toolchains");
    write(
        &toolchains.join("stable-aarch64-apple-darwin/lib/x"),
        "12345",
    );
    write(
        &toolchains.join("nightly-aarch64-apple-darwin/lib/x"),
        "12345",
    );
    let out = fx.doctor();
    assert!(out.contains("2 toolchains take"), "{out}");
    assert!(
        out.contains("ones no project uses are reported once projects"),
        "{out}"
    );
}

#[test]
fn reports_settings_it_cant_use_and_keeps_the_rest() {
    let fx = Fixture::new();
    script(&fx.bin.join("tmutil"), "exit 1");
    script(&fx.bin.join("mdutil"), "exit 1");
    write(
        &fx.steward.join("config.toml"),
        "auto-gc = true\nmax_builds = 3\ndir-idle = \"soon\"\n",
    );
    let out = fx.doctor();
    assert!(
        out.contains("max_builds: not a cargo-steward setting"),
        "{out}"
    );
    assert!(out.contains("dir-idle:"), "{out}");
    // The good one still applies.
    assert!(out.contains("Automatic cleanup is on"), "{out}");
}
