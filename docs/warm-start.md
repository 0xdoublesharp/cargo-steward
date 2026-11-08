# Warm starts and deduplication

A new git worktree normally starts with an empty build directory and compiles
every dependency from scratch, then keeps its own full copy of each. On a
filesystem with copy-on-write clones, cargo-steward avoids both:

- **`seed`** starts a new worktree's build directory as a clone of another
  worktree's: the main checkout's, or another worktree's when the main
  checkout has no build or a build is using it. The clone takes well under a
  second and no disk space, and cargo finds the dependencies already built.
- **`hooks`** installs a git hook that seeds each new worktree as git creates
  it.
- **`dedup`** finds files that are byte-for-byte identical across build
  directories and makes them share one copy on disk.

All three need a filesystem that can clone files: APFS on macOS, or btrfs, XFS
(formatted with reflink support, the default since xfsprogs 5.1) or bcachefs
on Linux. A build directory and the one it is cloned from must be on the same
volume. Elsewhere, these commands do nothing and say so. They never fall back
to copying bytes.

## `cargo steward seed`

```sh
cargo steward seed [--target-dir DIR] [--from DIR] [--worktree DIR] [--quiet]
```

Clones another worktree's build directory into this worktree's, if this one
hasn't built anything yet, and prints which one it used:

```
Seeded /src/app-fix/target from /src/app-docs/target (worktree app-docs, last used 4m ago; busy: the main checkout; 0.31 s, copy-on-write)
```

| Option | Default |
|---|---|
| `--worktree DIR` | The current directory |
| `--target-dir DIR` | The worktree's target directory, as `cargo metadata` reports it there (so `CARGO_TARGET_DIR`, `build.target-dir` and `build.build-dir` are respected) |
| `--from DIR` | The best of the repository's other worktrees' build directories (below) |
| `--quiet` | Print nothing |

`--from` can be given more than once. The first is used unless a build is
using it, then the most recently used of the others.

With `build.build-dir` set, the worktree's build directory (intermediate
artifacts) is seeded from another worktree's build directory, and its target
directory (final artifacts) from the same worktree's target directory.

### Which build it clones

The candidates are the build directories of the main checkout and of each of
the repository's other worktrees (`git worktree list`). Each is found without
running cargo there:

- A build directory inside the worktree (`target/`, or a relative
  `build.target-dir` or `build.build-dir`) is at the same place in every
  other worktree.
- Under the [`cargo` proxy](proxy.md), every worktree's target directory is
  in the repository's directory under `target-root`, including those of
  worktrees since deleted.
- Otherwise, the wrapper's records say which build directories each
  worktree's packages compiled into.

If none of these locates the main checkout's, `seed` runs `cargo metadata`
there.

The main checkout's build is used whenever it holds one and no build is using
it. Otherwise the others are tried from the most recently used (by the
wrapper's records, or the times cargo last wrote into each), skipping any
that hold no build or are on another volume. A candidate a build is using
gets half a second to come free before the next is tried; if builds are
using all of them, they're tried in turn until 5 seconds have passed, and
then the worktree builds cold.

It does nothing, and exits 0, when:

- `seed = false` is set in the settings;
- the target already holds a build, or exists and isn't empty;
- no other worktree has a build to clone from yet;
- the target and the source are the same directory (for example, a shared
  `CARGO_TARGET_DIR`);
- builds kept every source directory busy for 5 seconds;
- the filesystem can't clone, or the directories are on different volumes.

### What cargo rebuilds afterwards

Cargo decides what to rebuild by comparing file modification times, and a
clone keeps them. Dependencies from crates.io, git, or paths outside the
repository are found fresh. Crates inside the repository rebuild once, because
`git worktree add` writes their sources with new times. Incremental caches come
along, so those rebuilds are usually incremental.

### Safety

- The source is cloned while holding cargo's locks on each of its profiles,
  so it's never caught halfway through a build.
- The clone is made under a temporary name next to the target and renamed into
  place, so a build never sees a half-made directory.
- A pin (`cargo steward pin`) on the source isn't copied to the clone.
- After the clone, the two directories are independent: building in one never
  changes the other.

## `cargo steward hooks`

```sh
cargo steward hooks install [--repo DIR]
cargo steward hooks uninstall [--repo DIR]
cargo steward hooks status [--repo DIR]
```

`install` writes a `post-checkout` hook into the repository's hooks directory
(`core.hooksPath` if set, else `.git/hooks`). All of the repository's
worktrees share it. When git creates a worktree, the hook runs:

```sh
cargo-steward seed --worktree "$PWD" --quiet
```

Plain checkouts and branch switches don't trigger it. The hook never makes
git fail: if seeding can't run, git carries on as before.

The hook calls cargo-steward by the absolute path it was installed from. If
you move or reinstall the binary somewhere else, run `hooks install` again.

**An existing `post-checkout` hook is kept.** It's moved to
`post-checkout.pre-steward`, and the new hook runs it first with the same
arguments and exits with its status. `uninstall` removes the cargo-steward
hook and moves the original back, unchanged. If `core.hooksPath` points at a
directory you track in git, both commands change files there.

`status` reports whether the hook is installed, whether it chains another,
and whether seeding is turned off.

## `cargo steward dedup`

```sh
cargo steward dedup              # report what would be shared
cargo steward dedup --apply [-v] # share it; -v lists every file
```

Looks through every build directory cargo-steward knows about (those the
wrapper has seen and those under `scan-roots`) for files that:

- are at least 64 KiB;
- are on the same volume;
- have the same name and size (cargo puts a hash of the unit's inputs in its
  output names, so identical outputs have identical names);
- have the same bytes, compared in full.

With `--apply`, each duplicate is replaced by a clone of one kept copy, with
the duplicate's own modification time and permissions, so cargo sees no
change. Afterwards it reports the change in the volume's free space, which is
the real measure: sizes as `du` counts them don't show shared blocks.

Things it leaves alone:

- Incremental caches (`incremental/`), which rustc treats as a unit.
- Files with more than one hard link. Dedup never creates hard links: cargo
  and rustc rewrite some outputs in place, and that would change every linked
  copy.
- Files that are already clones of the kept copy. On APFS this is detected,
  so running `dedup` again finds nothing new to do. On Linux it isn't, so a
  later run compares and re-clones the same files again (harmless, but it
  does the reading again).
- Any file in a profile a build is using, or that changed after it was
  compared. These are counted and skipped until the next run.

With `dedup = true` (the default), automatic cleanup also deduplicates.

## Settings

In `~/.cargo/steward/config.toml`:

```toml
seed = true     # let `seed` (the hook and the proxy) clone build directories
dedup = true    # let automatic cleanup deduplicate build directories
```

`cargo steward dedup` run by hand works whatever `dedup` is set to.
