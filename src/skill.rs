//! `cargo steward skill`: installs the agent skill that teaches AI coding
//! agents (Claude Code, Codex) how to work with cargo-steward. The skill is
//! compiled in, so every installed binary carries the matching version.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// The skill, `skills/cargo-steward/SKILL.md` in the repository.
pub const SKILL: &str = include_str!("../skills/cargo-steward/SKILL.md");

/// The directory name agents look the skill up by.
const NAME: &str = "cargo-steward";

#[derive(Debug, clap::Args)]
pub struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Install the skill for each agent found (Claude Code: ~/.claude/skills,
    /// Codex: ~/.codex/skills), or into the given skills directories
    Install {
        /// A skills directory to install into, instead of the detected ones
        /// (repeatable)
        #[arg(long = "dir", value_name = "DIR")]
        dirs: Vec<PathBuf>,
    },
    /// Remove the skill from the detected (or given) skills directories
    Uninstall {
        #[arg(long = "dir", value_name = "DIR")]
        dirs: Vec<PathBuf>,
    },
    /// Show where the skill is installed and whether it's current
    Status,
    /// Print the skill
    Show,
}

pub fn run(args: &Args) -> Result<()> {
    match &args.command {
        Command::Install { dirs } => {
            let dirs = targets(dirs)?;
            for dir in &dirs {
                let path = install(dir)?;
                println!("Installed {}", path.display());
            }
            println!("Agents load it in their next session.");
        }
        Command::Uninstall { dirs } => {
            for dir in targets(dirs)? {
                if uninstall(&dir)? {
                    println!("Removed {}", dir.join(NAME).display());
                }
            }
        }
        Command::Status => {
            let found = detected();
            if found.is_empty() {
                println!("No agent skills directory found (~/.claude or ~/.codex).");
            }
            for dir in found {
                let file = dir.join(NAME).join("SKILL.md");
                let state = match fs::read_to_string(&file) {
                    Ok(text) if text == SKILL => "installed, current",
                    Ok(_) => "installed, out of date (run `cargo steward skill install`)",
                    Err(_) => "not installed",
                };
                println!("{}: {state}", file.display());
            }
        }
        Command::Show => print!("{SKILL}"),
    }
    Ok(())
}

/// The given directories, or those of the agents installed here.
fn targets(given: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if !given.is_empty() {
        return Ok(given.to_vec());
    }
    let found = detected();
    if found.is_empty() {
        bail!("found neither ~/.claude nor ~/.codex; pass the agent's skills directory with --dir");
    }
    Ok(found)
}

/// Skills directories of the agents whose config directories exist: Claude
/// Code's `~/.claude` (and `$CLAUDE_CONFIG_DIR` when that points elsewhere),
/// and Codex's `~/.codex` (and `$CODEX_HOME`).
fn detected() -> Vec<PathBuf> {
    let home = env::var_os("HOME").map(PathBuf::from);
    let var = |name: &str| {
        env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let candidates = [
        var("CLAUDE_CONFIG_DIR"),
        home.as_ref().map(|h| h.join(".claude")),
        var("CODEX_HOME"),
        home.as_ref().map(|h| h.join(".codex")),
    ];
    let mut dirs: Vec<PathBuf> = Vec::new();
    for dir in candidates.into_iter().flatten() {
        let skills = dir.join("skills");
        let same = |d: &PathBuf| d.canonicalize().ok() == skills.canonicalize().ok() && d.exists();
        if dir.is_dir() && !dirs.iter().any(|d| d == &skills || same(d)) {
            dirs.push(skills);
        }
    }
    dirs
}

/// Writes the skill into `skills_dir/cargo-steward/SKILL.md`.
fn install(skills_dir: &Path) -> Result<PathBuf> {
    let dir = skills_dir.join(NAME);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let file = dir.join("SKILL.md");
    crate::config::write_atomic(&file, SKILL.as_bytes())?;
    Ok(file)
}

/// Removes the skill's directory, if it holds this skill (and nothing else).
fn uninstall(skills_dir: &Path) -> Result<bool> {
    let dir = skills_dir.join(NAME);
    let file = dir.join("SKILL.md");
    let Ok(text) = fs::read_to_string(&file) else {
        return Ok(false);
    };
    if !text.contains(&format!("name: {NAME}")) {
        bail!("{} isn't cargo-steward's skill; leaving it", file.display());
    }
    fs::remove_file(&file)?;
    // Only if nothing else was put there.
    let _ = fs::remove_dir(&dir);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_skill_has_frontmatter_agents_can_load() {
        let mut lines = SKILL.lines();
        assert_eq!(lines.next(), Some("---"));
        assert_eq!(lines.next(), Some("name: cargo-steward"));
        let description = lines.next().unwrap_or_default();
        assert!(
            description.starts_with("description: Use when"),
            "{description}"
        );
        assert!(SKILL[3..].contains("\n---\n"));
    }

    #[test]
    fn every_command_the_skill_mentions_exists() {
        use clap::CommandFactory;
        let cli = crate::Cli::command();
        let known: Vec<&str> = cli.get_subcommands().map(|c| c.get_name()).collect();
        for word in SKILL.split("cargo steward ").skip(1) {
            let command = word
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
                .next()
                .unwrap_or_default();
            if command.is_empty() {
                continue;
            }
            assert!(
                known.contains(&command),
                "the skill mentions `cargo steward {command}`"
            );
        }
    }

    #[test]
    fn installs_and_uninstalls_without_touching_other_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join("skills");
        fs::create_dir_all(skills.join("other")).unwrap();
        fs::write(skills.join("other/SKILL.md"), "---\nname: other\n---\n").unwrap();

        let file = install(&skills).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), SKILL);
        assert!(uninstall(&skills).unwrap());
        assert!(!skills.join(NAME).exists());
        assert!(skills.join("other/SKILL.md").exists());
        assert!(!uninstall(&skills).unwrap());

        // Someone else's skill under the same name is left alone.
        fs::create_dir_all(skills.join(NAME)).unwrap();
        fs::write(skills.join(NAME).join("SKILL.md"), "---\nname: mine\n---\n").unwrap();
        assert!(uninstall(&skills).is_err());
    }
}
