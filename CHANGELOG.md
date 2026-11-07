# Changelog

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Fixed

- `ci trim --budget` weighs only what it can remove: it no longer evicts
  build units to make up for profiles a build is using, `doc/` or final
  outputs, and says when those leave the directory over the budget.
- `ci start` no longer creates a missing build directory, so cargo still
  creates it with `CACHEDIR.TAG` (and, on macOS, a Time Machine exclusion).

### Added

- `link-target` setting for the `cargo` proxy: when it routes a workspace's
  build output, it also points a missing `<workspace>/target` at the routed
  directory (only where git ignores `target`), so `./target` paths and builds
  that bypass the proxy land in the same place.
- `rustc-wrapper` mode that removes the codegen-unit object files each
  incremental rebuild replaces (rust-lang/rust#161824), for Rust 1.88 and
  later in both build-dir layouts, and runs rustc through an inner wrapper
  such as sccache. Replaced objects are kept for `object-grace` (10 minutes)
  so running binaries keep symbolized backtraces.
- `cargo steward gc`: removes stale object files, old-layout build files after
  an upgrade to Cargo 1.100, idle incremental caches and build units, build
  directories of deleted workspaces or unused for weeks, and, when a disk runs
  low, whole build directories cheapest-to-rebuild first. Reports unless given
  `--apply`, then shows the measured change in free space per disk. Directories
  a running program was started from are kept (`process-guard`). Scans run in
  parallel.
- Automatic background cleanup after builds, with no daemon; on macOS it also
  shares identical files across build directories as clones.
- Warm starts: `cargo steward seed` starts a new worktree's build directory as
  a copy-on-write clone of its main one; `cargo steward hooks` installs a git
  `post-checkout` hook that does it for every new worktree.
- `cargo steward dedup`: replaces identical files across build directories with
  clones (APFS, btrfs, XFS, bcachefs).
- The `cargo` proxy (`cargo steward shim install|uninstall|status|explain`):
  routes each worktree's target directory and TMPDIR under a root, seeds new
  worktrees, and limits concurrent builds with build slots. `[[routes]]` give
  directory trees their own roots and limits, and `{label}` in a root groups
  output per project. Its naming matches existing per-worktree shims.
- CI support: `cargo steward ci start|trim` trims a job's build output to the
  units it used and to a size budget, with a composite GitHub Action.
- `cargo steward setup` (`--undo`, `--volume NAME`, automatic scan roots),
  `doctor` (wrapper chain, toolchains in the leak window and unused ones,
  sccache layout, dev-profile debug info, Time Machine and Spotlight,
  rust-analyzer target dirs, binary health), `status` (with unshared sizes),
  `pin` and `unpin`.
- `cargo steward remove`: removes whole build directories (or a retiring
  worktree's, with `--worktree`) the way cleanup does: under cargo's locks,
  not while a program runs from them, never anything that isn't a build
  directory, and pinned ones only with `--force`.
- An agent skill (`skills/cargo-steward/SKILL.md`, compiled in) that teaches
  AI coding agents to maintain Rust workspaces with cargo-steward;
  `cargo steward skill install|uninstall|status|show` installs it for Claude
  Code and Codex, and `scripts/install.sh` does so after setup.
- Cleanup removes superseded incremental sessions, which rustc otherwise keeps
  until that crate's next compile.
- `scripts/install.sh` for new machines (from source, git, or a release
  binary) and a release workflow building macOS and Linux binaries.

### Fixed

- `doctor` quotes scan roots in its `tmutil addexclusion` and `mdutil -i off`
  fix commands, so they work for volume names with spaces.
- `scripts/install.sh` runs `cargo install` without the configured
  `rustc-wrapper`, so it can reinstall cargo-steward when the binary that
  wrapper names is missing and every other build fails.
- `setup` and `doctor` read the user-wide Cargo config Cargo reads: the
  extensionless `$CARGO_HOME/config` whenever it exists, even next to a
  `config.toml`. With only that file, `doctor` reported no wrapper and
  skipped the missing-binary check, and flagged `~/.cargo/config` as a
  project that bypasses cargo-steward.
- Writing a settings or Cargo config file through a symlink (as dotfile
  managers set up) updates the file it links to instead of replacing the
  link, and keeps the file's permissions; `setup`'s backup of Cargo's config
  gets the original's permissions instead of 0644.
- `setup` records cargo-steward as installed on PATH (such as Homebrew's or
  Nix's link) rather than the versioned file it resolves to, which an upgrade
  removes and then every build fails.
- `setup` keeps the other keys of an inline `build = { ... }` in Cargo's
  config (it replaced the table with an empty one, losing `jobs`,
  `target-dir` and the rest, in setup and `--undo` alike), and refuses a
  `build` that isn't a table instead of overwriting it.
- `setup` stops when Cargo's config exists but can't be read (permissions,
  not UTF-8) instead of treating it as empty and replacing it with no backup.
- `setup --undo` puts back the `build.rustc-wrapper` and `build.build-dir`
  that setup replaced, recorded in the new `previous-wrapper` and
  `previous-build-dir` settings. It used to set the inner wrapper as the
  wrapper even when that came from `--inner` and was never configured, and
  removed a `build-dir` that `--build-dir` had overwritten. Settings written
  by earlier versions are undone as before.
