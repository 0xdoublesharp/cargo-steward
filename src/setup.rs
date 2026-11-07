//! `cargo steward setup`: makes cargo-steward cargo's `rustc-wrapper` in the
//! user-wide Cargo config, keeping whatever wrapper was there (such as
//! sccache) as the inner wrapper, and writes cargo-steward's own settings.
//! On a first run it also finds where build output already goes and adds
//! those places as scan roots; on macOS it can create a build volume.

mod roots;
mod volume;

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, Item, Table, value};

use crate::config::{self, Config, Paths};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Wrapper to run rustc through [default: the current `build.rustc-wrapper`, such as sccache]
    #[arg(long, value_name = "PROGRAM")]
    inner: Option<String>,
    /// Directory tree to search for build directories, such as a scratch volume (repeatable)
    #[arg(long = "scan-root", value_name = "DIR")]
    scan_roots: Vec<PathBuf>,
    /// Also set `build.build-dir` to DIR/{workspace-path-hash}, moving every
    /// workspace's intermediate build output there
    #[arg(long, value_name = "DIR")]
    build_dir: Option<PathBuf>,
    /// Don't clean up automatically after builds
    #[arg(long)]
    no_auto_gc: bool,
    /// macOS: create (or reuse) the APFS volume /Volumes/NAME in the system
    /// disk's container for build output, and add it as a scan root
    #[arg(long, value_name = "NAME")]
    volume: Option<String>,
    /// Put the previous wrapper back and remove what setup added to Cargo's config
    #[arg(long, conflicts_with_all = ["inner", "scan_roots", "build_dir", "no_auto_gc", "volume"])]
    undo: bool,
    /// Show the changes without making them
    #[arg(long)]
    dry_run: bool,
}

pub fn run(args: &Args) -> Result<()> {
    let paths = Paths::resolve().context("cannot locate the home directory")?;
    let cargo_home = config::cargo_home().context("cannot locate CARGO_HOME")?;
    let cargo_config = config::user_cargo_config(&cargo_home);
    // Only a missing file is an empty config: one that can't be read must
    // not be replaced by what setup adds.
    let original = match fs::read_to_string(&cargo_config) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", cargo_config.display())),
    };
    let mut doc: DocumentMut = original
        .parse()
        .with_context(|| format!("parsing {}", cargo_config.display()))?;
    if let Some(build) = doc.get("build")
        && !build.is_table_like()
    {
        bail!(
            "`build` in {} is {}, not a table; fix it, then run setup again",
            cargo_config.display(),
            build.type_name()
        );
    }
    let mut settings = config::load(&paths)?;
    let exe = steward_exe(&cargo_home)?;

    let build_key = |key: &str| {
        doc.get("build")
            .and_then(|b| b.get(key))
            .and_then(Item::as_str)
            .map(str::to_owned)
    };
    let current = build_key("rustc-wrapper");
    let current_build_dir = build_key("build-dir");
    let current_is_us = current.as_deref().is_some_and(is_steward);

    if args.undo {
        undo(&mut doc, &mut settings, current_is_us)?;
    } else {
        // Record what setup replaces, for `--undo`. Once cargo-steward is the
        // wrapper (or manages build-dir), the record from that first run stays.
        if !current_is_us {
            settings.previous_wrapper = Some(current.clone().unwrap_or_default());
        }
        if args.build_dir.is_some() && !settings.managed_build_dir {
            settings.previous_build_dir = Some(current_build_dir.unwrap_or_default());
        }
        let inner = match (&args.inner, &current) {
            (Some(inner), _) => Some(inner.clone()),
            (None, Some(current)) if !current_is_us => Some(current.clone()),
            (None, _) => settings.inner_wrapper.clone(),
        };
        if inner.as_deref().is_some_and(is_steward) {
            bail!("the inner wrapper can't be cargo-steward itself");
        }
        settings.inner_wrapper = inner.filter(|i| !i.is_empty());
        settings.auto_gc = !args.no_auto_gc;
        let mut new_roots = Vec::new();
        if let Some(name) = &args.volume {
            let mount = volume::run(name, args.dry_run)?;
            // In a dry run the volume may not exist yet.
            new_roots.push(mount.canonicalize().unwrap_or(mount));
        }
        for root in &args.scan_roots {
            new_roots.push(
                root.canonicalize()
                    .with_context(|| format!("scan root {}", root.display()))?,
            );
        }
        if new_roots.is_empty() && settings.scan_roots.is_empty() {
            let detected = roots::detect(&roots::Sources {
                target_dir_env: env::var_os("CARGO_TARGET_DIR")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from),
                cargo_config: Some(&doc),
                cargo_home: cargo_home.clone(),
                recorded: roots::recorded_build_dirs(&paths),
                home: env::var_os("HOME").map(PathBuf::from),
            });
            if !detected.is_empty() {
                println!("No scan roots were configured; adding where build output already goes:");
                for d in &detected {
                    println!("  {}  ({})", d.root.display(), d.reason);
                }
                println!();
            }
            new_roots.extend(detected.into_iter().map(|d| d.root));
        }
        for root in new_roots {
            if !settings.scan_roots.contains(&root) {
                settings.scan_roots.push(root);
            }
        }
        let build = build_table(&mut doc)?;
        build["rustc-wrapper"] = value(exe.to_string_lossy().as_ref());
        if let Some(dir) = &args.build_dir {
            let dir = absolute(dir)?;
            build["build-dir"] = value(format!("{}/{{workspace-path-hash}}", dir.display()));
            settings.managed_build_dir = true;
        }
    }

    let updated = doc.to_string();
    print_plan(&cargo_config, &original, &updated, &paths, &settings);
    if args.undo {
        print_other_pieces(&paths);
    } else if let Some(name) = &args.volume
        && args.build_dir.is_none()
    {
        println!(
            "\nTo send every workspace's build output to the volume:\n  \
             cargo steward setup --build-dir {}",
            shell_quote(&Path::new("/Volumes").join(name).join("cargo-build"))
        );
    }
    if args.dry_run {
        println!("\n(dry run: nothing written)");
        return Ok(());
    }
    if updated != original {
        if !original.is_empty() {
            let backup = cargo_config
                .with_extension(format!("toml.steward-backup-{}", crate::ledger::now_secs()));
            // The backup holds what the config does, so it is as private.
            let permissions = fs::metadata(&cargo_config).ok().map(|m| m.permissions());
            config::write_with_permissions(&backup, original.as_bytes(), permissions.as_ref())
                .with_context(|| format!("writing {}", backup.display()))?;
            println!("Backed up the previous config to {}", backup.display());
        }
        fs::create_dir_all(&cargo_home)?;
        config::write_atomic(&cargo_config, updated.as_bytes())?;
    }
    config::save(&paths, &settings)?;
    warn_about_overrides();
    if !args.undo {
        println!("\nDone. Builds now run through cargo-steward. Next: `cargo steward doctor`.");
    }
    Ok(())
}

/// Puts back what setup replaced: the wrapper and build-dir recorded when
/// setup ran, or, for settings from versions that didn't record them, the
/// inner wrapper and no build-dir.
fn undo(doc: &mut DocumentMut, settings: &mut Config, current_is_us: bool) -> Result<()> {
    let build = build_table(doc)?;
    if current_is_us {
        let previous = match &settings.previous_wrapper {
            Some(recorded) => Some(recorded.as_str()).filter(|w| !w.is_empty()),
            None => settings.inner_wrapper.as_deref(),
        };
        match previous {
            Some(wrapper) => build["rustc-wrapper"] = value(wrapper),
            None => {
                build.remove("rustc-wrapper");
            }
        }
    }
    if settings.managed_build_dir {
        match settings
            .previous_build_dir
            .as_deref()
            .filter(|d| !d.is_empty())
        {
            Some(dir) => build["build-dir"] = value(dir),
            None => {
                build.remove("build-dir");
            }
        }
        settings.managed_build_dir = false;
    }
    if build.is_empty() {
        doc.remove("build");
    }
    settings.previous_wrapper = None;
    settings.previous_build_dir = None;
    settings.auto_gc = false;
    Ok(())
}

/// The `[build]` table, converted from an inline `build = { ... }` with its
/// keys kept, and created if missing. Fails if `build` is anything else.
fn build_table(doc: &mut DocumentMut) -> Result<&mut Table> {
    let item = doc
        .entry("build")
        .or_insert_with(|| Item::Table(Table::new()));
    if let Some(inline) = item.as_inline_table() {
        *item = Item::Table(inline.clone().into_table());
    }
    let kind = item.type_name();
    item.as_table_mut()
        .with_context(|| format!("`build` in Cargo's config is {kind}, not a table"))
}

/// The binary cargo should run, by a path that outlives upgrades:
/// `$CARGO_HOME/bin/cargo-steward` when it exists, else the first
/// `cargo-steward` on PATH that is this executable (a package manager's link,
/// not the versioned file it points at), else this executable as it was run.
fn steward_exe(cargo_home: &Path) -> Result<PathBuf> {
    let current = env::current_exe().context("locating cargo-steward")?;
    Ok(pick_exe(
        cargo_home,
        env::var_os("PATH").as_deref(),
        current,
    ))
}

fn pick_exe(cargo_home: &Path, path: Option<&OsStr>, current: PathBuf) -> PathBuf {
    let installed = cargo_home.join("bin").join(exe_name());
    if installed.is_file() {
        return installed;
    }
    path.into_iter()
        .flat_map(env::split_paths)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(exe_name()))
        .find(|candidate| crate::fsutil::same_file(candidate, &current))
        .unwrap_or(current)
}

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "cargo-steward.exe"
    } else {
        "cargo-steward"
    }
}

fn is_steward(program: &str) -> bool {
    Path::new(program)
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s == "cargo-steward")
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(env::current_dir()?.join(path))
}

fn print_plan(
    cargo_config: &Path,
    original: &str,
    updated: &str,
    paths: &Paths,
    settings: &Config,
) {
    let wrapper = |text: &str| -> Option<String> {
        let doc: DocumentMut = text.parse().ok()?;
        doc.get("build")?
            .get("rustc-wrapper")?
            .as_str()
            .map(str::to_owned)
    };
    let show = |w: Option<String>| w.unwrap_or_else(|| "(none)".to_owned());
    println!("Cargo config: {}", cargo_config.display());
    println!(
        "  build.rustc-wrapper: {} -> {}",
        show(wrapper(original)),
        show(wrapper(updated))
    );
    println!("cargo-steward settings: {}", paths.config().display());
    println!(
        "  inner wrapper: {}",
        settings.inner_wrapper.as_deref().unwrap_or("(none)")
    );
    println!(
        "  automatic cleanup: {}",
        if settings.auto_gc { "on" } else { "off" }
    );
    for root in &settings.scan_roots {
        println!("  scan root: {}", root.display());
    }
}

/// `setup --undo` reverts only what setup itself changed; these pieces are
/// installed and removed by their own commands.
fn print_other_pieces(paths: &Paths) {
    println!(
        "\nsetup --undo doesn't remove these, which may also be installed:\n  \
         git hooks that seed new worktrees   cargo steward hooks status    (remove: cargo steward hooks uninstall)\n  \
         the cargo proxy                     cargo steward shim status     (remove: cargo steward shim uninstall)\n  \
         settings and records                {}\n  \
         the binary                          cargo uninstall cargo-steward\n\
         See docs/install.md, \"Uninstall\".",
        paths.home.display()
    );
}

/// Quotes a path for a shell command line when it needs it.
pub fn shell_quote(path: &Path) -> String {
    let text = path.display().to_string();
    if !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+@%:,=".contains(&b))
    {
        text
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// An environment variable outranks the config file, which would bypass
/// cargo-steward.
fn warn_about_overrides() {
    for var in ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"] {
        if let Some(value) = env::var_os(var).filter(|v| !v.is_empty())
            && !is_steward(&value.to_string_lossy())
        {
            println!(
                "\nwarning: {var}={} is set in this environment and overrides Cargo's config, \
                 so builds started from it bypass cargo-steward. Point it at cargo-steward and set \
                 CARGO_STEWARD_INNER={} instead.",
                value.to_string_lossy(),
                value.to_string_lossy()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swaps_the_wrapper_in_and_back_out_keeping_comments() {
        let original = "# my settings\n[build]\nrustc-wrapper = \"sccache\" # cache\njobs = 8\n";
        let mut doc: DocumentMut = original.parse().unwrap();
        build_table(&mut doc).unwrap()["rustc-wrapper"] = value("/home/u/.cargo/bin/cargo-steward");
        let text = doc.to_string();
        assert!(text.contains("# my settings"));
        assert!(text.contains("jobs = 8"));
        assert!(text.contains("cargo-steward"));

        let mut settings = Config {
            inner_wrapper: Some("sccache".into()),
            auto_gc: true,
            ..Config::default()
        };
        let mut doc: DocumentMut = text.parse().unwrap();
        undo(&mut doc, &mut settings, true).unwrap();
        let restored = doc.to_string();
        assert!(restored.contains("rustc-wrapper = \"sccache\""));
        assert!(restored.contains("jobs = 8"));
        assert!(!settings.auto_gc);
    }

    #[test]
    fn undo_without_an_inner_wrapper_drops_the_empty_table() {
        let mut doc: DocumentMut = "[build]\nrustc-wrapper = \"/x/cargo-steward\"\n"
            .parse()
            .unwrap();
        let mut settings = Config::default();
        undo(&mut doc, &mut settings, true).unwrap();
        assert_eq!(doc.to_string().trim(), "");
    }

    #[test]
    fn quotes_paths_for_the_shell() {
        assert_eq!(shell_quote(Path::new("/a/b-c_d.e")), "/a/b-c_d.e");
        assert_eq!(shell_quote(Path::new("/a/b c")), "'/a/b c'");
        assert_eq!(shell_quote(Path::new("/a/it's")), r"'/a/it'\''s'");
    }

    #[test]
    fn recognizes_itself() {
        assert!(is_steward("/Users/me/.cargo/bin/cargo-steward"));
        assert!(is_steward("cargo-steward"));
        assert!(!is_steward("sccache"));
    }

    /// Cargo reads the extensionless `config` whenever it exists, even next
    /// to a `config.toml`; setup edits that one.
    #[test]
    fn prefers_the_legacy_name_as_cargo_does() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        assert_eq!(config::user_cargo_config(home), home.join("config.toml"));
        fs::write(home.join("config"), "").unwrap();
        assert_eq!(config::user_cargo_config(home), home.join("config"));
        fs::write(home.join("config.toml"), "").unwrap();
        assert_eq!(config::user_cargo_config(home), home.join("config"));
    }

    /// An inline `build = { ... }` becomes a `[build]` table with its keys;
    /// any other `build` value is refused rather than replaced.
    #[test]
    fn keeps_an_inline_build_table_and_refuses_other_values() {
        let mut doc: DocumentMut =
            "build = { jobs = 8, target-dir = \"/srv/t\", rustc-wrapper = \"sccache\" }\n"
                .parse()
                .unwrap();
        build_table(&mut doc).unwrap()["rustc-wrapper"] = value("/x/cargo-steward");
        let doc: DocumentMut = doc.to_string().parse().unwrap();
        assert_eq!(doc["build"]["jobs"].as_integer(), Some(8));
        assert_eq!(doc["build"]["target-dir"].as_str(), Some("/srv/t"));
        assert_eq!(
            doc["build"]["rustc-wrapper"].as_str(),
            Some("/x/cargo-steward")
        );

        // Undo keeps them too.
        let mut doc: DocumentMut = "build = { jobs = 8, rustc-wrapper = \"/x/cargo-steward\" }\n"
            .parse()
            .unwrap();
        let mut settings = Config {
            previous_wrapper: Some("sccache".into()),
            ..Config::default()
        };
        undo(&mut doc, &mut settings, true).unwrap();
        let doc: DocumentMut = doc.to_string().parse().unwrap();
        assert_eq!(doc["build"]["jobs"].as_integer(), Some(8));
        assert_eq!(doc["build"]["rustc-wrapper"].as_str(), Some("sccache"));

        let mut doc: DocumentMut = "build = 3\n".parse().unwrap();
        let err = format!("{:#}", build_table(&mut doc).unwrap_err());
        assert!(err.contains("not a table"), "{err}");
        assert_eq!(doc.to_string(), "build = 3\n");
        let err = format!(
            "{:#}",
            undo(&mut doc, &mut Config::default(), true).unwrap_err()
        );
        assert!(err.contains("not a table"), "{err}");
    }

    /// Undo restores the wrapper and build-dir that were there before setup,
    /// not the inner wrapper given with `--inner`.
    #[test]
    fn undo_restores_what_setup_recorded() {
        let mut doc: DocumentMut =
            "[build]\nrustc-wrapper = \"/x/cargo-steward\"\nbuild-dir = \"/new/{h}\"\n"
                .parse()
                .unwrap();
        let mut settings = Config {
            inner_wrapper: Some("sccache".into()),
            previous_wrapper: Some(String::new()),
            managed_build_dir: true,
            previous_build_dir: Some("/old/{h}".into()),
            ..Config::default()
        };
        undo(&mut doc, &mut settings, true).unwrap();
        assert_eq!(doc.to_string(), "[build]\nbuild-dir = \"/old/{h}\"\n");
        assert_eq!(settings.previous_wrapper, None);
        assert_eq!(settings.previous_build_dir, None);
        assert!(!settings.managed_build_dir);

        // A recorded wrapper is put back; no build-dir before means none after.
        let mut doc: DocumentMut =
            "[build]\nrustc-wrapper = \"/x/cargo-steward\"\nbuild-dir = \"/new/{h}\"\n"
                .parse()
                .unwrap();
        let mut settings = Config {
            inner_wrapper: Some("other".into()),
            previous_wrapper: Some("sccache".into()),
            managed_build_dir: true,
            previous_build_dir: Some(String::new()),
            ..Config::default()
        };
        undo(&mut doc, &mut settings, true).unwrap();
        assert_eq!(doc.to_string(), "[build]\nrustc-wrapper = \"sccache\"\n");
    }

    /// A package manager's link on PATH is recorded, not the versioned file
    /// it points at, which an upgrade removes.
    #[cfg(unix)]
    #[test]
    fn records_the_link_on_path_rather_than_the_versioned_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cellar = root.join("Cellar/cargo-steward/1.0/bin");
        let bin = root.join("bin");
        let elsewhere = root.join("elsewhere");
        for dir in [&cellar, &bin, &elsewhere] {
            fs::create_dir_all(dir).unwrap();
        }
        let real = cellar.join(exe_name());
        fs::write(&real, "").unwrap();
        std::os::unix::fs::symlink(&real, bin.join(exe_name())).unwrap();
        // A different cargo-steward earlier on PATH isn't this one.
        fs::write(elsewhere.join(exe_name()), "").unwrap();
        let path =
            env::join_paths(["relative".as_ref(), elsewhere.as_path(), bin.as_path()]).unwrap();
        let cargo_home = root.join("cargo");
        assert_eq!(
            pick_exe(&cargo_home, Some(&path), real.clone()),
            bin.join(exe_name())
        );
        // Not on PATH: this executable, as it was run (not resolved).
        let link = root.join("run-as");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let unrelated = env::join_paths([elsewhere.as_path()]).unwrap();
        assert_eq!(pick_exe(&cargo_home, Some(&unrelated), link.clone()), link);
        assert_eq!(pick_exe(&cargo_home, None, link.clone()), link);
    }

    #[test]
    fn undo_removes_a_managed_build_dir_and_leaves_another_wrapper() {
        let mut doc: DocumentMut = "[build]\nrustc-wrapper = \"sccache\"\nbuild-dir = \"/b/{x}\"\n"
            .parse()
            .unwrap();
        let mut settings = Config {
            managed_build_dir: true,
            inner_wrapper: Some("other".into()),
            ..Config::default()
        };
        undo(&mut doc, &mut settings, false).unwrap();
        assert_eq!(doc.to_string(), "[build]\nrustc-wrapper = \"sccache\"\n");
        assert!(!settings.managed_build_dir);
    }

    #[test]
    fn uses_the_installed_binary_when_there_is_one() {
        let tmp = tempfile::tempdir().unwrap();
        let current = steward_exe(tmp.path()).unwrap();
        assert!(current.is_absolute() && current.exists());
        let installed = tmp.path().join("bin").join(exe_name());
        fs::create_dir_all(installed.parent().unwrap()).unwrap();
        fs::write(&installed, "").unwrap();
        assert_eq!(steward_exe(tmp.path()).unwrap(), installed);
    }

    #[test]
    fn makes_relative_paths_absolute() {
        assert_eq!(absolute(Path::new("/a/b")).unwrap(), PathBuf::from("/a/b"));
        assert_eq!(
            absolute(Path::new("a/b")).unwrap(),
            env::current_dir().unwrap().join("a/b")
        );
        assert_eq!(shell_quote(Path::new("")), "''");
    }
}
