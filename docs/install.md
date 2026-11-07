# Installing cargo-steward

cargo-steward runs on macOS and Linux. It's one binary, `cargo-steward`,
which Cargo finds as `cargo steward` when it's on `PATH` (normally in
`~/.cargo/bin`). Its settings and records live in `~/.cargo/steward`.

## Install

`scripts/install.sh` installs the binary, then runs `cargo steward setup` and
`cargo steward doctor`. Options it doesn't know are passed to `setup`.

**From a checkout** (needs Rust 1.89 or newer):

```sh
git clone <repository URL> cargo-steward
cargo-steward/scripts/install.sh
```

This runs `cargo install --locked --path` on the checkout. Pass
`--from-source DIR` to install a different one.

**From git**, without a checkout:

```sh
scripts/install.sh --git <repository URL> [--rev <commit>]
# or by hand:
cargo install --locked --git <repository URL> cargo-steward
```

**A prebuilt binary** (no Rust toolchain needed to install):

```sh
scripts/install.sh --release 0.1.0 --repo OWNER/cargo-steward   # or --release latest
```

This downloads `cargo-steward-<target>.tar.gz` for this machine (the target
comes from `uname`: `aarch64-apple-darwin`, `x86_64-apple-darwin`,
`x86_64-unknown-linux-gnu` or `aarch64-unknown-linux-gnu`), checks it against
the release's `.sha256` file, and installs it to `${CARGO_HOME:-~/.cargo}/bin`.
Releases are built by `.github/workflows/release.yml` when a `v*` tag is
pushed.

`--no-setup` installs without running `setup`.

## First-time setup

`setup` (which the install script runs) does three things:

1. Makes cargo-steward Cargo's `rustc-wrapper` in `~/.cargo/config.toml`,
   keeping comments and a backup. A wrapper that was already there, such as
   `sccache`, becomes cargo-steward's inner wrapper.
2. Turns on automatic cleanup after builds.
3. Sets scan roots: the directories searched for build directories the
   wrapper hasn't seen. Given none, it adds where build output already goes:
   the parent of `CARGO_TARGET_DIR`, the fixed part of `build.build-dir` or
   `build.target-dir` in Cargo's user config, and volumes other than the home
   directory's that hold build directories it has recorded. It prints what it
   added and why. Add others with `--scan-root DIR`.

`setup --dry-run` shows the changes without making them. Run
`cargo steward doctor` afterwards; it lists anything left to fix.

### A build volume (macOS)

Build output on the system disk is kept for about a day after deletion by
Time Machine's local snapshots, and Spotlight indexes it. A separate APFS
volume in the same container avoids both and shares the disk's free space,
with no repartitioning:

```sh
cargo steward setup --volume Scratch --dry-run   # see the plan
cargo steward setup --volume Scratch
```

This finds the system disk's APFS container (`diskutil info -plist /`), runs
`diskutil apfs addVolume <container> APFS Scratch`, and adds `/Volumes/Scratch`
as a scan root. If the volume already exists it's used as is. Two steps need
sudo, so `setup` prints them for you to run:

```sh
sudo mdutil -i off /Volumes/Scratch            # no Spotlight
sudo tmutil addexclusion -v /Volumes/Scratch   # no Time Machine
```

Then send build output there, either with `CARGO_TARGET_DIR` per shell or
project, or for every workspace at once:

```sh
cargo steward setup --build-dir /Volumes/Scratch/cargo-build
```

On Linux, `--volume` isn't available: put build output on a separate
filesystem yourself and pass its mount point with `--scan-root`.

## Upgrade

Install again the same way, adding `--no-setup`:

```sh
scripts/install.sh --no-setup                                   # from an updated checkout
scripts/install.sh --release latest --repo OWNER/cargo-steward --no-setup
```

`cargo install` and the release download both replace the binary in place,
and Cargo's config already points at that path, so the next build uses the
new version. The script ends by running `doctor`, which checks that the
wrapper Cargo runs is the same version as the one just installed. (Running
`setup` again is harmless: it keeps your settings, except that it turns
automatic cleanup back on unless given `--no-auto-gc`.)

## Uninstall

Each piece is removed by its own command. Run them in this order, while the
binary is still installed:

```sh
cargo steward setup --undo        # restore the previous rustc-wrapper; turn off automatic cleanup
cargo steward hooks uninstall     # git hooks that seed new worktrees, if installed
cargo steward shim uninstall      # the cargo proxy, if installed
cargo uninstall cargo-steward     # or rm ~/.cargo/bin/cargo-steward for a prebuilt binary
rm -rf ~/.cargo/steward           # settings, records and the cleanup log
```

`cargo steward hooks status` and `cargo steward shim status` show whether
those are installed. `setup --undo` puts back the `build.rustc-wrapper` and
`build.build-dir` that setup replaced, removing them where there were none. It
leaves build directories alone; remove any you no longer want with `rm -rf`,
or run `cargo steward gc --apply` before uninstalling.
