//! cargo-steward: keeps Cargo build output fast and bounded.
//!
//! One binary with two roles. As Cargo's `rustc-wrapper` it runs every
//! compile (see [`wrapper`]); as `cargo steward <command>` it reports on and
//! cleans up build directories.

mod ci;
mod config;
mod dedup;
mod doctor;
mod fsutil;
mod gc;
mod ledger;
mod proxy;
mod scan;
mod seed;
mod setup;
mod status;
mod wrapper;

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "cargo steward",
    bin_name = "cargo steward",
    version,
    about = "Keeps Cargo build output fast and bounded",
    long_about = "Keeps Cargo build output fast and bounded.\n\n\
        Run `cargo steward setup` once: cargo-steward becomes Cargo's rustc-wrapper \
        (keeping sccache or any other wrapper behind it), removes the debug object files \
        each rebuild leaves behind, and cleans up stale and abandoned build directories \
        in the background after builds."
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Make cargo-steward Cargo's rustc-wrapper and turn on automatic cleanup
    Setup(setup::Args),
    /// Check the setup and report what to change
    Doctor,
    /// List the build directories cargo-steward manages
    Status {
        /// Measure each directory's size (slower)
        #[arg(long)]
        sizes: bool,
    },
    /// Show what a cleanup would remove; with --apply, remove it
    Gc {
        /// Remove what the report lists
        #[arg(long)]
        apply: bool,
        /// List every item, not just the totals
        #[arg(short, long)]
        verbose: bool,
        /// Run as the wrapper's background cleanup: apply, quietly, at low priority
        #[arg(long, hide = true)]
        auto: bool,
    },
    /// Protect a build directory from cleanup (stale object files are still removed)
    Pin { dir: PathBuf },
    /// Undo `pin`
    Unpin { dir: PathBuf },
    /// Start a new worktree's build directory as a clone of its main one
    Seed(seed::SeedArgs),
    /// Install or remove the git hooks that seed new worktrees
    Hooks(seed::HooksArgs),
    /// Replace identical files across build directories with clones
    Dedup {
        /// Replace them; otherwise only report
        #[arg(long)]
        apply: bool,
        /// List every file group, not just the totals
        #[arg(short, long)]
        verbose: bool,
    },
    /// CI support: trim build output to what a job used, within a budget
    Ci(ci::Args),
    /// Install or remove the `cargo` proxy
    Shim(proxy::ShimArgs),
}

const COMMANDS: [&str; 15] = [
    "setup",
    "doctor",
    "status",
    "gc",
    "pin",
    "unpin",
    "seed",
    "hooks",
    "dedup",
    "ci",
    "shim",
    "help",
    "--help",
    "-h",
    "--version",
];

fn main() -> ExitCode {
    let mut args: Vec<OsString> = env::args_os().collect();
    // Installed as `cargo` ahead of the real one: act as the proxy.
    if args
        .first()
        .and_then(|a| Path::new(a).file_stem())
        .is_some_and(|stem| stem == "cargo")
    {
        proxy::run(args);
    }
    // `cargo steward ...` runs `cargo-steward steward ...`.
    if args.get(1).is_some_and(|a| a == "steward") {
        args.remove(1);
    } else if let Some(first) = args.get(1)
        && is_wrapper_call(first)
    {
        // Cargo's rustc-wrapper protocol: `cargo-steward <rustc> <args>...`.
        wrapper::run(args.split_off(1));
    }

    let cli = Cli::parse_from(args);
    let result = match cli.command {
        Cmd::Setup(args) => setup::run(&args),
        Cmd::Doctor => doctor::run(),
        Cmd::Status { sizes } => status::run(sizes),
        Cmd::Gc {
            apply,
            verbose,
            auto,
        } => {
            if auto {
                gc::become_background();
            }
            gc::run(gc::Options {
                apply: apply || auto,
                auto,
                verbose,
            })
        }
        Cmd::Pin { dir } => pin(&dir, true),
        Cmd::Unpin { dir } => pin(&dir, false),
        Cmd::Seed(args) => seed::run_seed(&args),
        Cmd::Hooks(args) => seed::run_hooks(&args),
        Cmd::Dedup { apply, verbose } => dedup::run(apply, verbose),
        Cmd::Ci(args) => ci::run(&args),
        Cmd::Shim(args) => proxy::run_shim(&args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Whether the first argument is a compiler cargo is asking us to wrap,
/// rather than one of our commands.
fn is_wrapper_call(first: &OsString) -> bool {
    let text = first.to_string_lossy();
    if COMMANDS.contains(&text.as_ref()) || text.starts_with('-') {
        return false;
    }
    let name = Path::new(first)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned());
    text.contains('/')
        || name.is_some_and(|n| n.contains("rustc") || n.contains("clippy"))
        || env::var_os("CARGO").is_some()
}

fn pin(dir: &Path, on: bool) -> Result<()> {
    let dir = dir
        .canonicalize()
        .with_context(|| dir.display().to_string())?;
    let Some(root) = scan::find_root(&dir) else {
        bail!("{} isn't inside a cargo build directory", dir.display());
    };
    let file = root.join(gc::PIN_FILE);
    if on {
        fs::write(&file, "cargo-steward: keep this build directory\n")?;
        println!("Pinned {}", root.display());
    } else {
        match fs::remove_file(&file) {
            Ok(()) => println!("Unpinned {}", root.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("{} wasn't pinned", root.display())
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
