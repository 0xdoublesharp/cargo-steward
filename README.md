# cargo-steward

Keeps Cargo build output fast and bounded, so a machine that builds all day
(many worktrees, many agents, several toolchains) doesn't fill its disk or
slow down.

- **Stops the debug object-file leak.** Since Rust 1.88, every incremental
  rebuild on macOS (or anywhere with `split-debuginfo = "unpacked"`) leaves a
  full new set of `*.rcgu.o` / `*.rcgu.dwo` files behind and never removes the
  old ones ([rust-lang/rust#161824]). Build directories grow by hundreds of
  gigabytes, and every rustc start slows down listing them
  ([rust-lang/cargo#16665]). Cargo 1.101 fixes this for its own builds
  ([rust-lang/cargo#17675]); cargo-steward fixes it for every toolchain from
  1.88 up, including projects pinned to older ones.
- **Removes what no build uses any more**: object files earlier builds
  replaced, old-layout files after an upgrade to Cargo 1.100's new build
  directory layout, idle incremental caches and build units, whole build
  directories whose worktrees were deleted or that nothing has built in for
  weeks, and, when a disk runs low, the least recently used build
  directories.
- **Runs itself.** It's Cargo's `rustc-wrapper` (with sccache or any other
  wrapper kept behind it), and starts its cleanup in the background after
  builds. There is no daemon, no cron job and no launchd agent.
- **Checks your setup**: `cargo steward doctor` reports toolchains and pins in
  the leak window, configs that bypass the wrapper, and on macOS, scratch
  volumes that Time Machine backs up or Spotlight indexes.

Cargo's own answers are coming ([build-dir cleanup][rust-lang/cargo#5026],
[whole-directory cleanup][rust-lang/cargo#13136], [cross-workspace
deduplication][rust-lang/cargo#17453]) but haven't shipped. cargo-steward is
meant to step aside as each one does.

## Install

```sh
cargo install --path .            # from a checkout
cargo steward setup               # become Cargo's rustc-wrapper; turn on automatic cleanup
cargo steward doctor              # check everything
```

`setup` edits `~/.cargo/config.toml` (keeping its comments, and a backup next
to it): `build.rustc-wrapper` becomes cargo-steward, and the wrapper that was
there, such as `sccache`, becomes cargo-steward's inner wrapper. Undo it with
`cargo steward setup --undo`.

Build directories are found two ways: those the wrapper sees as you build, and
any under a scan root. Point it at wherever build output lives:

```sh
cargo steward setup --scan-root /Volumes/Scratch
```

Requires Rust 1.89 or newer to build; macOS and Linux.

## Commands

```sh
cargo steward status [--sizes]    # build directories, when each was last used, layout, size
cargo steward gc                  # what a cleanup would remove, by kind
cargo steward gc --apply [-v]     # remove it now
cargo steward pin <dir>           # never clean this build directory (except stale objects)
cargo steward unpin <dir>
cargo steward doctor
```

## What gets removed

Cheapest-to-rebuild first. Every threshold is a setting.

| What | When | Rebuild cost |
|---|---|---|
| Object files a later compile replaced | Right after the compile, by the wrapper; any left over, by cleanup | None. Nothing reads them. |
| Old-layout files (`deps/`, `.fingerprint/`, `build/<pkg>-<hash>/`) | Once Cargo 1.100's layout is in use in that directory and no older toolchain has built there for `legacy-idle` (3 days) | None |
| A crate's incremental cache | Unused for `incremental-idle` (7 days) | That crate compiles from scratch once |
| A build unit (new layout) | Unused for `unit-idle` (14 days) | That unit rebuilds if needed again |
| A whole build directory | Every workspace that built there is deleted, for `orphan-grace` (2 days); or unused for `dir-idle` (21 days) | Full rebuild |
| Whole build directories, least recently used first | The disk has under `min-free` (50 GiB) free; until it has `target-free` (100 GiB). Never one used within `recent-guard` (1 day) | Full rebuild |

Safety:

- Every removal from a build directory happens while holding cargo's own
  locks on it (`.cargo-lock` and `.cargo-build-lock`, exclusively), so a running
  build is never touched; a busy directory is skipped until next time.
- Directories are renamed aside before they're deleted, so nothing ever sees
  one half removed.
- Incremental caches are removed whole. Removing part of one can crash rustc.
- The wrapper only removes a compiled unit's own objects, only after rustc
  succeeds, and only ones the compile didn't rewrite. Binaries keep their debug
  info: the objects they reference are the ones kept.
- `gc` without `--apply` only reports.

Sizes are counted the way `du` counts them. APFS and btrfs clones share blocks
that each copy counts in full, so the disk's free space is the real measure.

## Settings

`~/.cargo/steward/config.toml` (or `$CARGO_HOME/steward/`, or
`$CARGO_STEWARD_HOME`):

```toml
inner-wrapper = "sccache"     # what rustc runs through; unset for none
leak-fix = true               # remove replaced object files after each compile
auto-gc = true                # clean up in the background after builds
gc-interval = "1h"            # at most this often (sooner when space is low)
scan-roots = ["/Volumes/Scratch"]
unit-idle = "14d"
incremental-idle = "7d"
legacy-idle = "3d"
dir-idle = "21d"
orphan-grace = "2d"
recent-guard = "1d"
min-free = "50GiB"
target-free = "100GiB"
```

`CARGO_STEWARD_INNER` overrides `inner-wrapper` (empty for none).

Automatic cleanups log to `~/.cargo/steward/gc.log`.

## How it works

Cargo runs `cargo-steward <rustc> <args>` for every compile. Before a compile
that writes objects, the wrapper lists the unit's existing `*.rcgu.o` files.
After rustc succeeds, it removes those the compile didn't rewrite. Each compile
gives all of its objects new names, including the ones it reuses from the
incremental cache, so anything with an old name and an old timestamp belongs
to an earlier compile. Calls that compile nothing (`rustc -vV`, `--print`) are
handed straight to rustc. Measured overhead is under a millisecond per
compile, and about 4 ms on cargo's cached version probe.

For workspace members, the wrapper also appends one line to a ledger: which
build directory, which package, how long the compile took. Cleanup compacts it
into a registry. That's how cleanup knows when a directory was last used, and
when every worktree that built into it is gone.

When a cleanup is due, the wrapper starts `cargo-steward gc --auto` in its own
session, at background CPU and I/O priority, with its output going to the log,
so cargo never waits for it.

## Build volume (macOS)

Build output on the system disk is caught by Time Machine's local snapshots,
which keep deleted files for about a day; Spotlight indexes it too. A separate
APFS volume in the same container shares free space with the system disk and
avoids both. It needs no repartitioning:

```sh
diskutil apfs addVolume disk3 APFS Scratch        # `diskutil list` shows your container
sudo mdutil -i off /Volumes/Scratch               # no Spotlight
sudo tmutil addexclusion -v /Volumes/Scratch      # no Time Machine
cargo steward setup --scan-root /Volumes/Scratch
```

Then send build output there, either with `CARGO_TARGET_DIR`, or for every
workspace at once:

```sh
cargo steward setup --build-dir /Volumes/Scratch/cargo-build
```

which sets `build.build-dir = "/Volumes/Scratch/cargo-build/{workspace-path-hash}"`:
one intermediate build directory per worktree, in one place, which IDEs and
rust-analyzer follow too.

## CI

On ephemeral runners with [Swatinem/rust-cache], the leak doesn't arise
(that action turns off incremental builds). On persistent self-hosted runners,
especially macOS ones, it does, and build directories grow without bound. There:

```sh
export RUSTC_WRAPPER=cargo-steward CARGO_STEWARD_INNER=sccache   # env outranks config in CI
cargo steward gc --apply                                         # e.g. as a nightly job
```

## Development

```sh
cargo test                     # unit and end-to-end tests (real cargo builds)
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The end-to-end tests build with the cargo running them. To check another
version or layout:

```sh
STEWARD_TEST_CARGO=$(rustup +beta which cargo) RUSTC=$(rustup +beta which rustc) cargo test --test e2e
```

[rust-lang/rust#161824]: https://github.com/rust-lang/rust/issues/161824
[rust-lang/cargo#16665]: https://github.com/rust-lang/cargo/issues/16665
[rust-lang/cargo#17675]: https://github.com/rust-lang/cargo/pull/17675
[rust-lang/cargo#5026]: https://github.com/rust-lang/cargo/issues/5026
[rust-lang/cargo#13136]: https://github.com/rust-lang/cargo/issues/13136
[rust-lang/cargo#17453]: https://github.com/rust-lang/cargo/issues/17453
[Swatinem/rust-cache]: https://github.com/Swatinem/rust-cache

## License

MIT. See [LICENSE](LICENSE).
