//! Execute the Action's install step against local release archives.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

use sha2::{Digest, Sha256};

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

struct Fixture {
    tmp: tempfile::TempDir,
    bin: PathBuf,
    archive: PathBuf,
    version: String,
}

fn script(path: impl AsRef<std::path::Path>, body: &str) {
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        fs::create_dir_all(root.join("cargo/bin")).unwrap();
        fs::create_dir(root.join("unpacked")).unwrap();
        let version = env!("CARGO_PKG_VERSION").to_owned();
        script(
            root.join("unpacked/cargo-steward"),
            &format!("echo cargo-steward {version}"),
        );
        // Exercise all four published target names regardless of the test host.
        let archive = root.join("release.tar.gz");
        assert!(
            Command::new("tar")
                .args(["-czf"])
                .arg(&archive)
                .arg("-C")
                .arg(root.join("unpacked"))
                .arg("cargo-steward")
                .status()
                .unwrap()
                .success()
        );
        let digest = sha256(&fs::read(&archive).unwrap());
        for target in [
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
        ] {
            let dir = root.join(format!("releases/download/v{version}"));
            fs::create_dir_all(&dir).unwrap();
            let name = format!("cargo-steward-{target}.tar.gz");
            fs::copy(&archive, dir.join(&name)).unwrap();
            fs::write(
                dir.join(format!("{name}.sha256")),
                format!("{digest}  {name}\n"),
            )
            .unwrap();
        }
        script(
            bin.join("cargo"),
            "printf '%s\\n' \"$*\" >> \"$CARGO_LOG\"\nexit 99",
        );
        Self {
            tmp,
            bin,
            archive,
            version,
        }
    }

    fn command(&self) -> Command {
        let action = include_str!("../action.yml");
        let step = action.split("    - name: cargo steward ci").next().unwrap();
        let body = step.split_once("      run: |\n").unwrap().1;
        let body = body
            .lines()
            .map(|line| line.strip_prefix("        ").unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n");
        self.command_with_body(&body)
    }

    fn command_with_body(&self, body: &str) -> Command {
        let root = self.tmp.path();
        let mut cmd = Command::new("bash");
        cmd.args(["-c", body])
            .current_dir(root)
            .env(
                "PATH",
                format!(
                    "{}:{}:/usr/bin:/bin",
                    self.bin.display(),
                    root.join("cargo/bin").display()
                ),
            )
            .env("HOME", root)
            .env("CARGO_HOME", root.join("cargo"))
            .env_remove("CARGO_INSTALL_ROOT")
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env("CARGO_LOG", root.join("cargo.log"))
            .env("CARGO_TARGET_DIR", root.join("build"))
            .env("GITHUB_ACTION_PATH", env!("CARGO_MANIFEST_DIR"))
            .env("GITHUB_PATH", root.join("github-path"))
            .env("STEWARD_VERSION", "")
            .env("STEWARD_FROM_SOURCE", "false")
            .env(
                "CARGO_STEWARD_RELEASE_URL",
                format!("file://{}/releases", root.display()),
            )
            .env("TMPDIR", root);
        cmd
    }

    fn assert_ok(&self, out: &Output) {
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn action_downloads_verified_release_without_compiling_and_reuses_it() {
    let fx = Fixture::new();
    fx.assert_ok(&fx.command().output().unwrap());
    assert!(
        !fx.tmp.path().join("cargo.log").exists(),
        "must not compile"
    );
    assert_eq!(
        fs::read(fx.tmp.path().join("cargo/bin/cargo-steward")).unwrap(),
        fs::read(fx.tmp.path().join("unpacked/cargo-steward")).unwrap()
    );
    assert!(
        fs::read_to_string(fx.tmp.path().join("github-path"))
            .unwrap()
            .contains("cargo/bin")
    );
    // A restored binary works even when the release server is unavailable.
    fx.assert_ok(
        &fx.command()
            .env("CARGO_STEWARD_RELEASE_URL", "file:///missing")
            .output()
            .unwrap(),
    );
}

#[test]
fn action_replaces_a_stale_binary_and_accepts_a_version_prefix() {
    let fx = Fixture::new();
    script(
        fx.tmp.path().join("cargo/bin/cargo-steward"),
        "echo cargo-steward 0.0.0",
    );
    fx.assert_ok(
        &fx.command()
            .env("STEWARD_VERSION", format!("v{}", fx.version))
            .output()
            .unwrap(),
    );
    assert!(!fx.tmp.path().join("cargo.log").exists());
}

#[test]
fn action_rejects_bad_checksums_without_compiling() {
    let fx = Fixture::new();
    let dir = fx
        .tmp
        .path()
        .join(format!("releases/download/v{}", fx.version));
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "sha256") {
            fs::write(path, "bad-checksum\n").unwrap();
        }
    }
    let out = fx.command().output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("checksum mismatch"));
    assert!(!fx.tmp.path().join("cargo.log").exists());
    assert!(!fx.tmp.path().join("cargo/bin/cargo-steward").exists());
}

#[test]
fn action_uses_authenticated_asset_api_and_retries_a_connection_failure() {
    let fx = Fixture::new();
    script(fx.bin.join("curl"), "exit 88");
    script(fx.bin.join("sleep"), ":");
    script(
        fx.bin.join("gh"),
        r#"
printf '%s\n' "$*" >> "$HOME/gh.log"
if [ ! -f "$HOME/attempted" ]; then
  touch "$HOME/attempted"
  exit 56
fi
previous=
destination=
for arg do
  if [ "$previous" = --dir ]; then destination=$arg; fi
  previous=$arg
done
cp "$RELEASE_FIXTURE"/* "$destination/"
"#,
    );
    let out = fx
        .command()
        .env_remove("CARGO_STEWARD_RELEASE_URL")
        .env("GH_TOKEN", "fixture-token")
        .env(
            "RELEASE_FIXTURE",
            fx.tmp
                .path()
                .join(format!("releases/download/v{}", fx.version)),
        )
        .output()
        .unwrap();
    fx.assert_ok(&out);
    let log = fs::read_to_string(fx.tmp.path().join("gh.log")).unwrap();
    assert_eq!(log.lines().count(), 2, "{log}");
    assert!(
        log.contains(&format!(
            "release download v{} --repo 0xdoublesharp/cargo-steward",
            fx.version
        )),
        "{log}"
    );
    assert!(log.contains(".tar.gz.sha256"), "{log}");
    assert!(!fx.tmp.path().join("cargo.log").exists());
}

#[test]
fn action_stops_after_repeated_asset_api_failures_without_compiling() {
    let fx = Fixture::new();
    script(fx.bin.join("curl"), "exit 88");
    script(fx.bin.join("sleep"), ":");
    script(
        fx.bin.join("gh"),
        "echo attempt >> \"$HOME/gh.log\"\nexit 56",
    );
    let out = fx
        .command()
        .env_remove("CARGO_STEWARD_RELEASE_URL")
        .env("GH_TOKEN", "fixture-token")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("failed after 6 attempts"));
    assert_eq!(
        fs::read_to_string(fx.tmp.path().join("gh.log"))
            .unwrap()
            .lines()
            .count(),
        6
    );
    assert!(!fx.tmp.path().join("cargo.log").exists());
}

#[test]
fn action_source_opt_in_builds_the_checkout_even_with_a_matching_binary() {
    let fx = Fixture::new();
    fs::copy(
        fx.tmp.path().join("unpacked/cargo-steward"),
        fx.tmp.path().join("cargo/bin/cargo-steward"),
    )
    .unwrap();
    script(
        fx.bin.join("cargo"),
        "printf '%s|%s|%s\\n' \"${RUSTC_WRAPPER-}\" \"${CARGO_BUILD_RUSTC_WRAPPER-}\" \"$*\" >> \"$CARGO_LOG\"\ncp \"$HOME/unpacked/cargo-steward\" \"$CARGO_HOME/bin/cargo-steward\"",
    );
    let out = fx
        .command()
        .env("STEWARD_FROM_SOURCE", "true")
        .env("RUSTC_WRAPPER", "")
        .env("CARGO_BUILD_RUSTC_WRAPPER", "")
        .output()
        .unwrap();
    fx.assert_ok(&out);
    let log = fs::read_to_string(fx.tmp.path().join("cargo.log")).unwrap();
    assert!(
        log.starts_with("||install --locked --force --path"),
        "{log}"
    );
    fx.assert_ok(
        &fx.command()
            .env("STEWARD_FROM_SOURCE", "true")
            .env("STEWARD_VERSION", format!("v{}", fx.version))
            .output()
            .unwrap(),
    );
    let log = fs::read_to_string(fx.tmp.path().join("cargo.log")).unwrap();
    assert!(
        log.contains(&format!(
            "install --locked --force cargo-steward --version {}",
            fx.version
        )),
        "{log}"
    );
}

#[test]
fn release_download_retries_http_504_and_connection_failures() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let fx = Fixture::new();
    let archive = fs::read(&fx.archive).unwrap();
    let digest = format!("{}\n", sha256(&archive));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        // Recover from an empty reply and from 504s on both archive and checksum.
        for request in 0..5 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut bytes = [0; 4096];
            let len = stream.read(&mut bytes).unwrap();
            let head = String::from_utf8_lossy(&bytes[..len]);
            if request == 0 {
                continue;
            }
            let (status, body): (&str, &[u8]) = if request == 1 || request == 3 {
                ("504 Gateway Timeout", b"temporary failure")
            } else if head.contains(".sha256 ") {
                ("200 OK", digest.as_bytes())
            } else {
                ("200 OK", &archive)
            };
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        }
    });
    // The real curl retains its retry behavior; allow loopback HTTP only in this fixture.
    script(
        fx.bin.join("curl"),
        "for arg do\n  case $arg in\n    =https,file) set -- \"$@\" '=https,file,http' ;;\n    *) set -- \"$@\" \"$arg\" ;;\n  esac\n  shift\ndone\nexec /usr/bin/curl \"$@\"",
    );
    let out = fx.command_with_body("sh \"$GITHUB_ACTION_PATH/scripts/install.sh\" --release \"$TEST_RELEASE\" --repo 0xdoublesharp/cargo-steward --no-setup")
        .env("TEST_RELEASE", &fx.version)
        .env("CARGO_STEWARD_RELEASE_URL", format!("http://{address}"))
        .output().unwrap();
    fx.assert_ok(&out);
    server.join().unwrap();
}
