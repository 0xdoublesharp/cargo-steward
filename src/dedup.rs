//! Replaces identical files across build directories with copy-on-write
//! clones. (Stub: implemented by the warm-start workstream.)

use anyhow::{Result, bail};

pub fn run(_apply: bool, _verbose: bool) -> Result<()> {
    bail!("`dedup` isn't implemented yet")
}
