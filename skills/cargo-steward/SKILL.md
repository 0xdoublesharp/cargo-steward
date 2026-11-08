---
name: cargo-steward
description: Use when building, cleaning or maintaining Rust/Cargo workspaces on a machine with cargo-steward; when disk space is low or build output (target/, build dirs, incremental caches, *.rcgu.o) needs cleaning; when creating or retiring git worktrees of a Rust repository; when a Rust build is unexpectedly cold or slow; or before running `cargo clean`, deleting target directories, or setting RUSTC_WRAPPER, CARGO_TARGET_DIR or --target-dir.
---

# Maintaining Rust workspaces with cargo-steward

cargo-steward keeps Cargo build output fast and bounded. On a machine where it
is set up, most build-output housekeeping already happens without you: work
with it rather than around it.

Check that it's present before relying on it:

```sh
command -v cargo-steward && cargo steward doctor
```

If it isn't installed, none of this applies; follow the project's own rules.

`doctor`, `status`, `gc`, `remove`, `dedup` and `shim explain` take `--json`
and then print one JSON document on stdout, with sizes in bytes and stable
ids for kinds and statuses. Read that instead of parsing the text.

## What already happens on its own

- **Every compile runs through it** (Cargo's `rustc-wrapper`, with sccache or
  any other wrapper behind it). It removes the debug object files (`*.rcgu.o`,
  `*.rcgu.dwo`) that each incremental rebuild leaves behind on Rust 1.88 to
  1.100 (rust-lang/rust#161824).
- **Cleanup runs in the background after builds** (at most hourly) and
  removes, cheapest to rebuild first:
  - leftover object files and superseded incremental sessions;
  - old-layout files after an upgrade to Cargo 1.100+;
  - incremental caches and build units nobody has used lately;
  - build directories of deleted worktrees, or unused for weeks;
  - when a disk is low, the build directories that are cheapest to rebuild.

  It holds Cargo's own locks while it works, never touches a running build,
  and keeps any directory a running program was started from.
- **With the `cargo` proxy on PATH**, each worktree's target directory and
  TMPDIR are routed under a central root, a new worktree's first build starts
  as a copy-on-write clone of the main checkout's (seconds instead of
  minutes), and concurrent builds can be limited.

## Rules

1. **Don't free space by hand.** Don't run `cargo clean`, `rm -rf target`, or
   delete `*.rcgu.o`, `incremental/` or `deps/` files to save space. Preview
   with `cargo steward gc`, then run `cargo steward gc --apply`. It keeps what
   builds still use, and it removes an incremental session whole: deleting
   part of one can crash rustc.
2. **Don't reroute or rewrap builds** unless the task requires it: no
   `RUSTC_WRAPPER`, `build.rustc-wrapper`, `CARGO_TARGET_DIR`, `--target-dir`
   or `build.target-dir` of your own. Setting `RUSTC_WRAPPER` (even to empty)
   bypasses cargo-steward. To run rustc through another wrapper, set
   `CARGO_STEWARD_INNER` instead. `cargo steward shim explain` shows where a
   build in the current directory goes and why.
3. **Create worktrees where the project's other worktrees live**, inside the
   repository's tree when the project keeps them there (for example
   `<repo>/.worktrees/<name>`). A worktree outside the tree misses the
   project's `.cargo/config.toml`, so it builds with different settings,
   can't start from another checkout's build, and recompiles everything.
   After creating one, `cargo steward shim explain --json` (in it) should
   report `"seed": {"status": "will_seed", ...}`.
4. **Retire a worktree's build output with it.** Before deleting a worktree
   your task created, remove its build directory, then the worktree:

   ```sh
   cargo steward remove --worktree <path> --json            # preview
   cargo steward remove --worktree <path> --apply --json
   git worktree remove <path>
   ```

   If the worktree is already gone, cleanup removes its build directory a
   couple of days later on its own. Never remove build directories, or
   worktrees, that another task or person owns.
5. **Protect a build directory a long run depends on** (a benchmark baseline,
   a node under test) with `cargo steward pin <build dir>`, and
   `cargo steward unpin <build dir>` when done. Running programs are
   protected anyway; pin anything that must survive between runs.
6. **Report real numbers.** Sizes in `gc` and `status` are counted as `du`
   counts them, so blocks shared between copy-on-write clones count in every
   copy. Report `freed_on_disk` from `cargo steward gc --apply --json` (the
   "Freed on disk" line in text), or the change in `df`, as the space
   recovered, and `unshared_bytes` from `cargo steward status --sizes --json`
   (the UNSHARED column) as what a directory really costs.

## Maintaining a workspace

When asked to clean up or check on Rust workspaces:

1. `cargo steward doctor --json`. Checks with level `warn` carry a `fix`.
   Note what concerns this workspace: toolchain pins in the leak window,
   project configs or Makefiles that override the wrapper, worktrees outside
   the tree, unused toolchains. Fix them only within the task's scope, and
   mention the rest.
2. `cargo steward status --json`. Find this workspace's build directories
   (`build_dirs[].workspaces`), when each was last used, and which belong to
   deleted workspaces (`orphaned`).
3. `cargo steward gc --json` previews a cleanup; `cargo steward gc --apply
   --json` runs it. It is safe while other builds run: busy directories are
   skipped (`skipped[].reason` says why) and retried next time.
4. Retire the worktrees your task created that are merged or abandoned
   (rule 4).
5. On macOS, `cargo steward dedup` previews sharing identical files across
   build directories as clones; `--apply` does it. Automatic cleanup already
   does this.
6. Report the commands you ran, the `freed_on_disk` figures, and anything
   skipped (busy, pinned, or kept because a program runs from it).

Before a large build, check free space on the build's actual disk:
`cargo steward shim explain --json` reports it as `target_dir.free_bytes`
when the proxy routes the build (`target_dir.routed`); otherwise run `df -h`
on the target directory you know. If it's low, run
`cargo steward gc --apply` first.

## Commands

| Command | Use |
| --- | --- |
| `cargo steward doctor [--json]` | Check the setup; says what to change |
| `cargo steward status [--sizes] [--json]` | Build directories, last use, layout, size, unshared size |
| `cargo steward gc [--apply] [-v] [--json]` | Preview, or run, a cleanup |
| `cargo steward remove (--worktree DIR \| BUILD_DIR...) [--apply] [--json]` | Remove whole build directories safely |
| `cargo steward pin DIR` / `unpin DIR` | Keep, or stop keeping, a build directory |
| `cargo steward dedup [--apply] [--json]` | Share identical files across build directories |
| `cargo steward seed --worktree DIR` | Start a worktree's build directory from the main checkout's, or the most recently used idle worktree's |
| `cargo steward hooks install\|status` | Seed every new worktree of a repository (git hook) |
| `cargo steward shim explain [--json] [-- ARGS]` | Where a build here goes, which route applies, whether it seeds, free space there |
| `cargo steward ci start\|trim` | Trim a CI job's build output to what it used |

Settings live in `~/.cargo/steward/config.toml`, and automatic cleanups log to
`~/.cargo/steward/gc.log`. Change settings only when the user asks.

## When something looks wrong

- **A build is cold in a new worktree.** Run `cargo steward shim explain
  --json` there. Check that `target_dir.path` is the worktree's own, what
  `seed.status` says, and that the worktree is inside the project tree
  (rule 3).
- **A build fails mentioning cargo-steward.** Rerun once. If it persists,
  `cargo steward doctor`, then tell the user; `cargo steward setup --undo`
  restores the previous wrapper. Don't edit `~/.cargo/config.toml` yourself.
- **Disk keeps filling.** Run `cargo steward status --sizes` and look for
  large directories whose workspace still exists but that nobody builds in.
  Ask the user before removing ones your task doesn't own.
