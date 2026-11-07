//! `cargo steward setup`: makes cargo-steward cargo's `rustc-wrapper` in the
//! user-wide Cargo config, keeping whatever wrapper was there (such as
//! sccache) as the inner wrapper, and writes cargo-steward's own settings.

use std::env;
use std::fs;
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
    /// Put the previous wrapper back and remove what setup added to Cargo's config
    #[arg(long, conflicts_with_all = ["inner", "scan_roots", "build_dir", "no_auto_gc"])]
    undo: bool,
    /// Show the changes without making them
    #[arg(long)]
    dry_run: bool,
}

pub fn run(args: &Args) -> Result<()> {
    let paths = Paths::resolve().context("cannot locate the home directory")?;
    let cargo_home = config::cargo_home().context("cannot locate CARGO_HOME")?;
    let cargo_config = cargo_config_path(&cargo_home);
    let original = fs::read_to_string(&cargo_config).unwrap_or_default();
    let mut doc: DocumentMut = original
        .parse()
        .with_context(|| format!("parsing {}", cargo_config.display()))?;
    let mut settings = config::load(&paths)?;
    let exe = steward_exe(&cargo_home)?;

    let current = doc
        .get("build")
        .and_then(|b| b.get("rustc-wrapper"))
        .and_then(Item::as_str)
        .map(str::to_owned);
    let current_is_us = current.as_deref().is_some_and(is_steward);

    if args.undo {
        undo(&mut doc, &mut settings, current_is_us);
    } else {
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
        for root in &args.scan_roots {
            let root = root
                .canonicalize()
                .with_context(|| format!("scan root {}", root.display()))?;
            if !settings.scan_roots.contains(&root) {
                settings.scan_roots.push(root);
            }
        }
        let build = build_table(&mut doc);
        build["rustc-wrapper"] = value(exe.to_string_lossy().as_ref());
        if let Some(dir) = &args.build_dir {
            let dir = absolute(dir)?;
            build["build-dir"] = value(format!("{}/{{workspace-path-hash}}", dir.display()));
            settings.managed_build_dir = true;
        }
    }

    let updated = doc.to_string();
    print_plan(&cargo_config, &original, &updated, &paths, &settings);
    if args.dry_run {
        println!("\n(dry run: nothing written)");
        return Ok(());
    }
    if updated != original {
        if !original.is_empty() {
            let backup = cargo_config
                .with_extension(format!("toml.steward-backup-{}", crate::ledger::now_secs()));
            fs::write(&backup, &original)
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

fn undo(doc: &mut DocumentMut, settings: &mut Config, current_is_us: bool) {
    let build = build_table(doc);
    if current_is_us {
        match &settings.inner_wrapper {
            Some(inner) => build["rustc-wrapper"] = value(inner.as_str()),
            None => {
                build.remove("rustc-wrapper");
            }
        }
    }
    if settings.managed_build_dir {
        build.remove("build-dir");
        settings.managed_build_dir = false;
    }
    if build.is_empty() {
        doc.remove("build");
    }
    settings.auto_gc = false;
}

/// `$CARGO_HOME/config.toml`, or the older extensionless `config` when only
/// that exists.
fn cargo_config_path(cargo_home: &Path) -> PathBuf {
    let toml = cargo_home.join("config.toml");
    let legacy = cargo_home.join("config");
    if !toml.exists() && legacy.is_file() {
        legacy
    } else {
        toml
    }
}

fn build_table(doc: &mut DocumentMut) -> &mut Table {
    let item = doc
        .entry("build")
        .or_insert_with(|| Item::Table(Table::new()));
    if !item.is_table() {
        *item = Item::Table(Table::new());
    }
    item.as_table_mut().expect("just made a table")
}

/// The installed binary cargo should run: `$CARGO_HOME/bin/cargo-steward`
/// when that's where this one is installed, else this executable.
fn steward_exe(cargo_home: &Path) -> Result<PathBuf> {
    let installed = cargo_home.join("bin").join(exe_name());
    if installed.is_file() {
        return Ok(installed);
    }
    let current = env::current_exe().context("locating cargo-steward")?;
    Ok(current.canonicalize().unwrap_or(current))
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
        build_table(&mut doc)["rustc-wrapper"] = value("/home/u/.cargo/bin/cargo-steward");
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
        undo(&mut doc, &mut settings, true);
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
        undo(&mut doc, &mut settings, true);
        assert_eq!(doc.to_string().trim(), "");
    }

    #[test]
    fn recognizes_itself() {
        assert!(is_steward("/Users/me/.cargo/bin/cargo-steward"));
        assert!(is_steward("cargo-steward"));
        assert!(!is_steward("sccache"));
    }
}
