<p align="center">
  <img src="https://raw.githubusercontent.com/0xdoublesharp/cargo-steward/main/docs/assets/banner.png" alt="cargo-steward — Keep Cargo fast. Keep build output in check." width="100%">
</p>

<p align="center">
  <a href="https://github.com/0xdoublesharp/cargo-steward/actions/workflows/ci.yml"><img src="https://github.com/0xdoublesharp/cargo-steward/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://crates.io/crates/cargo-steward"><img src="https://img.shields.io/crates/v/cargo-steward?color=ed6839" alt="crates.io"></a>
  <a href="https://github.com/0xdoublesharp/cargo-steward/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-8ee6c0" alt="License: MIT"></a>
  <img src="https://img.shields.io/badge/platforms-macOS%20%7C%20Linux-b9c7d2" alt="Platforms: macOS and Linux">
</p>

<p align="center">
  <a href="#install">Install</a> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="#commands">Commands</a> ·
  <a href="#safety">Safety</a> ·
  <a href="#documentation">Documentation</a>
</p>

**Fast, bounded Cargo builds for people and AI agents working across many
git worktrees.** cargo-steward removes leaked debug objects, cleans unused
build output, gives new worktrees a warm start, and keeps expensive compiler
work in bounded CI caches.

It runs as Cargo's `rustc-wrapper`, keeping sccache or another wrapper
behind it. An optional `cargo` proxy routes build output and limits
concurrent builds. Automatic cleanup runs after builds, with no daemon,
cron job or launchd agent.

## What it does

| | What you get |
|---|---|
| **Smaller build directories** | Removes leaked debug objects after each compile on toolchains from 1.88 up, including pinned ones. Background cleanup removes replaced objects, idle caches and build units, old Cargo layouts, and deleted or long-idle worktrees' build directories. |
| **Warm worktrees** | Starts a new worktree as a copy-on-write clone of the best build among the repository's other worktrees. Shares identical files across build directories. [Warm starts →](docs/warm-start.md) |
| **Less contention** | Shares compile slots across builds, with optional estimated memory admission. Waiting compilers sleep until work finishes. The proxy's `max-builds` limits whole Cargo builds. |
| **Output where you want it** | Routes each worktree's build output and temporary files to a scratch volume. [Cargo proxy →](docs/proxy.md) |
| **Bounded CI caches** | Keeps the units a job used, then retains costly compiler work per byte, accounting for dependencies. Falls back to LRU without cost observations. [CI →](docs/ci.md) |
| **Build cost visibility** | `stats` reports per-unit execution and queue time, plus CPU and peak RSS for measured compilers. `status` quickly reads recorded build directories; discovery is opt-in. |
| **A setup check** | `doctor` finds bad settings, leaking or unused toolchains, wrapper overrides, split sccache caches, full dev debug info, extra rust-analyzer targets, and macOS backup or indexing overhead. |

When a disk runs low, cleanup prioritizes the build directories that cost
least to rebuild. [See what gets removed and when.](#what-gets-removed)

## Install

On **macOS or Linux**, with Rust 1.89 or newer:

```sh
cargo install --locked cargo-steward
cargo steward setup
cargo steward skill install
cargo steward doctor
```

<details>
<summary><strong>Install from source instead</strong></summary>

```sh
git clone https://github.com/0xdoublesharp/cargo-steward cargo-steward
cargo-steward/scripts/install.sh
```

The script installs the binary, then runs setup, installs the agent skill
and checks the result with `doctor`.

</details>

For **prebuilt binaries**, installation options, upgrading and uninstalling,
see the [installation guide](docs/install.md).

## Quick start

```sh
cargo steward setup               # become Cargo's rustc-wrapper; turn on automatic cleanup
cargo steward doctor              # check everything; it says what to change
cargo steward status              # recorded build directories; --discover searches for others
cargo steward stats               # per-unit execution, queue time and direct-compiler resources
cargo steward gc                  # preview a cleanup (`--apply` to run one now)
```

`setup` edits Cargo's user config (`~/.cargo/config.toml`, keeping its
comments and a backup next to it): `build.rustc-wrapper` becomes
cargo-steward, and the wrapper that was there, such as `sccache`, becomes
cargo-steward's inner wrapper. `cargo steward setup --undo` puts it back.

Before deleting a worktree, remove its build directory:

```sh
cargo steward remove --worktree ../app-fix-login --apply
git worktree remove ../app-fix-login
```

## Why build directories keep growing

- **Leaked debug objects.** With Rust 1.88 to 1.100, every incremental
  rebuild on macOS (or anywhere with `split-debuginfo = "unpacked"`) writes
  a full new set of `*.rcgu.o` / `*.rcgu.dwo` files and never removes the old
  ones ([rust-lang/rust#161824]). Build directories grow by hundreds of
  gigabytes, and every rustc start slows down listing them
  ([rust-lang/cargo#16665]). Cargo 1.101 removes them for its own builds
  ([rust-lang/cargo#17675]).
- **No cleanup.** Cargo doesn't remove what a build directory no longer
  needs ([rust-lang/cargo#5026]), or a build directory whose workspace is
  gone ([rust-lang/cargo#13136]), and each worktree keeps its own full copy
  of every dependency ([rust-lang/cargo#17453]).
- **Cold starts and contention.** A new worktree compiles every dependency
  from scratch, and ten agents each running `cargo test` with a job per core
  oversubscribe CPU and memory.

## Commands

| Command | What it does | Docs |
|---|---|---|
| `setup` | Makes cargo-steward Cargo's rustc-wrapper, turns on automatic cleanup and sets scan roots. `--build-dir` also sets `build.build-dir`; `--volume` creates a macOS build volume | [Installation](docs/install.md#first-time-setup) |
| `doctor` | Checks the setup and says what to change | |
| `status` | Quickly lists recorded build directories and their last observed compile; `--discover` scans for others and checks artifact use; `--sizes` measures disk use | |
| `stats` | Reports per-unit costs from the wrapper; `--target-dir` chooses a build directory and `--json` emits a machine-readable report | [JSON](docs/json.md#stats---json) |
| `gc` | Shows what a cleanup would remove, by kind (`-v`: every item); `--apply` removes it now | [Cleanup](#what-gets-removed) |
| `pin`, `unpin` | Keeps a build directory (named by any path inside it) out of cleanup, except its stale object files | |
| `remove` | Removes whole build directories: the paths given, the one each `--worktree` builds into, or all whose workspaces were deleted with `--orphaned`, without waiting out `orphan-grace`. Previews without `--apply`; pinned ones need `--force` | |
| `seed` | Clones the best available build from the repository's worktrees | [Warm starts](docs/warm-start.md#cargo-steward-seed) |
| `hooks` | Installs the git hook that seeds each new worktree | [Hooks](docs/warm-start.md#cargo-steward-hooks) |
| `dedup` | Replaces identical files across build directories with clones | [Deduplication](docs/warm-start.md#cargo-steward-dedup) |
| `ci` | Trims CI build output to what the job used, within a budget | [CI](docs/ci.md) |
| `shim` | Installs the `cargo` proxy; `status` and `explain` show what it does here | [Cargo proxy](docs/proxy.md) |
| `skill` | Installs the agent skill for Claude Code and Codex | [Agent skill](#agent-skill) |

<details>
<summary><strong>Full command reference</strong></summary>

```sh
cargo steward setup [--inner PROGRAM] [--scan-root DIR]... [--build-dir DIR]
                    [--no-auto-gc] [--volume NAME] [--undo] [--dry-run]
cargo steward doctor [--json]
cargo steward status [--discover] [--sizes] [--json]
cargo steward stats [--target-dir DIR] [--json]
cargo steward gc [--apply] [-v] [--json]
cargo steward pin DIR
cargo steward unpin DIR
cargo steward remove [BUILD_DIR]... [--worktree DIR]... [--orphaned]
                     [--apply] [--force] [--json]
cargo steward seed [--target-dir DIR] [--from DIR]... [--worktree DIR] [-q]
cargo steward hooks install|uninstall|status [--repo DIR]
cargo steward dedup [--apply] [-v] [--json]
cargo steward ci start [--target-dir DIR]
cargo steward ci trim [--target-dir DIR] [--budget SIZE] [--keep-workspace]
                      [--keep-package PACKAGE]... [--keep-incremental]
                      [--eviction rebuild-cost|lru] [--dry-run] [-v]
cargo steward shim install [--bin-dir DIR] [--shell zsh|bash|fish]
cargo steward shim uninstall [--bin-dir DIR]
cargo steward shim status
cargo steward shim explain [--json] [-- CARGO_ARGS...]
cargo steward skill install|uninstall [--dir DIR]...
cargo steward skill status|show
```

</details>

With `--json`, `status`, `stats`, `gc`, `remove`, `dedup`, `doctor` and
`shim explain` print one JSON document for scripts and agents;
[docs/json.md](docs/json.md) describes each one.

The reports (`status`, `stats`, `doctor`, `shim status`, `shim explain`, and `gc`,
`remove` and `dedup` without `--apply`) change nothing, and work where the
steward home is read-only, as in an agent's sandbox.

## What gets removed

Cheapest to rebuild first. Every threshold is a [setting](#settings).

| What (`--json` kind) | When | Rebuild cost |
|---|---|---|
| Object files a later compile replaced (`stale_objects`) | By the wrapper, right after each incremental compile; anything left over, by cleanup | None |
| Superseded incremental sessions (`old_sessions`) | Older sessions of an incremental cache, which rustc never loads again | None |
| Old-layout files: `deps/`, `.fingerprint/`, `build/<pkg>-<hash>/` (`legacy_layout`) | Once a profile also holds Cargo 1.100's layout and no older toolchain has built there for `legacy-idle` (3 days) | None |
| A crate's incremental cache (`idle_incremental`) | Unused for `incremental-idle` (7 days) | That crate compiles from scratch once |
| A Cargo 1.100-layout build unit (`idle_unit`) | Unused for `unit-idle` (14 days), and no unit used within that time was built from it | That unit rebuilds if needed again |
| A whole build directory whose workspaces were all deleted (`orphaned`) | At once when each was a linked worktree that git has removed (`git worktree remove` or `prune`); otherwise once unused for `orphan-grace` (2 days) | Full rebuild |
| A whole idle build directory (`idle`) | Unused for `dir-idle` (21 days) | Full rebuild |
| Whole build directories, for space (`low_space`) | A disk has under `min-free` (50 GiB) free: until it has `target-free` (100 GiB), or until removals stop raising free space | Full rebuild |
| A TMPDIR the proxy routed (`routed_tmp`, `leftover_tmp`) | With its build directory; or, when that build directory is gone, once nothing in it has changed for `orphan-grace` | None |

When a disk runs low, build directories go in order of idle time ×
unshared size ÷ recorded compile time, highest first: a big directory
nothing has used in weeks goes before a small one used yesterday, and one
that took an hour to build stays longer than one as big that took a minute.
Pinned directories and any used within `recent-guard` (1 day) are never
removed for space. When removals stop raising free space (Time Machine's
local snapshots or clones still hold the blocks), cleanup stops and says
so.

## Safety

- Every removal from a build directory happens while holding cargo's own
  locks on it (`.cargo-lock`, `.cargo-build-lock` and `.cargo-artifact-lock`,
  exclusively), and what was found unused is dated again under them. A
  directory a build is using is skipped until next time.
- Nothing a running program was started from (a test node, a server) is
  removed, whatever its age (`process-guard`). A build directory such a
  program runs from is cleaned inside instead of removed.
- Replaced object files from within `object-grace` (10 minutes) are kept
  while a program started from their unit's binary or test (or the copy cargo
  puts in the profile directory) runs, so it keeps symbolized backtraces.
  With `process-guard = false`, they're kept for `object-grace` either way.
- The wrapper removes only the objects of the unit it just compiled, only
  after rustc succeeds, and only ones the compile didn't rewrite. Cleanup
  keeps the objects each unit's outputs still reference, so binaries keep
  their debug info.
- Incremental caches and sessions are removed whole: removing part of one
  can crash rustc.
- Directories are renamed aside before they're deleted, so nothing sees one
  half removed. The next `gc --apply` finishes what an interrupted cleanup
  left.

Sizes are counted the way `du` counts them, so blocks that APFS or btrfs
clones share count in every copy. `status --sizes` also shows each
directory's UNSHARED size (what removing it alone frees), and `gc --apply`
reports the change in each disk's free space ("Freed on disk").

## Settings

Settings live in `config.toml` in the steward home: `$CARGO_STEWARD_HOME`,
else `$CARGO_HOME/steward`, else `~/.cargo/steward`.

<details>
<summary><strong>All settings and their defaults</strong></summary>

```toml
# inner-wrapper = "sccache"   # what rustc runs through; unset: rustc directly (setup keeps the wrapper it replaces)
leak-fix = true               # remove the object files each incremental compile replaces
auto-gc = false               # clean up in the background after builds (`setup` turns this on)
gc-interval = "1h"            # at most this often, or sooner when a disk is low on space
scan-roots = []               # directory trees searched for build directories the wrapper hasn't seen
unit-idle = "14d"             # remove a Cargo 1.100-layout build unit unused this long
incremental-idle = "7d"       # remove a crate's incremental cache unused this long
legacy-idle = "3d"            # remove old-layout files once no old-layout build has run for this long
dir-idle = "21d"              # remove a whole build directory unused this long
orphan-grace = "2d"           # remove a deleted workspace's build directory once unused this long
recent-guard = "1d"           # never remove for space a build directory used within this
min-free = "50GiB"            # when a disk has less free than this, remove build directories...
target-free = "100GiB"        # ...until it has this much (at least min-free)
object-grace = "10m"          # keep replaced objects up to this long while a binary built from them runs
process-guard = true          # never remove what a running program was started from
# max-compiles = 8            # most compiles at once, machine-wide; unset or 0: no limit
# compile-memory-budget = "32GiB" # optional estimated reservations, not an RSS cap
compile-memory-estimate = "2GiB"  # units whose compiler has not been measured
record-costs = true           # per-unit observations beside compiler output
seed = true                   # let `seed` (the hook and the proxy) clone build directories
dedup = true                  # on macOS, share identical files after each cleanup

# The cargo proxy (docs/proxy.md); with none of the roots set it just runs cargo:
# target-root = "/Volumes/Scratch/{label}/cargo"   # each worktree's target directory goes under this
# tmp-root = "/Volumes/Scratch/{label}/tmp"        # and its TMPDIR under this
# max-builds = 4                                   # most cargo builds at once; unset or 0: no limit
# real-cargo = "/path/to/cargo"                    # the cargo to run; searched for when unset
link-target = false                                # point a missing <workspace>/target at the routed directory
#
# [[routes]]                                       # per-directory overrides (docs/proxy.md#routes)
# path = "/Users/me/Workspaces/bigproject"
# target-root = "/Volumes/Scratch/bigproject/cargo"
# max-builds = 3
# force = true                                     # route even over a target dir chosen elsewhere
```

</details>

Durations are written like `10m`, `12h`, `14d` or `2w`. Sizes are bytes, or
take `K`, `M`, `G` or `T` (alone, or with `B` or `iB`), always binary:
`50GiB` and `50G` are the same.

A setting that can't be used (a wrong type, a duration or size that doesn't
parse, `target-free` below `min-free`) falls back to its default alone; the
others still apply. Commands print a warning for each, and `doctor` lists
them under Settings, along with keys that aren't settings (usually typos).
The wrapper and the proxy never fail a build over one. `setup` rewrites the
file with the settings it knows, stops at a setting of the wrong type, and
records what it replaced in Cargo's config (`previous-wrapper`,
`previous-build-dir`, `managed-build-dir`) for `--undo`.

Environment variables:

| Variable | Effect |
|---|---|
| `CARGO_STEWARD_HOME` | The steward home |
| `CARGO_STEWARD_INNER` | Overrides `inner-wrapper`; empty for none |
| `CARGO_STEWARD_MAX_COMPILES` | Overrides `max-compiles`; `0` or empty for no limit |
| `CARGO_STEWARD_MEMORY_BUDGET` | Overrides `compile-memory-budget`; `0` or empty disables memory admission |

## How it works

<details>
<summary><strong>Wrapper, compile slots, build records and background cleanup</strong></summary>

**The wrapper.** Cargo runs `cargo-steward <rustc> <args>` for every
compile. Calls that compile nothing (`rustc -vV`, `--print`) are handed
straight to rustc. Before an incremental compile, the wrapper lists the
unit's existing `*.rcgu.o` / `*.rcgu.dwo` files; after rustc succeeds, it
removes those the compile didn't rewrite. Each incremental compile gives all
of its objects new names, including the ones it reuses, so anything with an
old name and an old timestamp belongs to an earlier compile. When some of
those are from within `object-grace` and the unit has an executable, the
wrapper first looks for a program started from it (or from the copy cargo
puts in the profile directory), and keeps them while one runs. An inner
wrapper that isn't installed is skipped, so builds keep working without it.

When the process that started a compile exits first (Cargo killed alone by
a supervisor's timeout, say), nothing will use that compile's output. The
wrapper notices within a second and sends SIGTERM to the compile and every
process below it. A wrapper still waiting for a compile slot leaves without
starting its compile.

**Compile slots.** With `max-compiles = N`, at most N compiles run at once
on the machine, however many builds start them: several agents each running
cargo with a job per core share N rustc processes. Each compile holds a lock
on one of N files in `compile-slots/` in the steward home while its rustc
runs, and the system releases it however the compile ends. A compile that
finds every slot taken waits without printing anything, and one started
from inside another compile (by a proc macro, say) doesn't wait. The
proxy's `max-builds` limits whole cargo builds instead.

With `compile-memory-budget`, a compile also reserves estimated memory.
A compiler's measured peak RSS plus 25% supplies its next estimate;
unmeasured units use `compile-memory-estimate`. Through an inner cache
wrapper a compiler is measured only when the cache's client runs it itself,
as sccache does for units it can't cache, such as executables. Without an
explicit count limit, memory mode uses the logical CPU count. These are cooperative
reservations, not an RSS cap: linker processes can overlap, estimates can
be low, and builds with admission disabled are outside the budget.
An oversized compile runs alone. The head waiter blocks new memory-mode
admissions while current compiles finish. A completion pipe wakes it when
any compile finishes; a one-second recovery check handles crashed holders.
Other waiters block in the kernel. Nested compilers share a live enclosing lease.
Count and memory overrides are independent. Inner cache requests currently
take the same admission path, including cache hits.
With a memory budget enabled, `CARGO_STEWARD_MAX_COMPILES=0` removes the
explicit count override and memory mode uses the logical CPU count.

**Unit costs.** `stats` reads small `.cargo-steward-<hash>.json` observations
alongside build output. Execution includes an inner cache wrapper. CPU is
measured only for direct compiler children, including children they wait
for. RSS is the largest individual child peak, not their combined peak;
through a cache wrapper it is recorded only above `compile-memory-estimate`,
which the cache's own client never reaches.
Cloning output preserves estimates and resets checkout counters.
`record-costs = false` disables new observations. Cargo or other cache
cleaners may leave these metadata files behind; they are disposable hints.
During an upgrade, old wrappers can leave stale reservation fields; the
new scheduler treats them conservatively until those compiles finish.

**Records.** For each workspace member it compiles, the wrapper appends a
line to a ledger in the steward home: the build directory, the package, the
linked git worktree it's in (read from `.git`, without running git), and how
long the compile took. Cleanup compacts the ledger into a registry. That's
how it knows when a build directory was last used, what rebuilding it
costs, and when every worktree that built into it is gone. Once one is
gone, cleanup asks its repository: a linked worktree whose
`.git/worktrees/<name>` is gone was removed for good. Records of build
directories on a volume that isn't mounted are kept until it's back.

**Finding build directories.** Cleanup manages the build directories the
wrapper has recorded and any it finds under `scan-roots` (six levels deep).
A build directory's root is where cargo keeps `.rustc_info.json`, or its
`CACHEDIR.TAG` beside build profiles. Both of Cargo's layouts are handled,
and a directory holding both.

**When cleanup runs.** With `auto-gc` on, after a compile the wrapper
starts `cargo-steward gc --auto` when one is due: in its own session, at
background CPU and I/O priority, so cargo never waits for it. Only one
cleanup runs at a time, and automatic ones log to `gc.log` in the steward
home. A cleanup is due once `gc-interval` has passed since the last. When
the compile's disk is below `min-free`, one is due after 5 minutes instead;
while those cleanups free nothing, the wait doubles, up to `gc-interval`,
and it's back to 5 minutes once one frees space or no disk is low. On macOS
with `dedup = true`, each applied cleanup then shares identical files among
what changed since the last pass.

</details>

## Build volume (macOS)

Build output on the system disk is held for about a day after deletion by
Time Machine's local snapshots, and Spotlight indexes it.
`cargo steward setup --volume Scratch` creates a separate APFS volume in the
same container, which shares free space with the system disk and avoids
both, and `setup --build-dir` sends every workspace's build output there.
See [docs/install.md](docs/install.md#a-build-volume-macos).

## CI

On persistent self-hosted runners, make cargo-steward the wrapper and clean
up periodically. On runners that restore a cached build directory,
`cargo steward ci start` and `cargo steward ci trim` keep the cache to what
the job used. Both, and the GitHub Action, are in [docs/ci.md](docs/ci.md).

For GitHub Actions, restore the cache, run `mode: start`, build and test,
then run `mode: trim` before the cache is saved:

```yaml
- uses: Swatinem/rust-cache@v2
- uses: 0xdoublesharp/cargo-steward@v0.2.2
  with:
    mode: start
- run: cargo test --locked
  env:
    RUSTC_WRAPPER: cargo-steward
- uses: 0xdoublesharp/cargo-steward@v0.2.2
  with:
    mode: trim
    budget: 6GiB
```

`start` reuses a matching cached binary or downloads a checksum-verified
prebuilt release. Use `install-from-source: true` for development checkouts.

The wrapper records direct-compiler CPU samples for cost-aware retention.
An inner cache wrapper such as sccache keeps working, but its client timings
are not used as compiler costs. `eviction: lru` selects recency alone;
`keep-packages` selects workspace packages, one per line. See the
[complete workflow and retention options](docs/ci.md#github-hosted-runners-with-rust-cache).

## Agent skill

`cargo steward skill install` installs a skill (`SKILL.md`) that teaches AI
coding agents to build, clean up and retire worktrees with cargo-steward. It
goes into the skills directory of each agent found, Claude Code
(`$CLAUDE_CONFIG_DIR` or `~/.claude`) and Codex (`$CODEX_HOME` or
`~/.codex`), or into each `--dir DIR` given. The skill is built into the
binary: `skill status` shows whether each installed copy matches it, and
`skill install` updates them. `skill show` prints it, and `skill uninstall`
removes it.

## Documentation

| Guide | Covers |
|---|---|
| [Installation](docs/install.md) | Source and binary installs, setup, upgrades and uninstalling |
| [Warm starts](docs/warm-start.md) | Worktree seeding, git hooks and deduplication |
| [Cargo proxy](docs/proxy.md) | Routing output, per-project settings and build limits |
| [CI](docs/ci.md) | Trimming caches and using the GitHub Action |
| [JSON output](docs/json.md) | Machine-readable reports for scripts and agents |
| [Releasing](docs/releasing.md) | Versioning, CI, trusted publishing and the Action listing |
| [Changelog](CHANGELOG.md) | Release history |

## Development

With the toolchain pinned in `rust-toolchain.toml`:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
shellcheck scripts/install.sh
```

CI runs on macOS and Linux, with additional end-to-end checks against
Cargo beta and nightly.

[rust-lang/rust#161824]: https://github.com/rust-lang/rust/issues/161824
[rust-lang/cargo#16665]: https://github.com/rust-lang/cargo/issues/16665
[rust-lang/cargo#17675]: https://github.com/rust-lang/cargo/pull/17675
[rust-lang/cargo#5026]: https://github.com/rust-lang/cargo/issues/5026
[rust-lang/cargo#13136]: https://github.com/rust-lang/cargo/issues/13136
[rust-lang/cargo#17453]: https://github.com/rust-lang/cargo/issues/17453

## License

MIT. See [LICENSE](LICENSE).
