# Changelog

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.2.2] - 2026-11-09

### Fixed

- Cleanup counts only successfully removed superseded incremental sessions
  and their bytes. Failed deletions appear under kept paths with the error
  instead of inflating removal totals, including in JSON output.

## [0.2.1] - 2026-11-08

### Changed

- The GitHub Action downloads a checksum-verified prebuilt release by
  default and reuses a matching binary restored by rust-cache. Set
  `install-from-source: true` to build a development checkout or crates.io
  release; source mode always rebuilds, even with a matching version cached.

### Fixed

- Release archive and checksum downloads retry transient HTTP errors,
  including 504, with connection and request timeouts. GitHub Actions uses
  the authenticated asset API when `gh` is available, avoiding release-page
  download failures.

## [0.2.0] - 2026-11-08

### Added

- Optional `compile-memory-budget` admission alongside the compile count
  limit. Unknown units reserve `compile-memory-estimate` (2 GiB); direct
  compiler observations receive 25% headroom. Waiting uses kernel locks,
  with a turnstile that lets a large head waiter drain active compiles and
  a completion pipe that wakes it when any compile finishes.
  `CARGO_STEWARD_MEMORY_BUDGET=0` disables memory admission for one build.
- `stats [--target-dir DIR] [--json]` reports per-unit execution, queue,
  direct-compiler CPU time and peak RSS. `record-costs = false` disables
  observations; cloned builds retain estimates without inheriting counters.
- `ci trim --eviction rebuild-cost` retains expensive compiler CPU work per
  byte, including dependent work invalidated through Cargo fingerprints,
  using a median of recent direct-compiler observations. Unmeasured
  builds fall back to LRU; `--eviction lru` selects it explicitly.
  `--keep-package PACKAGE` retains selected workspace members as candidates
  for caching, still subject to job usage and the byte budget.
- GitHub Action inputs `eviction` and `keep-packages` expose the retention
  policy and selected workspace packages directly in workflows.

### Fixed

- Memory admission uses the configured unknown-unit estimate for unreadable
  or count-only reservations, and retries interrupted completion waits.
- CI workspace retention follows custom library and binary crate names
  recorded in dep-info when deciding which incremental caches belong to a
  package.
- Routine `status` no longer walks every scan root or every incremental
  session. It reports recorded build directories; `--discover` performs the
  full scan. Discovery uses directory-entry types instead of a metadata
  syscall for every file, including compiler-cache blobs.
- `doctor` reports sccache hit/miss counts, non-cacheable reasons and path
  normalization guidance, distinguishing incremental local builds from
  disposable CI builds.
- Compile-limit integration tests use a parent-controlled start/release
  barrier instead of a shared busy-directory marker, so fast compiler
  processes cannot make the overlap assertion depend on scheduling.
- Release builds use supported native macOS runners. A manual
  workflow run can rebuild binaries from an existing release tag without
  republishing the crate.

## [0.1.0] - 2026-11-08

### Added

- A crate-and-lightning logo and a reorganized README with installation,
  quick-start commands and expandable reference sections.
- With `build.build-dir` set, the `cargo` proxy also seeds a new worktree's
  build directory, from the build directory of the worktree its target
  directory is cloned from. It runs `cargo metadata` once, only when it
  seeds a new target directory.

### Changed

- Seeding starts a new worktree from the best build the repository has, not
  only the main checkout's: the main checkout's when it holds a build no
  build is using, else the most recently used other worktree's, trying each
  for half a second and all of them for up to 5 s before building cold.
  `seed` finds the other worktrees' build directories from the layout they
  share, the `cargo` proxy's `target-root` and the wrapper's records,
  without running cargo in each, and prints which one it used and why.
- `seed --from` can be given more than once; the first is preferred, then
  the most recently used.
- The `cargo` proxy seeds a new worktree when any other checkout of the
  repository has a target directory under `target-root`, not only the main
  checkout, and leaves the choice to `seed`: it runs `cargo-steward seed
  --target-dir <new> --worktree <dir> --quiet`. `shim explain` shows which
  directory it would seed from.

### Fixed

- On Linux, cleanup's own reads no longer count as a build using what it
  reads. Under `relatime`, reading a unit's fingerprint to see whether it's
  idle moved its access time, so idle units and caches always looked used and
  were never removed. Cleanup and `dedup` now read build output with
  `O_NOATIME`.
- The `cargo` proxy seeds from target directories it created itself. Cargo
  doesn't tag a directory it didn't create, and without its rustc cache file
  such a directory wasn't recognized as a build to clone, so the new worktree
  built cold.
- Cleanup no longer removes Cargo 1.100-layout build units that builds still
  use. Reading a unit doesn't move its times (APFS, `noatime`), so every
  dependency looked idle `unit-idle` after its last rebuild; units are now
  kept while anything used since was built from them, following cargo's
  fingerprints across profiles.
- The `cargo` proxy holds a build slot only while cargo builds. Programs a
  build left running (a server from `cargo run`, a test's daemon,
  `cargo watch`) kept the slot and blocked other builds; the waiting message
  now names who holds each slot.
- Low-space cleanup weighs build directories by their unshared size (clones
  from `seed` and `dedup` no longer look full size) and stops when removals
  stop raising free space (Time Machine local snapshots), instead of going on
  to every directory on the disk.
- What an interrupted cleanup renamed aside inside a build directory is
  finished by the next `gc --apply`; it was left forever. A preview no longer
  deletes anything, and trash a running cleanup is still deleting is left to
  it.
- Removing a whole build directory also locks profiles a build started after
  cleanup looked (`--release`, another `--target`).
- `status`, `doctor`, and the `gc` and `dedup` previews work when the steward
  home is read-only, as in an agent's sandbox; they no longer compact the
  ledger.
- A setting that doesn't parse falls back to its default alone, so a typo no
  longer drops `inner-wrapper` (sccache) and `auto-gc` with it; `doctor`
  reports bad and unknown settings, and other commands warn.
- Cleanup and `remove` delete the TMPDIR the proxy routed for a build
  directory they remove.
- Cleanup keeps what the wrapper recorded about build directories on a volume
  that isn't mounted.
- The wrapper lists a unit's objects only before incremental compiles, which
  are the only ones that leave stale objects: a full `deps/` listing per
  dependency compile is gone.
- Object names found across a read boundary are no longer recorded cut short.
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
- `ci trim --budget` weighs only what it can remove: it no longer evicts
  build units to make up for profiles a build is using, `doc/` or final
  outputs, and says when those leave the directory over the budget.
- `ci start` no longer creates a missing build directory, so cargo still
  creates it with `CACHEDIR.TAG` (and, on macOS, a Time Machine exclusion).
- `ci start` saves the fingerprint access times it resets, so a job that
  fails or is cancelled before `ci trim` no longer loses when its units were
  last used: `gc` goes by the saved times, and the next `ci start` (or
  `ci trim`) puts them back.

### Changed

- Cleanup lists each profile's units, incremental caches and fingerprints
  once, when it inspects the build directory, and checks again under cargo's
  locks only what it is about to remove; it reads fingerprints to find the
  units builds still need only when some unit is idle enough to go. On 40
  build directories, a cleanup that removes nothing takes about a third of
  the time it did.
- The deduplication after each automatic cleanup looks only at files
  created or replaced since its last pass (recorded in `dedup.stamp` in the
  steward home) and at the files at the same path in the other build
  directories, instead of reading every build directory again;
  `cargo steward dedup` still looks at every file, and reads build
  directories in parallel.
- While a disk is below `min-free` and cleanups free nothing on it, the
  wrapper waits longer before starting the next (5 minutes, doubling up to
  `gc-interval`), instead of starting one every 5 minutes; the wait resets
  once a cleanup frees space or the disk recovers.

### Added

- `--json` for `status`, `gc`, `remove`, `dedup`, `doctor` and
  `shim explain`: one JSON document on stdout, with sizes in bytes, RFC 3339
  times and stable ids for kinds and statuses, so scripts and agents don't
  parse text. `shim explain --json` includes the free space where the target
  directory goes. [docs/json.md](docs/json.md) describes each document.
- `max-compiles` setting (or `CARGO_STEWARD_MAX_COMPILES`): at most that
  many compiles run at once, machine-wide. The wrapper holds one of N lock
  file slots for as long as rustc runs, so concurrent builds from many
  worktrees stop oversubscribing CPU and memory. `doctor` shows the limit.
- Cleanup removes the build directory of a deleted worktree at once, without
  waiting out `orphan-grace`, when git confirms it: every workspace that built
  there was in a linked worktree whose admin directory git has removed. The
  wrapper records each workspace's worktree in the ledger for this; older
  ledgers and registries still read, and their directories keep the grace.
- `cargo steward remove --orphaned [--apply]` previews, then removes, every
  build directory whose recorded workspaces were all deleted, without waiting
  out `orphan-grace`, with the TMPDIRs the proxy routed for them. It takes
  cargo's locks and keeps directories a program runs from, as `remove` does;
  pinned ones need `--force`.
- Cleanup removes TMPDIRs the proxy routed (`<tmp-root>/<repo>/<worktree>`,
  under each configured `tmp-root`) whose build directory is gone, once
  nothing in them has changed for `orphan-grace`, such as those of worktrees
  deleted before cleanup removed them along with the build directory.
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

### Changed

- `object-grace` keeps replaced object files only while a program started
  from their unit's binary or test executable (or the copy cargo uplifts)
  runs, for up to that long; otherwise a rebuild keeps only the newest
  generation, so an edit-test loop no longer keeps every generation from the
  last 10 minutes. The wrapper and cleanup look for running programs only
  when there are replaced objects within the grace. With
  `process-guard = false`, the grace applies to every replaced object as
  before.
