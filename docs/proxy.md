# The `cargo` proxy

cargo-steward can stand in for `cargo` itself. Installed as `cargo` ahead of
the real one on PATH, it decides where each build's output and temporary
files go, warms up new worktrees, and limits how many builds run at once.
Then it `exec`s the real cargo with the same arguments (or, holding a build
slot, runs it and waits). It replaces the
per-project `cargo` shims that many multi-worktree setups grow, and uses the
same directory names, so build directories those shims made keep working.

For each `cargo ...` it:

1. **Routes the target directory.** With `target-root` set, it sets
   `CARGO_TARGET_DIR` to `<target-root>/<repo>-<hash>/<worktree>`. Every
   checkout of a repository shares `<repo>-<hash>`, and each worktree gets
   its own directory under it.
2. **Routes temporary files.** With `tmp-root` set, it sets `TMPDIR` (with
   a trailing slash), `TMP` and `TEMP` to `<tmp-root>/<repo>-<hash>/<worktree>`.
   Cleanup removes that directory with the worktree's build directory, and
   one whose build directory is gone otherwise once nothing in it has
   changed for `orphan-grace`.
3. **Seeds new worktrees.** When a build runs in a linked worktree whose
   target directory is new or empty, and another checkout of the repository
   has a target directory in `<target-root>/<repo>-<hash>/`, it first runs
   `cargo-steward seed --target-dir <new> --worktree <dir> --quiet` and
   waits for it. `seed` clones the main checkout's target directory, or,
   when the main checkout hasn't built or a build is using it, the most
   recently used other one (see
   [warm-start.md](warm-start.md#which-build-it-clones)). With
   `build.build-dir` set, `seed` asks `cargo metadata` where the new
   worktree's build directory is and starts it from the same worktree's
   build directory too. A failed seed only means a cold build.
   `cargo steward shim explain` names the directory it would seed from.
   Turn this off with `seed = false`.
4. **Waits for a build slot.** With `max-builds = N`, at most N builds run
   at once across the machine. A build that has to wait prints
   `cargo-steward: waiting for one of N Cargo build slots (held by ...)`
   once, naming each holder's process, command and directory. The proxy
   holds the slot while cargo runs (tests included) and no longer: programs
   a build leaves running don't keep it, and `cargo run` gives it up once
   its program starts. Cargo run from inside a build (a build script, a
   `cargo-*` subcommand) reuses the slot. `cargo watch` takes none itself;
   each build it starts takes its own.
5. **Runs the real cargo**, with `argv[0]` still `cargo`.

The proxy never runs `git` or any other program, except `seed` in step 3,
which runs only for a target directory that is new or empty.
It finds the repository by reading `.git` itself. It adds about 2 ms to a
cargo invocation.

If something goes wrong (unreadable settings, a directory it can't create),
it prints one `warning: cargo-steward: ...` line and runs cargo without that
change. The only thing that stops cargo from running is having no real cargo
to run.

## Install

```sh
cargo steward shim install --shell zsh   # or bash, fish; omit to just print the PATH line
cargo steward shim status                # installed? first on PATH? what would it do here?
cargo steward shim explain -- test -p foo  # what it would do for one command, here
cargo steward shim explain --json          # the same as one JSON document (docs/json.md)
cargo steward shim uninstall
```

`install` creates `<bin-dir>/cargo` as a symlink to the cargo-steward
executable. The default `<bin-dir>` is `bin` in the steward home
(`$CARGO_STEWARD_HOME`, else `$CARGO_HOME/steward`). Use `--bin-dir DIR`
to pick another directory. It won't replace a `cargo` there that isn't its
own. `--shell` adds a marked block to `~/.zshenv` (`$ZDOTDIR/.zshenv`, read by every zsh, including non-interactive ones),
`~/.bashrc` or `~/.config/fish/config.fish` that puts the bin directory first
on PATH. `uninstall` removes the symlink and any such blocks.

## Settings

The settings are in `config.toml` in the steward home. With none of these
set, the proxy just runs cargo.

| Setting       | Default | Meaning |
| ------------- | ------- | ------- |
| `target-root` | unset   | Directory that holds the per-worktree target directories. |
| `tmp-root`    | unset   | Directory that holds the per-worktree `TMPDIR`s. |
| `max-builds`  | unset   | Most cargo builds to run at once. Unset or `0`: no limit. |
| `real-cargo`  | unset   | The cargo to run. Searched for when unset. |
| `seed`        | `true`  | Seed new worktrees' target directories from another checkout's. |

Roots must be absolute paths. A leading `~/` means `$HOME`. `{label}` in a
root is replaced by the repository's label (the `<label>` of the namespace
below), which groups each project's output under its own folder:

```toml
target-root = "/Volumes/Scratch/{label}/cargo"
tmp-root = "/Volumes/Scratch/{label}/tmp"
```

### Routes

`[[routes]]` give commands run under one directory tree their own
`target-root`, `tmp-root` and `max-builds`. The route with the longest
matching `path` applies; otherwise the top-level settings do. A route's
`max-builds` counts its own set of slots, separate from the top-level one. A
route that sets no roots routes nothing (useful to exempt a tree, or to give
it a build limit alone).

```toml
[[routes]]
path = "/Users/me/Workspaces/0xdiesis"
target-root = "/Volumes/Scratch/0xdiesis/cargo"   # the names its old shim used
tmp-root = "/Volumes/Scratch/0xdiesis/tmp"
max-builds = 3
```

A route with `force = true` routes even when `CARGO_TARGET_DIR` is already set
or a workspace config sets `build.target-dir`, as a shim that always sets the
target directory does; only `--target-dir` or `--config` on the command line
win. Use it when replacing such a shim, so that a target directory exported
for one checkout (say, by a shell hook) doesn't leak into builds run in
another (`make -C other-repo`).

`cargo steward shim explain` shows which route applies to the current
directory.

## Naming

The namespace for a command is `<label>-<hash>/<key>`, worked out from the
directory the command works in. That's the directory of `--manifest-path`
when one comes before `--`, and the current directory otherwise.

- **In git:** `common` is the repository's shared git directory (what
  `git rev-parse --path-format=absolute --git-common-dir` prints) and
  `checkout` is this checkout's own git directory (`--absolute-git-dir`),
  both with symlinks resolved.
  - `label` is the name of `common`'s parent when `common` is named `.git`,
    and `common`'s own name otherwise (submodules, bare repositories).
  - `hash` is the first 10 hex digits of SHA-256 of `common`'s full path.
  - `key` is `main` in the main checkout. In a linked worktree it's the name
    of the worktree's git directory, `.git/worktrees/<name>`, which is
    usually but not always the worktree directory's name.
- **Outside git:** `label` is the directory's name, `hash` is from its full
  path, and `key` is `standalone`.

In `label`, every character outside `[A-Za-z0-9_-]` becomes `-`.

## What isn't routed

The proxy leaves the target directory and `TMPDIR` alone when something else
has already chosen a target directory:

- `CARGO_TARGET_DIR` or `CARGO_BUILD_TARGET_DIR` is set. This includes
  cargo run from a build script under a routed build, which inherits the
  outer build's directory.
- `--target-dir`, or `--config` setting `build.target-dir`, comes before `--`.
- A `.cargo/config.toml` (or `.cargo/config`) in the current directory, the
  command's directory or any of their parents sets `build.target-dir`.

A `build.target-dir` in `$CARGO_HOME/config.toml` doesn't count, because a
machine-wide target directory is what the proxy replaces.

## Build slots

Each build holds an exclusive `flock` on one of `max-builds` lock files in
`<steward home>/slots/`. The descriptor stays open and inheritable across
`exec`, so the slot is held for exactly as long as cargo runs. The kernel
releases it when cargo exits, however it exits. Cargo run from inside a
build (a build script running `cargo`) inherits the descriptor and the
variables naming it (`CARGO_STEWARD_SLOT_FD`, `CARGO_STEWARD_SLOT`), and
keeps using its parent's slot instead of waiting for a second one. The proxy
reuses an inherited slot only if the descriptor is really open on that slot
file and holds its lock, so a leftover variable can't get around the limit.

Only commands that may compile take a slot. These don't: `metadata`, `fmt`,
`tree`, `help`, `version`, `locate-project`, `pkgid`, `read-manifest`,
`fetch`, `update`, `generate-lockfile`, `clean`, `search`, `info`, `login`,
`logout`, `owner`, `yank`, `uninstall`, `new`, `init`, `steward`, and
anything with `--help`, `-h`, `--version` or `--list`. Unknown commands do
take a slot, because cargo expands aliases itself and an alias for a build
mustn't get around the limit.

## Finding the real cargo

In order:

1. `real-cargo`;
2. each `cargo` on PATH;
3. `$CARGO_HOME/bin/cargo`;
4. the `cargo` next to `rustup`.

Any candidate that is the proxy itself is skipped, by file identity, so
symlinks and hard links to the proxy are skipped too. The path is kept as
found, not resolved: rustup's `cargo` is a symlink to `rustup`, which looks
at the name it was run as. `+toolchain` arguments pass through unchanged.

If another `cargo` shim comes after the proxy on PATH, the proxy runs it.
Set `real-cargo` to skip it, for example to `~/.cargo/bin/cargo`.

## Replacing a project shim

Diesis's `scripts/scratch`, a Python `cargo` shim with a `.env.scratch.json`
holding `root` and `cargo_max_processes`, maps over like this:

| Diesis                                  | cargo-steward            |
| --------------------------------------- | ------------------------ |
| `<root>/cargo/<namespace>`              | `target-root = "<root>/cargo"` |
| `<root>/tmp/<namespace>`                | `tmp-root = "<root>/tmp"` |
| `cargo_max_processes = N`               | `max-builds = N`         |
| `DIESIS_REAL_CARGO`                     | `real-cargo`             |
| `<root>/bin/cargo` on PATH              | `<steward home>/bin/cargo` on PATH |

Namespaces match exactly, so the proxy finds the shim's existing build
directories. For example, Diesis's `diesis-core/diesis` checkout is
`diesis-ae0deba9a3/main`, and its `tps-harness-20261105` worktree is
`diesis-ae0deba9a3/tps-harness-20261105`. Compare
`cargo steward shim explain` with `scripts/scratch --check` in a checkout
before switching. Then take the project's `bin` directory off PATH, or
set `real-cargo`, so the proxy doesn't run the old shim behind it.

Differences from the Diesis shim:

- **Not ported:** `SCCACHE_DIR`, `SCCACHE_SERVER_UDS`, the
  `DIESIS_*` variables, the `data/` directory, the `reports` symlink, APFS
  volume verification, and the `--shell-env` / `--shell-reset` / `--cwd`
  modes. Set sccache's variables in your shell or in cargo's `[env]` if you
  need them.
- **Routing:** the shim always overrides `CARGO_TARGET_DIR`. The proxy
  respects a target directory chosen elsewhere (see above).
- **Build slots:** they're machine-wide in the steward home, not per project
  root. The shim's slots and the proxy's don't count against each other.

## Limits

- **Git discovery:** git directories are found by reading `.git`, so the
  proxy ignores `GIT_DIR`, `GIT_WORK_TREE`, `GIT_CEILING_DIRECTORIES` and
  `GIT_DISCOVERY_ACROSS_FILESYSTEM`. In the usual case of a hook running
  in its own checkout, it finds the same checkout.
- **Target-dir detection:** config files named by `--config <file>`, cargo's
  `-C <dir>`, and `[env]` settings aren't looked at.
- **Worktree named `main`:** a linked worktree whose git directory is named
  `main` shares the main checkout's directory, as with the Diesis shim.
- **Concurrent first builds:** two builds starting at once in a new worktree
  can both try to seed it. `seed` must cope with a target directory that's
  being filled.
- **Platforms:** Unix only, with `flock` and `exec`.
