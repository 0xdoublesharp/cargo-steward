//! Warm starts for new worktrees: `cargo steward seed` and the git hooks
//! that run it. (Stub: implemented by the warm-start workstream.)

use anyhow::{Result, bail};

#[derive(Debug, clap::Args)]
pub struct SeedArgs {}

pub fn run_seed(_args: &SeedArgs) -> Result<()> {
    bail!("`seed` isn't implemented yet")
}

#[derive(Debug, clap::Args)]
pub struct HooksArgs {}

pub fn run_hooks(_args: &HooksArgs) -> Result<()> {
    bail!("`hooks` isn't implemented yet")
}
