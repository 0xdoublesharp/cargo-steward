//! The `cargo` proxy: installed as `cargo` ahead of the real one on PATH, it
//! routes each worktree's build output, seeds new worktrees, limits
//! concurrent builds, then runs the real cargo. (Stub: implemented by the
//! proxy workstream.)

use std::ffi::OsString;

use anyhow::{Result, bail};

#[derive(Debug, clap::Args)]
pub struct ShimArgs {}

pub fn run_shim(_args: &ShimArgs) -> Result<()> {
    bail!("`shim` isn't implemented yet")
}

/// Runs as `cargo`: `args[0]` is the program name as invoked.
pub fn run(_args: Vec<OsString>) -> ! {
    eprintln!("error: cargo-steward's cargo proxy isn't implemented yet");
    std::process::exit(1)
}
