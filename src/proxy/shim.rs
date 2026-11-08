//! `cargo steward shim`: installs the proxy as `cargo` in a bin directory,
//! puts that directory on PATH, and explains what the proxy would do.

use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{Plan, gate, is_empty, real_cargo, settings, slot_dir, this_exe};
use crate::config::{self, Paths};
use crate::ledger;
use crate::seed::{self, baseline, baseline::Preview};

#[derive(Debug, clap::Subcommand)]
pub enum ShimCommand {
    /// Install the proxy: a `cargo` symlink to cargo-steward in a bin directory
    Install {
        /// Where to put the symlink [default: <steward home>/bin]
        #[arg(long, value_name = "DIR")]
        bin_dir: Option<PathBuf>,
        /// Also put the bin directory first on PATH in this shell's startup file
        #[arg(long, value_enum)]
        shell: Option<Shell>,
    },
    /// Remove the symlink, and the PATH lines `install --shell` added
    Uninstall {
        /// Where the symlink is [default: <steward home>/bin]
        #[arg(long, value_name = "DIR")]
        bin_dir: Option<PathBuf>,
    },
    /// Whether the proxy is installed and first on PATH, and what it would do here
    Status,
    /// What the proxy would do for a cargo command run here
    Explain {
        /// The cargo command's arguments [default: build]
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Shell {
    Zsh,
    Bash,
    Fish,
}

const ALL_SHELLS: [Shell; 3] = [Shell::Zsh, Shell::Bash, Shell::Fish];
const BEGIN: &str = "# >>> cargo-steward shim >>>";
const END: &str = "# <<< cargo-steward shim <<<";

pub fn run(command: &ShimCommand) -> Result<()> {
    match command {
        ShimCommand::Install { bin_dir, shell } => install(bin_dir.as_deref(), *shell),
        ShimCommand::Uninstall { bin_dir } => uninstall(bin_dir.as_deref()),
        ShimCommand::Status => status(),
        ShimCommand::Explain { args } => explain(args),
    }
}

fn bin_dir(given: Option<&Path>) -> Result<PathBuf> {
    match given {
        Some(dir) => Ok(dir.to_path_buf()),
        None => Ok(Paths::resolve()
            .context("can't find the steward home: set CARGO_STEWARD_HOME or HOME")?
            .home
            .join("bin")),
    }
}

/// Whether a symlink points at a cargo-steward executable (this one or
/// another install, existing or not).
fn ours(link: &Path) -> bool {
    fs::read_link(link).is_ok_and(|target| {
        target
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("cargo-steward"))
    })
}

fn install(bin_dir_arg: Option<&Path>, shell: Option<Shell>) -> Result<()> {
    let me = this_exe().context("can't locate the cargo-steward executable")?;
    let bin = bin_dir(bin_dir_arg)?;
    fs::create_dir_all(&bin).with_context(|| format!("creating {}", bin.display()))?;
    let link = bin.join("cargo");
    match fs::symlink_metadata(&link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if link.canonicalize().ok().as_deref() == Some(me.as_path()) {
                println!("{} already points at {}", link.display(), me.display());
            } else if ours(&link) {
                fs::remove_file(&link)?;
                symlink(&me, &link)?;
                println!("Repointed {} at {}", link.display(), me.display());
            } else {
                let target = fs::read_link(&link).unwrap_or_default();
                bail!(
                    "{} already exists and points at {}; remove it first",
                    link.display(),
                    target.display()
                );
            }
        }
        Ok(_) => bail!(
            "{} already exists and isn't a symlink; refusing to replace it",
            link.display()
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            symlink(&me, &link).with_context(|| format!("creating {}", link.display()))?;
            println!("Created {} -> {}", link.display(), me.display());
        }
        Err(e) => return Err(e).with_context(|| link.display().to_string()),
    }

    let line = path_line(shell.unwrap_or_else(login_shell), &bin);
    match shell {
        Some(shell) => {
            let rc = rc_file(shell)?;
            let old = match fs::read_to_string(&rc) {
                Ok(text) => text,
                Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
                Err(e) => return Err(e).with_context(|| rc.display().to_string()),
            };
            if let Some(dir) = rc.parent() {
                fs::create_dir_all(dir)?;
            }
            if old.contains(BEGIN) {
                // Rewritten in place, so an rc file that's a symlink stays one.
                fs::write(&rc, with_block(&old, &line))?;
            } else {
                let mut file = OpenOptions::new().create(true).append(true).open(&rc)?;
                file.write_all(&with_block(&old, &line).as_bytes()[old.len()..])?;
            }
            println!(
                "Put {} first on PATH in {}. Open a new shell, or run:\n\n    {line}\n",
                bin.display(),
                rc.display()
            );
        }
        None if first_cargo_is(&me) => {}
        None => println!(
            "Put {} first on PATH: add this to your shell's startup file (or rerun with \
             --shell zsh|bash|fish to have it added):\n\n    {line}\n",
            bin.display()
        ),
    }
    if let Some(paths) = Paths::resolve() {
        println!(
            "Routing is set by target-root, tmp-root and max-builds in {}.",
            paths.config().display()
        );
    }
    Ok(())
}

fn uninstall(bin_dir_arg: Option<&Path>) -> Result<()> {
    let link = bin_dir(bin_dir_arg)?.join("cargo");
    match fs::symlink_metadata(&link) {
        Ok(meta) if meta.file_type().is_symlink() && ours(&link) => {
            fs::remove_file(&link)?;
            println!("Removed {}", link.display());
        }
        Ok(_) => println!(
            "Left {} alone: it isn't cargo-steward's proxy",
            link.display()
        ),
        Err(_) => println!("No proxy at {}", link.display()),
    }
    for shell in ALL_SHELLS {
        let Ok(rc) = rc_file(shell) else { continue };
        let Ok(text) = fs::read_to_string(&rc) else {
            continue;
        };
        if let Some(stripped) = without_block(&text) {
            fs::write(&rc, stripped).with_context(|| rc.display().to_string())?;
            println!("Removed the PATH line from {}", rc.display());
        }
    }
    Ok(())
}

/// The first `cargo` on PATH.
fn first_cargo() -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|d| d.join("cargo"))
        .find(|p| p.is_file())
}

fn first_cargo_is(me: &Path) -> bool {
    first_cargo()
        .and_then(|p| p.canonicalize().ok())
        .is_some_and(|p| p == me)
}

fn status() -> Result<()> {
    let me = this_exe();
    let link = bin_dir(None)?.join("cargo");
    let installed = link.canonicalize().ok();
    match (&installed, &me) {
        (Some(target), Some(me)) if target == me => {
            println!("proxy:        {} -> {}", link.display(), me.display())
        }
        _ if ours(&link) => println!(
            "proxy:        {} points at another cargo-steward ({}); rerun `shim install`",
            link.display(),
            fs::read_link(&link).unwrap_or_default().display()
        ),
        _ => println!("proxy:        not installed in {}", link.display()),
    }
    match (first_cargo(), &me) {
        (Some(first), Some(me)) if first.canonicalize().ok().as_ref() == Some(me) => {
            println!("first cargo:  {} (the proxy)", first.display())
        }
        (Some(first), _) => println!(
            "first cargo:  {} (not the proxy: put {} ahead of it on PATH)",
            first.display(),
            link.parent().unwrap_or(Path::new("")).display()
        ),
        (None, _) => println!("first cargo:  none on PATH"),
    }
    let rcs: Vec<String> = ALL_SHELLS
        .iter()
        .filter_map(|&s| rc_file(s).ok())
        .filter(|rc| fs::read_to_string(rc).is_ok_and(|t| t.contains(BEGIN)))
        .map(|rc| rc.display().to_string())
        .collect();
    if !rcs.is_empty() {
        println!("PATH set in:  {}", rcs.join(", "));
    }
    println!();
    explain(&[])
}

fn explain(args: &[OsString]) -> Result<()> {
    let args: Vec<OsString> = if args.is_empty() {
        vec!["build".into()]
    } else {
        args.to_vec()
    };
    let paths = Paths::resolve();
    let config = settings(paths.as_ref(), false);
    match &paths {
        Some(p) => {
            if p.config().exists() {
                println!("settings:     {}", p.config().display());
            } else {
                println!(
                    "settings:     {} (missing: defaults, nothing routed)",
                    p.config().display()
                );
            }
            for problem in config::load_lenient(p).1 {
                println!("              {problem}");
            }
        }
        None => println!("settings:     none (no HOME): defaults"),
    }
    println!(
        "command:      cargo {}",
        args.iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    );
    let cwd = env::current_dir()?;
    match Plan::new(&config, &cwd, &args) {
        Ok(plan) => describe(&config, paths.as_ref(), &plan),
        Err(e) => println!("routing:      not routed: {e:#}"),
    }
    let me = this_exe();
    match real_cargo(&config, me.as_deref()) {
        Some(real) => println!("real cargo:   {}", real.display()),
        None => println!("real cargo:   none found: the proxy can't run cargo"),
    }
    let command = gate::command(&args).unwrap_or_else(|| "(none)".into());
    let plan = Plan::new(&config, &cwd, &args).ok();
    let limit = plan.as_ref().and_then(|p| p.max_builds).filter(|&n| n > 0);
    let set = plan.as_ref().map(|p| p.slots.clone()).unwrap_or_default();
    if let Some(route) = plan.as_ref().and_then(|p| p.route.as_ref()) {
        println!("route:        {}", route.display());
    }
    match (limit, &paths) {
        (None, _) | (_, None) => println!("build slot:   no limit (max-builds isn't set)"),
        (Some(n), Some(paths)) if gate::is_build(&args) => println!(
            "build slot:   takes one of {n} (in {})",
            slot_dir(paths, &set).display()
        ),
        (Some(_), _) => println!("build slot:   none needed: `{command}` doesn't build"),
    }
    Ok(())
}

fn describe(config: &config::Config, paths: Option<&Paths>, plan: &Plan) {
    println!("directory:    {}", plan.dir.display());
    match &plan.namespace.git {
        Some(git) => {
            let place = match &git.worktree {
                Some(worktree) => worktree.display().to_string(),
                None => "inside the git directory".into(),
            };
            if git.linked() {
                println!(
                    "checkout:     {place} (linked worktree; git dir {})",
                    git.checkout.display()
                );
            } else {
                println!("checkout:     {place} (main)");
            }
        }
        None => println!("checkout:     not in git (standalone)"),
    }
    println!("namespace:    {}", plan.namespace.relative().display());
    let routed = |dir: &Option<PathBuf>, setting: &str| match (&plan.skip, dir) {
        (_, None) => format!("not routed: {setting} isn't set"),
        (Some(why), Some(_)) => format!("not routed: {why}"),
        (None, Some(dir)) => dir.display().to_string(),
    };
    println!("target dir:   {}", routed(&plan.target, "target-root"));
    println!("tmp dir:      {}", routed(&plan.tmp, "tmp-root"));
    let seed = if !config.seed {
        "off".to_string()
    } else if plan.skip.is_some() || plan.target.is_none() {
        "no: nothing routed".into()
    } else if let (Some(target), Some(namespace)) = (&plan.target, &plan.seed_from) {
        if is_empty(target) {
            seed_source(paths, target, namespace, &plan.namespace.key)
        } else {
            "no: the target dir already has files".into()
        }
    } else {
        "no: only builds in a linked worktree are seeded".into()
    };
    println!("seed:         {seed}");
}

/// Which of the other target directories in `namespace` seeding `target`
/// would clone now, and why that one.
fn seed_source(paths: Option<&Paths>, target: &Path, namespace: &Path, key: &str) -> String {
    let registry = paths.map(ledger::peek).unwrap_or_default();
    let candidates = baseline::namespace_candidates(namespace, key, &registry);
    match baseline::preview(target, &candidates) {
        Preview::Nothing => format!(
            "no: no other worktree has built in {} yet",
            namespace.display()
        ),
        Preview::Ready { pick, busy } => {
            let mut why = candidates[pick].about();
            if !busy.is_empty() {
                let labels: Vec<&str> =
                    busy.iter().map(|&i| candidates[i].label.as_str()).collect();
                why.push_str(&format!("; busy: {}", labels.join(", ")));
            }
            format!("yes, from {} ({why})", candidates[pick].dir.display())
        }
        Preview::AllBusy => format!(
            "not now: builds are using every build to seed from; it waits up to {} s for one",
            seed::LOCK_WAIT.as_secs()
        ),
    }
}

/// The shell `$SHELL` names; zsh when unknown.
fn login_shell() -> Shell {
    match env::var_os("SHELL")
        .as_deref()
        .and_then(|s| Path::new(s).file_name())
        .and_then(|n| n.to_str())
    {
        Some("bash") => Shell::Bash,
        Some("fish") => Shell::Fish,
        _ => Shell::Zsh,
    }
}

/// The startup file `--shell` edits.
fn rc_file(shell: Shell) -> Result<PathBuf> {
    let var = |name: &str| {
        env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let home = var("HOME").context("HOME isn't set");
    Ok(match shell {
        // `.zshenv` is read by every zsh, including the non-interactive ones
        // scripts and agents run, not just interactive shells.
        Shell::Zsh => var("ZDOTDIR").map_or_else(|| home, Ok)?.join(".zshenv"),
        Shell::Bash => home?.join(".bashrc"),
        Shell::Fish => var("XDG_CONFIG_HOME")
            .map_or_else(|| home.map(|h| h.join(".config")), Ok)?
            .join("fish/config.fish"),
    })
}

/// The line that puts `bin` first on PATH.
fn path_line(shell: Shell, bin: &Path) -> String {
    let dir = bin.display().to_string();
    match shell {
        Shell::Fish => {
            let quoted = dir.replace('\\', "\\\\").replace('\'', "\\'");
            format!("set -gx PATH '{quoted}' $PATH")
        }
        Shell::Zsh | Shell::Bash => {
            let mut quoted = String::new();
            for c in dir.chars() {
                if matches!(c, '\\' | '"' | '$' | '`') {
                    quoted.push('\\');
                }
                quoted.push(c);
            }
            format!("export PATH=\"{quoted}:$PATH\"")
        }
    }
}

/// `text` with the marked block (replacing any old one) at the end.
fn with_block(text: &str, line: &str) -> String {
    let mut out = without_block(text).unwrap_or_else(|| text.to_string());
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("{BEGIN}\n{line}\n{END}\n"));
    out
}

/// `text` without the marked block; `None` when it has none.
fn without_block(text: &str) -> Option<String> {
    let start = text.find(BEGIN)?;
    let end = text[start..]
        .find(END)
        .map(|i| start + i + END.len())
        .unwrap_or(text.len());
    let end = if text[end..].starts_with('\n') {
        end + 1
    } else {
        end
    };
    Some(format!("{}{}", &text[..start], &text[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_the_path_line() {
        assert_eq!(
            path_line(Shell::Zsh, Path::new("/h/.cargo/steward/bin")),
            "export PATH=\"/h/.cargo/steward/bin:$PATH\""
        );
        assert_eq!(
            path_line(Shell::Bash, Path::new("/a b/$x\"`")),
            "export PATH=\"/a b/\\$x\\\"\\`:$PATH\""
        );
        assert_eq!(
            path_line(Shell::Fish, Path::new("/it's")),
            "set -gx PATH '/it\\'s' $PATH"
        );
    }

    #[test]
    fn adds_replaces_and_removes_the_block() {
        let rc = "alias ll='ls -l'\nexport EDITOR=vi";
        let added = with_block(rc, "export PATH=\"/a:$PATH\"");
        assert_eq!(
            added,
            format!("{rc}\n{BEGIN}\nexport PATH=\"/a:$PATH\"\n{END}\n")
        );
        // Installing again replaces the block rather than adding another.
        let again = with_block(&added, "export PATH=\"/b:$PATH\"");
        assert_eq!(again.matches(BEGIN).count(), 1);
        assert!(again.contains("/b:") && !again.contains("/a:"));
        // Removal restores the rest, and only the block.
        let mut tail = again.clone();
        tail.push_str("export LATER=1\n");
        assert_eq!(
            without_block(&tail).unwrap(),
            format!("{rc}\nexport LATER=1\n")
        );
        assert_eq!(without_block(rc), None);
        assert_eq!(with_block("", "x"), format!("{BEGIN}\nx\n{END}\n"));
        // A block at the very end, without a final newline, goes too.
        assert_eq!(
            without_block(&format!("a\n{BEGIN}\nx\n{END}")).unwrap(),
            "a\n"
        );
    }
}
