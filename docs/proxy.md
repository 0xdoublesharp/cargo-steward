# The `cargo` proxy

cargo-steward can stand in for `cargo` itself. Installed as `cargo` ahead of
the real one on `PATH`, it decides where each build's output and temporary
files go, warms up new worktrees, and limits how many builds run at once,
then runs the real cargo with the same arguments. It replaces the
per-project `cargo` shims that multi-worktree setups grow, and names its
directories the way such shims do, so build directories they made keep
working.

For each `cargo ...` it:

1. **Routes the target directory.** With `target-root` set, it sets
   `CARGO_TARGET_DIR` to `<target-root>/<repo>-<hash>/<worktree>`. Every
   checkout of a repository shares `<repo>-<hash>`, and each worktree gets
   its own directory under it ([naming](#naming)).
2. **Routes temporary files.** With `tmp-root` set, it sets `TMPDIR` (with
   a trailing slash), `TMP` and `TEMP` to
   `<tmp-root>/<repo>-<hash>/<worktree>`. Cleanup removes that directory
   with the worktree's build directory, or, when the build directory is gone
   some other way, once nothing in it has changed for `orphan-grace`.
3. **Seeds new worktrees.** When a build runs in a linked worktree whose
   target directory is new or empty, and another checkout of the repository
   has a target directory under `<target-root>/<repo>-<hash>/`, it first runs
   `cargo-steward seed --target-dir <new> --worktree <dir> --quiet` and waits
   for it. `seed` clones the best build among the repository's worktrees,
   and with `build.build-dir` set, seeds the worktree's build directory too
   ([warm-start.md](warm-start.md#which-build-it-clones)). A failed seed only
   means a cold build. `seed = false` turns this off.
4. **Waits for a build slot.** With `max-builds = N`, at most N builds run
   at once across the machine ([build slots](#build-slots)).
5. **Runs the real cargo**, with `argv[0]` still `cargo`.

It finds the repository by reading `.git` itself, and runs no other program
except `seed` (step 3, only for a new or empty target directory) and, with
`link-target`, `git check-ignore` when the link is missing. It adds about
2 ms to a cargo command.

If something goes wrong (unreadable settings, a directory it can't create),
it prints one `warning: cargo-steward: ...` line and runs cargo without that
change. The only thing that stops cargo from running is having no real cargo
to run.

## Install

```sh
cargo steward shim install --shell zsh       # or bash, fish; omit to just print the PATH line
cargo steward shim status                    # installed? first on PATH? what would it do here?
cargo steward shim explain -- test -p foo    # what it would do for one command, here
cargo steward shim explain --json            # the same as one JSON document (json.md)
cargo steward shim uninstall
```

`install` creates `<bin-dir>/cargo`, a symlink to the cargo-steward
executable. `<bin-dir>` is `bin` in the steward home unless `--bin-dir DIR`
names another; give `uninstall` the same `--bin-dir`. It won't replace a
`cargo` there that isn't its own. `--shell` adds a marked block that puts
the bin directory first on `PATH` to the shell's startup file:
`.zshenv` (in `$ZDOTDIR` or `~`, read by every zsh, including the
non-interactive ones agents run), `~/.bashrc`, or fish's `config.fish`
(under `$XDG_CONFIG_HOME` or `~/.config`). `uninstall` removes the symlink
and any such blocks.

## Settings

In `config.toml` in the steward home. With neither root set, the proxy
routes nothing.

| Setting       | Default | Meaning |
| ------------- | ------- | ------- |
| `target-root` | unset   | Directory that holds the per-worktree target directories. |
| `tmp-root`    | unset   | Directory that holds the per-worktree `TMPDIR`s. |
| `max-builds`  | unset   | Most cargo builds to run at once. Unset or `0`: no limit. |
| `real-cargo`  | unset   | The cargo to run. Searched for when unset ([below](#finding-the-real-cargo)). |
| `link-target` | `false` | When it routes a workspace's target directory, also make a missing `<workspace>/target` a symlink to it, so scripts that expect `./target` and builds that bypass the proxy (an IDE's) land in the same place. Inside git, only where git ignores `target`. Never for a forced route. |
| `seed`        | `true`  | Seed new worktrees' target directories ([warm-start.md](warm-start.md)). |

Roots must be absolute paths; a leading `~/` means `$HOME`. `{label}` in a
root is replaced by the repository's label (see [naming](#naming)), which
groups each project's output under its own folder:

```toml
target-root = "/Volumes/Scratch/{label}/cargo"
tmp-root = "/Volumes/Scratch/{label}/tmp"
```

### Routes

`[[routes]]` give commands run under one directory tree their own
`target-root`, `tmp-root` and `max-builds`. The route with the longest
matching `path` applies; otherwise the top-level settings do. A route's
`max-builds` counts its own set of slots, separate from the top-level one.
A route that sets no roots routes nothing, which exempts a tree, or gives it
a build limit alone.

```toml
[[routes]]
path = "/Users/me/Workspaces/bigproject"
target-root = "/Volumes/Scratch/bigproject/cargo"
tmp-root = "/Volumes/Scratch/bigproject/tmp"
max-builds = 3
```

With `force = true`, a route routes even when `CARGO_TARGET_DIR` is already
set or a workspace config sets `build.target-dir`, as a shim that always
sets the target directory does; only `--target-dir` or `--config` on the
command line win. Use it when replacing such a shim, so that a target
directory exported for one checkout (say, by a shell hook) doesn't leak into
builds run in another (`make -C other-repo`).

`cargo steward shim explain` shows which route applies to the current
directory.

## Naming

The namespace for a command is `<label>-<hash>/<key>`, worked out from the
directory the command works in: the directory of `--manifest-path` when one
comes before `--`, and the current directory otherwise.

- **In git:** `common` is the repository's shared git directory (what
  `git rev-parse --path-format=absolute --git-common-dir` prints) and
  `checkout` is this checkout's own git directory (`--absolute-git-dir`),
  both with symlinks resolved.
  - `label` is the name of `common`'s parent when `common` is named `.git`,
    and `common`'s own name otherwise (submodules, bare repositories).
  - `hash` is the first 10 hex digits of the SHA-256 of `common`'s full path.
  - `key` is `main` in the main checkout. In a linked worktree it's the name
    of the worktree's git directory, `.git/worktrees/<name>`, which is
    usually, but not always, the worktree directory's name.
- **Outside git:** `label` is the directory's name, `hash` is from its full
  path, and `key` is `standalone`.

In `label`, every character outside `[A-Za-z0-9_-]` becomes `-`. A linked
worktree whose git directory is named `main` shares the main checkout's
directories.

## What isn't routed

The proxy leaves the target directory and `TMPDIR` alone when something else
has already chosen a target directory:

- `CARGO_TARGET_DIR` or `CARGO_BUILD_TARGET_DIR` is set. This includes
  cargo run from a build script under a routed build, which inherits the
  outer build's directory.
- `--target-dir`, or `--config` setting `build.target-dir`, comes before `--`.
- A `.cargo/config.toml` (or `.cargo/config`) in the current directory, the
  command's directory or any of their parents sets `build.target-dir`.

A `build.target-dir` in `$CARGO_HOME/config.toml` doesn't count: a
machine-wide target directory is what the proxy replaces. A [forced
route](#routes) goes by the command line alone.

Config files named by `--config <file>`, cargo's `-C <dir>`, and `[env]`
settings aren't looked at. Git directories are found by reading `.git`, so
`GIT_DIR`, `GIT_WORK_TREE`, `GIT_CEILING_DIRECTORIES` and
`GIT_DISCOVERY_ACROSS_FILESYSTEM` don't change where a build goes.

## Build slots

With `max-builds = N`, each build holds an exclusive lock on one of N files
in `slots/` in the steward home (a route's limit uses its own subdirectory).
A build that has to wait prints, once:

```
cargo-steward: waiting for one of 4 Cargo build slots (held by pid 4121: `cargo test -p api` in /src/app; pid 4388: `cargo build` in /src/app-fix)
```

The proxy runs cargo as its child and holds the slot exactly while cargo
works, tests included, releasing it however cargo ends:

- Programs a build leaves running (a test's daemon) don't keep it.
- `cargo run` gives it up once cargo has started the program.
- Cargo run from inside a build (a build script, a `cargo-*` subcommand)
  reuses its parent's slot rather than waiting for a second one, but only
  while that build's proxy is one of its ancestors.
- Signals sent to the proxy alone are passed on to cargo; Ctrl-C at a
  terminal reaches cargo directly.

Only commands that may compile take a slot. These don't: `metadata`, `fmt`,
`tree`, `help`, `version`, `locate-project`, `pkgid`, `read-manifest`,
`fetch`, `update`, `generate-lockfile`, `clean`, `search`, `info`, `login`,
`logout`, `owner`, `yank`, `uninstall`, `new`, `init`, `steward`, `watch`
(each build `cargo watch` starts takes its own), `cargo` with no command
(`cargo --version`, `cargo --list`), and anything with `--help` or `-h`.
Unknown commands do take a slot, because cargo expands aliases
itself and an alias for a build mustn't get around the limit.

`max-builds` limits cargo commands; the wrapper's `max-compiles` limits the
compiles inside them ([README](../README.md#how-it-works)).

## Finding the real cargo

In order:

1. `real-cargo`;
2. each `cargo` on `PATH`;
3. `$CARGO_HOME/bin/cargo`;
4. the `cargo` next to `rustup`.

Any candidate that is the proxy itself is skipped, by file identity, so
symlinks and hard links to the proxy are skipped too. The path is kept as
found, not resolved: rustup's `cargo` is a symlink to `rustup`, which looks
at the name it was run as. `+toolchain` arguments pass through unchanged.

If another `cargo` shim comes after the proxy on `PATH`, the proxy runs it.
Set `real-cargo` to skip it, for example to `~/.cargo/bin/cargo`.

## Replacing a project shim

A project shim that puts each checkout's target directory under
`<root>/cargo/<namespace>` and its TMPDIR under `<root>/tmp/<namespace>`,
with the namespace named as [above](#naming), maps over like this:

| Project shim                         | cargo-steward                      |
| ------------------------------------ | ---------------------------------- |
| `<root>/cargo/<namespace>`           | `target-root = "<root>/cargo"`     |
| `<root>/tmp/<namespace>`             | `tmp-root = "<root>/tmp"`          |
| a limit on concurrent cargo builds   | `max-builds = N`                   |
| a variable naming the real cargo     | `real-cargo`                       |
| always overrides `CARGO_TARGET_DIR`  | `force = true` on a route          |
| `<root>/bin/cargo` on `PATH`         | `<steward home>/bin/cargo` on `PATH` |

Put these in a `[[routes]]` entry for the project's directory to leave
other projects as they are. Compare `cargo steward shim explain` with the
shim's own report in a checkout before switching. Then take the project's
`bin` directory off `PATH`, or set `real-cargo`, so the proxy doesn't run
the old shim behind it. The proxy's build slots are its own, and don't
count against a shim's.

The proxy sets only `CARGO_TARGET_DIR` and the temporary-directory
variables. Set anything else a shim exported (such as `SCCACHE_DIR`) in
your shell or in cargo's `[env]`.
