# cargo-steward in CI

Two commands keep a cached or persistent build directory down to what your
jobs use:

```sh
cargo steward ci start                 # before the job builds
cargo steward ci trim --budget 8GiB    # the job's last step, before the cache is saved
```

`trim` removes:

1. build units that no build in the job used since `start`;
2. the workspace's own crates, unless `--keep-workspace` or a matching
   `--keep-package PACKAGE` keeps them as cache candidates;
3. incremental caches, unless `--keep-incremental`;
4. object files that later compiles replaced (the [debug-object leak]);
5. then, if the units that are left still come to more than `--budget`, the
   units with the cheapest observed rebuild impact per byte, until they fit.
   Without observations this uses LRU; `--eviction lru` always uses LRU.

The budget weighs only what `trim` can remove. Profiles a running build holds,
`target/doc`, and final outputs cargo copies up into the profile directory
stay whatever the budget, and no unit is removed to make up for them; when
they leave the build directory over the budget, `trim` says so.

It prints a summary, and when `$GITHUB_STEP_SUMMARY` is set (as it is on
GitHub Actions) adds a table of kept and removed units and sizes to the job's
summary page. `--dry-run` reports without removing anything, and `-v` lists
each item.

Both commands find the build directory with `cargo metadata` in the current
directory (`build-dir` if one is set, else the target directory), or take
`--target-dir DIR`. They handle the build-directory layout before Cargo
1.100, Cargo 1.100's (`build/<package>/<hash>/`), and a directory holding
both.

## GitHub-hosted runners with rust-cache

[Swatinem/rust-cache] restores the build directory at the start of a job and
saves it in a post step after the job's last step, unless the cache was
already up to date (a new `Cargo.lock` or toolchain changes its key). Before
saving it drops the workspace's crates (unless `cache-workspace-crates`),
incremental data, final outputs, and units of packages that are no longer
dependencies, matched by name. Everything else is saved again: old versions,
features, and profiles of packages that are still dependencies stay, so the
cache grows until it reaches the repository's configured cache allowance
and older caches start being evicted. `trim` keeps only the units the job used.

With `cache-workspace-crates: true`, pass `keep-workspace: true` to `trim`
too. rust-cache removes incremental data whatever `trim` keeps.

Composite actions have no post step, so `trim` has to be an ordinary step,
placed **last** in the job. Post steps run after every ordinary step, so it
always runs before rust-cache's save:

```yaml
name: Rust
on: [push, pull_request]

permissions:
  contents: read

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2           # restores here, saves in its post step
      - uses: 0xdoublesharp/cargo-steward@v0.2.0
        with:
          mode: start                          # after the restore, before any build
      - run: cargo test --locked
        env:
          RUSTC_WRAPPER: cargo-steward            # records direct compiler costs
      - run: cargo steward stats
      - uses: 0xdoublesharp/cargo-steward@v0.2.0
        with:
          mode: trim                           # last step: runs before rust-cache saves
          budget: 6GiB
```

- `start` must come after the cache is restored and before anything builds.
  Extracting the cache writes every file, and `start` resets what it needs
  afterwards.
- Leave `trim` without an `if:`, so it runs only when the job succeeds. A
  failed job stops partway, before it has used everything it would have, and
  rust-cache doesn't save after a failure by default anyway. Nothing is lost
  by skipping it: the next `start` puts back the access times this one saved
  ([below](#how-used-this-job-works)).
- Pick a budget so every cache key you keep fits in the repository's cache
  allowance together.
  rust-cache compresses what it saves, and keeps one cache per OS, job, and
  toolchain.

With [actions/cache] instead, split it into `actions/cache/restore` and
`actions/cache/save`, and put `trim` just before the save step.

The `start` step installs the CLI; `RUSTC_WRAPPER` belongs on build/test steps
after it. To preserve sccache, also set `CARGO_STEWARD_INNER: sccache` on those
steps. CPU and RSS observations then remain unknown unless the cache already
contains direct-compiler samples. Unmeasured compiles use the fallback memory estimate.
Existing caches without CPU samples use LRU until direct compiles record some.

To retain selected workspace libraries, allow rust-cache to save workspace
crates and let cargo-steward select them:

```yaml
- uses: Swatinem/rust-cache@v2
  with:
    cache-workspace-crates: true
# ... cargo-steward start, then build/test ...
- uses: 0xdoublesharp/cargo-steward@v0.2.0
  with:
    mode: trim
    budget: 6GiB
    keep-packages: |
      protocol-core
      shared-types
    eviction: rebuild-cost
```

The names are Cargo package names, including packages with custom `[lib]`
crate names. Other workspace packages are removed. Selected packages still
have to be used in the job and fit the byte budget. For a nested workspace,
set `working-directory` on both Action steps and the Cargo run steps.

### Installing

`mode: start` installs cargo-steward when the matching version isn't already
on `PATH`. By default it builds the action's own checkout (the ref you wrote
after `@`) with `cargo install --locked --path`, which takes about a minute.
rust-cache caches binaries installed during a job (`cache-bin`, on by
default), so later runs find it and skip the build. Set `version` to
install that release from crates.io instead. To install it some other way,
put it on `PATH` before the `start` step.

To avoid compiling the CLI on a cold runner, install its prebuilt release
before `mode: start`. The installer verifies the archive's SHA-256 and
`--no-setup` keeps the runner's Cargo configuration intact:

```yaml
- name: Install prebuilt Cargo Steward
  shell: bash
  run: |
    installer="$RUNNER_TEMP/install-cargo-steward.sh"
    curl -fsSL https://raw.githubusercontent.com/0xdoublesharp/cargo-steward/v0.2.0/scripts/install.sh -o "$installer"
    sh "$installer" --release 0.2.0 --repo 0xdoublesharp/cargo-steward --no-setup
```

The Action detects the matching binary and skips its source build.

The build uses the runner's default toolchain, not your workspace's
`rust-toolchain.toml`, and needs Rust 1.89 or newer.

### Inputs

| Input | Default | |
|---|---|---|
| `mode` | (required) | `start` or `trim` |
| `budget` | none | `trim`: most to keep of what it can remove, such as `8GiB` |
| `keep-workspace` | `false` | `trim`: keep the workspace's own crates |
| `keep-incremental` | `false` | `trim`: keep incremental caches |
| `keep-packages` | empty | `trim`: workspace package names, one per line; usage and budget still apply |
| `eviction` | `rebuild-cost` | `trim`: compiler cost and dependency impact per byte, or `lru` |
| `target-dir` | from `cargo metadata` | the build directory |
| `working-directory` | `.` | where the Cargo workspace is |
| `version` | the action's ref | `start`: release to install from crates.io |

Linux and macOS runners only.

## Choosing what to retain

`cargo steward ci trim --budget 8GiB` defaults to `--eviction rebuild-cost`.
Where the wrapper recorded direct-compiler CPU samples, it removes cheaper
CPU work per byte first. It uses the median of nine recent samples so a
contended build does not permanently dominate retention. For up to 4,096
candidate units, it follows Cargo fingerprint dependencies and charges an
eviction for retained dependent work it would invalidate. Larger inventories
use cost per byte without that graph to bound memory and planning time.
Unmeasured units receive the median observed cost per byte; with no measurements, eviction
uses least recently used order. `--eviction lru` selects that order explicitly.

`--keep-package my-library` can be repeated to retain selected workspace
members as cache candidates. Job usage and the byte budget still apply.
`--keep-workspace` retains all workspace members as candidates.

The Action's start/trim steps do not enable the rustc wrapper. Existing
build caches without direct-compiler observations use LRU. Execution time
from an inner cache client is reported by `stats`, but is not treated as
compiler CPU cost. These options are available through the CLI.

This policy learns observed compiler costs and uses Cargo's fingerprint
dependency graph. It does not learn access frequency or predict source
change rates. Cargo remains responsible for deciding whether an artifact is
fresh; retention never allows stale compiler output to pass as fresh.

## Persistent self-hosted runners

A runner that keeps its build directories between jobs (particularly macOS,
where every incremental rebuild leaks a set of debug object files) grows
without bound. Two things fix that.

Make cargo-steward the wrapper, so each compile removes the objects it
replaced. Set it in the runner's environment (its `.env` file) or the
workflow's `env:`. Environment variables outrank cargo config files, and
`CARGO_STEWARD_INNER` keeps sccache, or whatever wrapper you used, behind it:

```sh
RUSTC_WRAPPER=cargo-steward
CARGO_STEWARD_INNER=sccache
```

Then clean up periodically, either for the whole machine:

```sh
cargo steward gc --apply          # e.g. a scheduled workflow, or cron
```

which goes by idle time and free disk space
([README](../README.md#what-gets-removed)), or for each job's build
directory, as its last step:

```yaml
      - run: cargo steward ci trim --keep-workspace --keep-incremental --budget 30GiB
```

On a persistent runner, keep the workspace's crates and incremental caches
(the next job rebuilds faster with them), and use `ci start` with `ci trim`
only where one workflow owns the build directory: a unit that another
workflow builds into the same directory, but this job didn't, counts as
unused. Without `ci start`, `ci trim` applies only the budget.

## How "used this job" works

Cargo keeps a fingerprint for each unit, in `.fingerprint/<package>-<hash>/`
(before Cargo 1.100) or `build/<package>/<hash>/fingerprint/` (Cargo 1.100),
and reads it every time a build considers the unit, including when the unit
is up to date and nothing compiles. That read is the signal.

`ci start` writes `.cargo-steward-ci-start` at the root of the build directory
with the time, saves when each unit's fingerprint was last used to
`.cargo-steward-ci-atimes` next to it, then sets the access time of every
fingerprint file to the Unix epoch, leaving modification times alone. An
access time older than the modification time is the case every common mount
updates on the next read: Linux `relatime` (the default) and `strictatime`,
and macOS, whose APFS updates an access time only when it's older than the
modification time. So after the job, a unit whose fingerprint files were
neither read nor written since the start was in none of its builds. `trim`
removes those units, puts back the saved access times of the fingerprints no
build read, and removes both files.

A job that fails or is cancelled before `trim` leaves the reset access times
and the saved ones. Until a `start` or `trim` puts them back (the next `start`
does so before saving them again), `cargo steward gc` goes by the saved
times, so a unit the job didn't reach doesn't look unused since it was built.

When the build directory doesn't exist yet, `start` leaves it alone: cargo
marks a build directory as a cache (`CACHEDIR.TAG`, and on macOS a Time
Machine exclusion) only when it creates it. Everything the job then builds is
its own, so `trim` has nothing to tell apart and applies only the budget.

Before resetting anything, `start` checks that this filesystem updates access
times: it creates a file, sets its access time to the epoch, reads it, and
looks. The result goes in the marker (`atime=updates` or `atime=frozen`). On a
`noatime` mount, `start` leaves access times alone, and `trim` says so and
applies only the budget.

Limits:

- **A build that didn't run doesn't count.** Units that only another workflow,
  a skipped step, or the part of a job after a failure would use are removed.
- **Resolution is one second.** Anything touched in the second `start` ran
  counts as used.
- **Anything else that reads the files counts as use.** A virus scanner, an
  indexer, or a backup reading the build directory during the job makes units
  look used, so they're kept.
- **Old-layout outputs whose fingerprint is gone** (left by another tool's cleanup)
  can't be dated. Only the budget removes them.
- Final outputs that cargo copies up into the profile directory
  (`target/debug/<bin>`) and `target/doc` aren't units and are left alone.
  rust-cache removes the former before saving.

Removal always takes cargo's own locks on each profile directory first, so a
build still running there is never touched. A busy directory is skipped with
a warning.

[debug-object leak]: https://github.com/rust-lang/rust/issues/161824
[Swatinem/rust-cache]: https://github.com/Swatinem/rust-cache
[actions/cache]: https://github.com/actions/cache
