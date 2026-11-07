# cargo-steward in CI

Two commands keep a cached or persistent build directory down to what your
jobs use:

```sh
cargo steward ci start                 # before the job builds
cargo steward ci trim --budget 8GiB    # the job's last step, before the cache is saved
```

`trim` removes, in this order:

1. build units that no build in the job used, since `start`;
2. the workspace's own crates, unless `--keep-workspace` (they change with most
   commits, so a cached copy rarely helps);
3. incremental caches, unless `--keep-incremental`;
4. object files that later compiles replaced (the [debug-object leak]);
5. then, if the build directory is still over `--budget`, the least recently
   used units that are left, until it's under.

It prints a summary, and when `$GITHUB_STEP_SUMMARY` is set (as it is on
GitHub Actions) adds a table of kept and removed units and sizes to the job's
summary page. `--dry-run` reports without removing anything, and `-v` lists
each item.

Both commands find the build directory with `cargo metadata` in the current
directory (`build-dir` if one is set, else the target directory), or take
`--target-dir DIR`. They handle the old build-directory layout and the one
Cargo 1.100 introduces (`build/<package>/<hash>/`), and a directory holding
both.

## GitHub-hosted runners with rust-cache

[Swatinem/rust-cache] restores the build directory at the start of a job and
saves it in a post step after the job's last step, unless the cache was
already up to date (a new `Cargo.lock` or toolchain changes its key). Before
saving it drops the workspace's crates (unless `cache-workspace-crates`),
incremental data, final outputs, and units of packages that are no longer
dependencies, matched by name. Everything else is saved again: old versions,
features, and profiles of packages that are still dependencies stay, so the
cache grows with each update until it hits GitHub's 10 GB per repository and
older caches start being evicted. `trim` keeps only the units the job used.

With `cache-workspace-crates: true`, pass `keep-workspace: true` to `trim`
too. rust-cache removes incremental data whatever `trim` keeps.

Composite actions have no post step, so `trim` has to be an ordinary step,
placed **last** in the job. Post steps run after every ordinary step, so it
always runs before rust-cache's save:

```yaml
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2           # restores now, saves in its post step
      - uses: doublesharp/cargo-steward@v0.1.0
        with:
          mode: start                          # after the restore, before any build
      - run: cargo build --all-targets
      - run: cargo test
      - uses: doublesharp/cargo-steward@v0.1.0
        with:
          mode: trim                           # last step: runs before rust-cache saves
          budget: 6GiB
```

- `start` must come after the cache is restored and before anything builds.
  Extracting the cache writes every file, and `start` resets what it needs
  afterwards.
- Leave `trim` without an `if:` (so it only runs when the job succeeds). A
  failed job stops partway, before it has used everything it would have, and
  rust-cache doesn't save after a failure by default anyway.
- Pick a budget so that every cache key you keep fits in 10 GB together.
  rust-cache compresses what it saves, and keeps one cache per OS, job, and
  toolchain.

With [actions/cache] instead, split it into `actions/cache/restore` and
`actions/cache/save`, and put `trim` just before the save step.

### Installing

`mode: start` installs cargo-steward when the matching version isn't already
on `PATH`. By default it builds the action's own checkout (the ref you wrote
after `@`) with `cargo install --locked --path`, which takes about a minute.
rust-cache caches binaries installed during a job (`cache-bin`, on by
default), so later runs find it and skip the build. Set `version` to install that release from
crates.io instead. To install it some other way, put it on `PATH` before the
`start` step.

The build uses the runner's default toolchain, not your workspace's
`rust-toolchain.toml`, and needs Rust 1.89 or newer.

### Inputs

| Input | Default | |
|---|---|---|
| `mode` | (required) | `start` or `trim` |
| `budget` | none | `trim`: largest size to leave, such as `8GiB` |
| `keep-workspace` | `false` | `trim`: keep the workspace's own crates |
| `keep-incremental` | `false` | `trim`: keep incremental caches |
| `target-dir` | from `cargo metadata` | the build directory |
| `working-directory` | `.` | where the Cargo workspace is |
| `version` | the action's ref | `start`: release to install from crates.io |

Linux and macOS runners only.

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

which goes by idle time and free disk space (see the README's settings), or
for each job's build directory, as its last step:

```yaml
      - run: cargo steward ci trim --keep-workspace --keep-incremental --budget 30GiB
```

On a persistent runner, keep the workspace's crates and incremental caches
(the next job rebuilds faster with them), and use `ci start` with `ci trim` only
where one workflow owns the build directory. A unit that another workflow
builds into the same directory, but this job didn't, counts as unused (see
below). Without `ci start`, `ci trim` applies only the budget.

## How "used this job" works

Cargo keeps a fingerprint for each unit, in `.fingerprint/<package>-<hash>/`
(old layout) or `build/<package>/<hash>/fingerprint/` (Cargo 1.100), and reads
it every time a build considers the unit, including when the unit is up to
date and nothing compiles. That read is the signal.

`ci start` writes `.cargo-steward-ci-start` at the root of the build directory
with the time, then sets the access time of every fingerprint file to the
Unix epoch, leaving modification times alone. An access time older than the
modification time is the case every common mount updates on the next read:
Linux `relatime` (the default) and `strictatime`, and macOS, whose APFS
behaves like `relatime`. So after the job, a unit whose fingerprint files were
neither read nor written since the start was in none of its builds. `trim`
removes those units, then gives the fingerprints of the units it keeps their
modification time as access time again, and removes the marker.

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
  look used. That errs on the side of keeping them.
- **Old-layout outputs whose fingerprint is gone** (left by another tool's
  cleanup) can't be dated reliably. Only the budget removes them.
- Final outputs that cargo copies up into the profile directory
  (`target/debug/<bin>`) and `target/doc` aren't units and are left alone.
  rust-cache removes the former before saving.

Removal always takes cargo's own locks on each profile directory first, so a
build still running there is never touched. A busy directory is skipped with
a warning.

[debug-object leak]: https://github.com/rust-lang/rust/issues/161824
[Swatinem/rust-cache]: https://github.com/Swatinem/rust-cache
[actions/cache]: https://github.com/actions/cache
