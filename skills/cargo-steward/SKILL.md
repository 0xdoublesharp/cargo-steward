---
name: cargo-steward
description: Use when building, cleaning or maintaining Rust/Cargo workspaces on a machine with cargo-steward; when disk space is low or build output (target/, build dirs, incremental caches, *.rcgu.o) needs cleaning; when creating or retiring git worktrees of a Rust repository; when a Rust build is cold, slow, or waiting for a build slot; or before running `cargo clean`, deleting target directories, or setting RUSTC_WRAPPER, CARGO_TARGET_DIR or --target-dir.
---

# Working with cargo-steward

cargo-steward runs every Rust compile here and keeps build output fast and
bounded. Work with it, not around it.

## Detect it

```sh
command -v cargo-steward && cargo steward doctor --json
```

It is active when the `wrapper` section has an `ok` check saying Cargo's
rustc-wrapper is cargo-steward. Otherwise follow the project's own rules.

## What it already does

- **Leak fix:** deletes the `*.rcgu.o` files each incremental rebuild
  replaces (Rust 1.88 to 1.100).
- **Background cleanup**, at most hourly: stale objects, old sessions, idle
  caches, build directories of deleted worktrees or unused for weeks, more
  when a disk runs low. It skips busy directories and running programs'.
- **Routing** (when `cargo` on PATH is the proxy): each worktree builds into
  its own directory under a central root, with its own TMPDIR.
- **Warm starts:** a new worktree's first build starts as a copy-on-write
  clone of the best build among the repository's checkouts (the main
  checkout's when no build holds it, else the most recently used).
- **Build slots:** at most `max-builds` cargo builds and `max-compiles`
  rustc processes run at once, machine-wide.

## Rules

1. **Never clean by hand.** No `cargo clean`, `rm -rf target`, or deleting
   `*.rcgu.o`, `incremental/` or `deps/`. Preview with `cargo steward gc`,
   then `cargo steward gc --apply`.
2. **Never set `RUSTC_WRAPPER`**; it bypasses cargo-steward. To run rustc
   through another wrapper, set `CARGO_STEWARD_INNER=<program>`.
3. **Target directory:** if `cargo steward shim explain --json` shows
   `target_dir.routed: true` and `proxy.active: true`, don't set
   `CARGO_TARGET_DIR` or `--target-dir`. Otherwise follow the project's
   policy.
4. **Create worktrees where the project's worktrees live** (its docs, or
   beside those in `git worktree list`). One outside the project tree misses
   its `.cargo/config.toml` and builds cold.
5. **Retire a worktree** you created before deleting it:

   ```sh
   cargo steward remove --worktree <dir> --json           # preview
   cargo steward remove --worktree <dir> --apply --json
   git worktree remove <dir>
   ```

   If you built it with your own `CARGO_TARGET_DIR`, pass that directory:
   `cargo steward remove <build-dir> --apply`. For worktrees already deleted:
   `cargo steward remove --orphaned [--apply]`. Never remove what another
   task or person owns.
6. **Pin what a long run depends on** (a benchmark baseline, a binary under
   test): `cargo steward pin <build-dir>`, then `unpin`.
7. **Report real numbers.** Sizes count like `du`, so clones count in every
   copy. Report space recovered from `freed_on_disk[].bytes` (`gc --apply
   --json` or `remove --apply --json`); report a directory's real cost as
   `unshared_bytes` (`status --sizes --json`).

## Machine output

`status`, `gc`, `remove`, `dedup`, `doctor` and `shim explain` take `--json`:
one document on stdout, sizes in bytes. Failure prints
`{"version": 1, "error": "..."}` and exits nonzero.

- `doctor`: `sections[].checks[]` (`level`, `message`, `fix`).
- `status`: `build_dirs[]` (`path`, `last_used`, `pinned`, `orphaned`,
  `workspaces`, `size_bytes`, `unshared_bytes`), `disks[].free_bytes`.
- `gc`, `remove`: `applied`, `actions[]` (`kind`, `path`, `bytes`),
  `skipped[].reason`, `total_bytes`, `freed_on_disk[]`; `remove` adds
  `worktrees[]` (`build_dir`, `exists`).
- `dedup`: `files`, `bytes`, `freed_on_disk_bytes`.
- `shim explain`: `target_dir` (`routed`, `path`, `free_bytes`), `seed`
  (`status`, `from`, `message`), `build_slot`, `proxy` (`active`: `cargo` on
  PATH is the proxy). Put `--json` before cargo arguments:
  `cargo steward shim explain --json -- test -p foo`.

**Before a large build**, check `target_dir.free_bytes` from `shim explain
--json` (unrouted: `df` the target directory). If low, run `gc --apply`.

## Waiting for a build slot

```
cargo-steward: waiting for one of 2 Cargo build slots (held by pid 4121: `cargo test` in /src/app)
```

All `max-builds` slots are taken by the builds named. Yours starts when one
frees: wait. Never kill other agents' builds or run the real cargo to skip
the line. If a holder is your own background build, finish or stop it.
`max-compiles` waits are silent: slower, not stuck.

## Commands

| Command | Use |
| --- | --- |
| `cargo steward doctor [--json]` | Check the setup |
| `cargo steward status [--sizes] [--json]` | Build directories, last use, sizes |
| `cargo steward gc [--apply] [-v] [--json]` | Preview, or run, a cleanup |
| `cargo steward remove (--worktree DIR \| BUILD_DIR... \| --orphaned) [--apply] [--json]` | Remove whole build directories safely |
| `cargo steward pin DIR` / `unpin DIR` | Protect a build directory from cleanup |
| `cargo steward dedup [--apply] [--json]` | Share identical files across build directories |
| `cargo steward seed --worktree DIR` | Start a worktree's build from another checkout's |
| `cargo steward shim explain [--json] [-- ARGS]` | Where a build here goes, seeding, free space |
| `cargo steward shim status` | Whether `cargo` on PATH is the proxy |

Change settings (`~/.cargo/steward/config.toml`) only when the user asks.

## Troubleshooting

- **Cold build in a new worktree:** run `cargo steward shim explain --json`
  there. `seed.status`: `will_seed` is good; `source_missing`: no other
  checkout has built yet; `all_busy`: builds hold every source, so seeding
  waits; `target_not_empty`: it already has output;
  `nothing_routed`: run `cargo steward seed --worktree <dir>` before the
  first build. Check rule 4.
- **Build fails mentioning cargo-steward:** rerun once. If it fails again,
  run `cargo steward doctor --json` and tell the user what it reports. Don't
  edit `~/.cargo/config.toml` or set `RUSTC_WRAPPER` to get past it.
- **Disk keeps filling:** find large idle directories in `cargo steward
  status --sizes --json`; ask before removing ones your task doesn't own.
