//! `--json` output: one JSON document per command on stdout, for scripts and
//! AI agents. Every document carries `"version": VERSION`; fields are only
//! ever added to a version, never renamed or retyped. Sizes are bytes,
//! times RFC 3339, and paths strings. docs/json.md describes each document.

use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;
use serde::Serialize;

/// The documents' format version.
pub const VERSION: u32 = 1;

/// Prints `doc` as the command's one JSON document.
pub fn print(doc: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(doc)?);
    Ok(())
}

/// What a command that failed prints in place of its document.
#[derive(Serialize)]
pub struct Failure<'a> {
    pub version: u32,
    pub error: &'a str,
}

/// A path as a string (lossily, for one that isn't UTF-8).
pub fn path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// A time as RFC 3339, to the second, in UTC.
pub fn time(time: SystemTime) -> String {
    humantime::format_rfc3339_seconds(time).to_string()
}

/// Whole seconds since `time`; 0 for a time in the future.
pub fn secs_ago(time: SystemTime) -> u64 {
    SystemTime::now()
        .duration_since(time)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn times_are_rfc3339_with_their_age() {
        let t = UNIX_EPOCH + Duration::from_secs(86_400);
        assert_eq!(time(t), "1970-01-02T00:00:00Z");
        let hour_ago = SystemTime::now() - Duration::from_secs(3600);
        assert!((3600..3700).contains(&secs_ago(hour_ago)));
        assert_eq!(secs_ago(SystemTime::now() + Duration::from_secs(60)), 0);
    }
}
