# Changelog

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

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
- `scripts/install.sh` for new machines (from source, git, or a release
  binary) and a release workflow building macOS and Linux binaries.
