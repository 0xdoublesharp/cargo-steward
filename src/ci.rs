//! CI support: `cargo steward ci`. (Stub: implemented by the CI workstream.)

use anyhow::{Result, bail};

#[derive(Debug, clap::Args)]
pub struct Args {}

pub fn run(_args: &Args) -> Result<()> {
    bail!("`ci` isn't implemented yet")
}
