# Installing cargo-steward

cargo-steward runs on macOS and Linux. It's one binary, `cargo-steward`,
which Cargo runs as `cargo steward` when it's on `PATH` (normally in
`~/.cargo/bin`). Its settings and records live in the steward home:
`$CARGO_STEWARD_HOME`, else `$CARGO_HOME/steward`, else `~/.cargo/steward`.

## Install

`scripts/install.sh` installs the binary, then runs `cargo steward setup`,
`cargo steward skill install` (when Claude Code or Codex is installed) and
`cargo steward doctor`. Options it doesn't know go to `setup`; put them
after `--` when they could be mistaken for the script's own.

**From a checkout** (needs Rust 1.89 or newer):

```sh
git clone https://github.com/0xdoublesharp/cargo-steward cargo-steward
cargo-steward/scripts/install.sh
cargo-steward/scripts/install.sh --scan-root /Volumes/Scratch   # passing an option to setup
```

This runs `cargo install --locked --path` on the checkout the script is in.
`--from-source DIR` installs another one.

**From git**, without a checkout:

```sh
scripts/install.sh --git https://github.com/0xdoublesharp/cargo-steward [--rev <commit>]
# or by hand, then run `cargo steward setup`:
cargo install --locked --git https://github.com/0xdoublesharp/cargo-steward cargo-steward
```

**A prebuilt binary** (no Rust toolchain needed):

```sh
scripts/install.sh --release 0.2.4 --repo 0xdoublesharp/cargo-steward   # or --release latest
```

This downloads `cargo-steward-<target>.tar.gz` for this machine
(`aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu` or
`aarch64-unknown-linux-gnu`) from that GitHub repository's release, checks
it against the release's `.sha256` file, and installs it to
`${CARGO_HOME:-~/.cargo}/bin`. `CARGO_STEWARD_RELEASE_URL` replaces
`https://github.com/OWNER/NAME/releases`, for a mirror.

`--no-setup` installs the binary and runs `doctor`, nothing else.

## First-time setup

`cargo steward setup` (which the install script runs):

1. Makes cargo-steward the `build.rustc-wrapper` in Cargo's user config
   (`$CARGO_HOME/config.toml`), keeping its comments and writing a backup
   next to it. A wrapper that was already there, such as `sccache`, becomes
   cargo-steward's inner wrapper; `--inner PROGRAM` names another.
2. Turns on automatic cleanup after builds (`--no-auto-gc` leaves it off).
3. Sets scan roots: directory trees searched for build directories the
   wrapper hasn't seen build. When none are set, it adds where build output
   already goes: the parent of `CARGO_TARGET_DIR`, the fixed part of
   `build.build-dir` or `build.target-dir` in Cargo's user config, and
   volumes other than the home directory's that hold build directories it
   has recorded. It prints what it added and why. `--scan-root DIR` adds
   more (repeat it for several).

`--build-dir DIR` also sets `build.build-dir` to
`DIR/{workspace-path-hash}`: one intermediate build directory per
workspace, all in one place, which IDEs and rust-analyzer follow too.

`setup --dry-run` shows the changes without making them. Running `setup`
again keeps your settings, except that it turns automatic cleanup back on
unless given `--no-auto-gc`. Run `cargo steward doctor` afterwards; it
lists anything left to fix.

`RUSTC_WRAPPER` or `CARGO_BUILD_RUSTC_WRAPPER` set in the environment
outranks Cargo's config. `setup` and `doctor` warn when one is set to
something else: point it at `cargo-steward` and move the old value to
`CARGO_STEWARD_INNER`.

### A build volume (macOS)

Build output on the system disk is held for about a day after deletion by
Time Machine's local snapshots, and Spotlight indexes it. A separate APFS
volume in the same container avoids both and shares the disk's free space,
with no repartitioning:

```sh
cargo steward setup --volume Scratch --dry-run   # see the plan
cargo steward setup --volume Scratch
```

This finds the system disk's APFS container, runs
`diskutil apfs addVolume <container> APFS Scratch`, and adds
`/Volumes/Scratch` as a scan root. A volume that already exists is used as
is. Two steps need sudo, so `setup` prints them for you to run:

```sh
sudo mdutil -i off /Volumes/Scratch            # no Spotlight
sudo tmutil addexclusion -v /Volumes/Scratch   # no Time Machine
```

Then send build output there, with `CARGO_TARGET_DIR` per shell or project,
with the [`cargo` proxy](proxy.md)'s `target-root`, or for every workspace
at once:

```sh
cargo steward setup --build-dir /Volumes/Scratch/cargo-build
```

Build directories that stay on the system disk should at least be out of
Time Machine. Cargo excludes target directories it creates, but not ones a
script created first; `doctor` lists those, with the `tmutil addexclusion`
command for each.

On Linux, put build output on a separate filesystem yourself and pass its
mount point with `--scan-root`.

## Upgrade

Install again the same way, adding `--no-setup`, then update the agent
skill:

```sh
scripts/install.sh --no-setup                                   # from an updated checkout
scripts/install.sh --release latest --repo 0xdoublesharp/cargo-steward --no-setup
cargo steward skill install
```

Both replace the binary in place, and Cargo's config already points at that
path, so the next build uses the new version. The script ends with
`doctor`, which checks that the wrapper Cargo runs is the version just
installed. The [`hooks`](warm-start.md#cargo-steward-hooks) call
cargo-steward by the path they were installed from; if that path changes,
run `hooks install` again in each repository.

## Uninstall

Each piece has its own command. Run them in this order, while the binary is
still installed:

```sh
cargo steward setup --undo        # restore the previous rustc-wrapper and build-dir; turn off automatic cleanup
cargo steward hooks uninstall     # in each repository with the worktree hook
cargo steward shim uninstall      # the cargo proxy, and its PATH lines
cargo steward skill uninstall     # the agent skill
cargo uninstall cargo-steward     # or rm ~/.cargo/bin/cargo-steward for a prebuilt binary
rm -rf ~/.cargo/steward           # the steward home: settings, records, the cleanup log
```

`hooks status`, `shim status` and `skill status` show whether those are
installed. `setup --undo` puts back the `build.rustc-wrapper` and
`build.build-dir` that setup replaced, and removes them where there were
none. It leaves build directories alone: run `cargo steward gc --apply` or
`cargo steward remove` before uninstalling, or delete them afterwards.
