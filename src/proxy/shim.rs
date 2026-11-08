//! `cargo steward shim`: installs the proxy as `cargo` in a bin directory,
//! puts that directory on PATH, and explains what the proxy would do.

use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::{Plan, gate, is_empty, real_cargo, settings, slot_dir, this_exe};
use crate::config::{self, Paths};
use crate::json;

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
        /// Print one JSON document instead of text
        #[arg(long)]
        json: bool,
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
        ShimCommand::Explain { args, json } => explain(args, *json),
    }
}

impl ShimCommand {
    /// Whether the command was asked for a JSON document.
    pub fn json(&self) -> bool {
        matches!(self, ShimCommand::Explain { json: true, .. })
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
    explain(&[], false)
}

/// What the proxy would do for a cargo command run here: what `explain`
/// shows, as text or JSON.
struct Explanation {
    /// The settings file, when there is a home to find it in.
    settings: Option<PathBuf>,
    /// Settings that can't be used, and so have their defaults.
    problems: Vec<String>,
    args: Vec<OsString>,
    plan: std::result::Result<Planned, String>,
    real_cargo: Option<PathBuf>,
    route: Option<PathBuf>,
    slot: Slot,
}

/// A plan and whether it seeds.
struct Planned {
    plan: Plan,
    seed: Seed,
}

/// Whether a build here starts its target dir as a clone of the main
/// checkout's, and if not, why not.
#[derive(Debug, Clone, PartialEq)]
enum Seed {
    Off,
    NothingRouted,
    TargetHasFiles,
    SourceMissing(PathBuf),
    Yes(PathBuf),
    NotLinked,
}

impl Seed {
    fn of(config: &config::Config, plan: &Plan) -> Seed {
        if !config.seed {
            Seed::Off
        } else if plan.skip.is_some() || plan.target.is_none() {
            Seed::NothingRouted
        } else if let Some(from) = &plan.seed_from {
            match &plan.target {
                Some(target) if !is_empty(target) => Seed::TargetHasFiles,
                _ if !from.is_dir() => Seed::SourceMissing(from.clone()),
                _ => Seed::Yes(from.clone()),
            }
        } else {
            Seed::NotLinked
        }
    }

    fn id(&self) -> &'static str {
        match self {
            Seed::Off => "off",
            Seed::NothingRouted => "nothing_routed",
            Seed::TargetHasFiles => "target_not_empty",
            Seed::SourceMissing(_) => "source_missing",
            Seed::Yes(_) => "will_seed",
            Seed::NotLinked => "not_linked_worktree",
        }
    }

    fn describe(&self) -> String {
        match self {
            Seed::Off => "off".into(),
            Seed::NothingRouted => "no: nothing routed".into(),
            Seed::TargetHasFiles => "no: the target dir already has files".into(),
            Seed::SourceMissing(from) => format!("no: {} doesn't exist yet", from.display()),
            Seed::Yes(from) => format!("yes, from {}", from.display()),
            Seed::NotLinked => "no: only builds in a linked worktree are seeded".into(),
        }
    }
}

/// Whether the command takes one of the build slots `max-builds` limits.
#[derive(Debug, Clone, PartialEq)]
enum Slot {
    NoLimit,
    Takes { limit: u32, dir: PathBuf },
    NotABuild { command: String },
}

impl Slot {
    fn id(&self) -> &'static str {
        match self {
            Slot::NoLimit => "no_limit",
            Slot::Takes { .. } => "takes_slot",
            Slot::NotABuild { .. } => "not_a_build",
        }
    }

    fn describe(&self) -> String {
        match self {
            Slot::NoLimit => "no limit (max-builds isn't set)".into(),
            Slot::Takes { limit, dir } => format!("takes one of {limit} (in {})", dir.display()),
            Slot::NotABuild { command } => format!("none needed: `{command}` doesn't build"),
        }
    }
}

impl Explanation {
    fn new(args: &[OsString]) -> Result<Explanation> {
        let args: Vec<OsString> = if args.is_empty() {
            vec!["build".into()]
        } else {
            args.to_vec()
        };
        let paths = Paths::resolve();
        let config = settings(paths.as_ref(), false);
        let problems = paths
            .as_ref()
            .map(|p| config::load_lenient(p).1)
            .unwrap_or_default();
        let cwd = env::current_dir()?;
        let plan = Plan::new(&config, &cwd, &args)
            .map(|plan| Planned {
                seed: Seed::of(&config, &plan),
                plan,
            })
            .map_err(|e| format!("{e:#}"));
        let me = this_exe();
        let command = gate::command(&args).unwrap_or_else(|| "(none)".into());
        let planned = plan.as_ref().ok().map(|p| &p.plan);
        let limit = planned.and_then(|p| p.max_builds).filter(|&n| n > 0);
        let set = planned.map(|p| p.slots.clone()).unwrap_or_default();
        let slot = match (limit, &paths) {
            (None, _) | (_, None) => Slot::NoLimit,
            (Some(limit), Some(paths)) if gate::is_build(&args) => Slot::Takes {
                limit,
                dir: slot_dir(paths, &set),
            },
            (Some(_), _) => Slot::NotABuild { command },
        };
        Ok(Explanation {
            settings: paths.as_ref().map(Paths::config),
            problems,
            real_cargo: real_cargo(&config, me.as_deref()),
            route: planned.and_then(|p| p.route.clone()),
            args,
            plan,
            slot,
        })
    }

    fn print(&self) {
        match &self.settings {
            Some(file) => {
                if file.exists() {
                    println!("settings:     {}", file.display());
                } else {
                    println!(
                        "settings:     {} (missing: defaults, nothing routed)",
                        file.display()
                    );
                }
                for problem in &self.problems {
                    println!("              {problem}");
                }
            }
            None => println!("settings:     none (no HOME): defaults"),
        }
        println!(
            "command:      cargo {}",
            self.args
                .iter()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ")
        );
        match &self.plan {
            Ok(planned) => describe(planned),
            Err(e) => println!("routing:      not routed: {e}"),
        }
        match &self.real_cargo {
            Some(real) => println!("real cargo:   {}", real.display()),
            None => println!("real cargo:   none found: the proxy can't run cargo"),
        }
        if let Some(route) = &self.route {
            println!("route:        {}", route.display());
        }
        println!("build slot:   {}", self.slot.describe());
    }

    fn document(&self) -> ExplainDoc {
        let planned = self.plan.as_ref().ok();
        let plan = planned.map(|p| &p.plan);
        ExplainDoc {
            version: json::VERSION,
            settings: SettingsDoc {
                path: self.settings.as_deref().map(json::path),
                exists: self.settings.as_ref().is_some_and(|f| f.exists()),
                problems: self.problems.clone(),
            },
            command: self
                .args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
            routing_error: self.plan.as_ref().err().cloned(),
            directory: plan.map(|p| json::path(&p.dir)),
            checkout: plan.map(|p| match &p.namespace.git {
                Some(git) => CheckoutDoc {
                    in_git: true,
                    linked: git.linked(),
                    worktree: git.worktree.as_deref().map(json::path),
                    git_dir: Some(json::path(&git.checkout)),
                },
                None => CheckoutDoc {
                    in_git: false,
                    linked: false,
                    worktree: None,
                    git_dir: None,
                },
            }),
            namespace: plan.map(|p| NamespaceDoc {
                repo: p.namespace.repo.clone(),
                key: p.namespace.key.clone(),
                path: json::path(&p.namespace.relative()),
            }),
            target_dir: plan.map(|p| RoutedDoc::of(p, &p.target, "target-root")),
            tmp_dir: plan.map(|p| RoutedDoc::of(p, &p.tmp, "tmp-root")),
            seed: planned.map(|p| SeedDoc {
                status: p.seed.id(),
                will_seed: matches!(p.seed, Seed::Yes(_)),
                from: p.plan.seed_from.as_deref().map(json::path),
                message: p.seed.describe(),
            }),
            route: self.route.as_deref().map(json::path),
            real_cargo: self.real_cargo.as_deref().map(json::path),
            build_slot: SlotDoc {
                status: self.slot.id(),
                limit: match &self.slot {
                    Slot::Takes { limit, .. } => Some(*limit),
                    _ => None,
                },
                dir: match &self.slot {
                    Slot::Takes { dir, .. } => Some(json::path(dir)),
                    _ => None,
                },
                message: self.slot.describe(),
            },
        }
    }
}

/// `shim explain --json`.
#[derive(Serialize)]
struct ExplainDoc {
    version: u32,
    settings: SettingsDoc,
    /// The cargo command's arguments.
    command: Vec<String>,
    /// Why nothing could be planned here; the plan's fields are null then.
    routing_error: Option<String>,
    directory: Option<String>,
    checkout: Option<CheckoutDoc>,
    namespace: Option<NamespaceDoc>,
    target_dir: Option<RoutedDoc>,
    tmp_dir: Option<RoutedDoc>,
    seed: Option<SeedDoc>,
    route: Option<String>,
    real_cargo: Option<String>,
    build_slot: SlotDoc,
}

#[derive(Serialize)]
struct SettingsDoc {
    path: Option<String>,
    exists: bool,
    problems: Vec<String>,
}

#[derive(Serialize)]
struct CheckoutDoc {
    in_git: bool,
    /// A linked worktree (`git worktree add`), not the main checkout.
    linked: bool,
    /// The working tree; null inside a git directory.
    worktree: Option<String>,
    git_dir: Option<String>,
}

#[derive(Serialize)]
struct NamespaceDoc {
    repo: String,
    key: String,
    /// `<repo>/<key>`, the part of routed paths below the roots.
    path: String,
}

#[derive(Serialize)]
struct RoutedDoc {
    routed: bool,
    /// Where it goes, when routed.
    path: Option<String>,
    /// Why not, when not.
    reason: Option<String>,
    /// When routed: free space on the disk that holds (or will hold) it.
    free_bytes: Option<u64>,
}

impl RoutedDoc {
    fn of(plan: &Plan, dir: &Option<PathBuf>, setting: &str) -> RoutedDoc {
        match (&plan.skip, dir) {
            (_, None) => RoutedDoc {
                routed: false,
                path: None,
                reason: Some(format!("{setting} isn't set")),
                free_bytes: None,
            },
            (Some(why), Some(_)) => RoutedDoc {
                routed: false,
                path: None,
                reason: Some(why.clone()),
                free_bytes: None,
            },
            (None, Some(dir)) => RoutedDoc {
                routed: true,
                path: Some(json::path(dir)),
                reason: None,
                free_bytes: crate::fsutil::free_space_near(dir),
            },
        }
    }
}

#[derive(Serialize)]
struct SeedDoc {
    /// "will_seed", "off", "nothing_routed", "target_not_empty",
    /// "source_missing" or "not_linked_worktree".
    status: &'static str,
    will_seed: bool,
    /// The main checkout's target dir, for a linked worktree.
    from: Option<String>,
    message: String,
}

#[derive(Serialize)]
struct SlotDoc {
    /// "no_limit", "takes_slot" or "not_a_build".
    status: &'static str,
    limit: Option<u32>,
    dir: Option<String>,
    message: String,
}

fn explain(args: &[OsString], as_json: bool) -> Result<()> {
    let explanation = Explanation::new(args)?;
    if as_json {
        json::print(&explanation.document())
    } else {
        explanation.print();
        Ok(())
    }
}

fn describe(planned: &Planned) {
    let plan = &planned.plan;
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
    println!("seed:         {}", planned.seed.describe());
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
