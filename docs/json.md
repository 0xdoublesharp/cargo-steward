# JSON output

`status`, `gc`, `remove`, `dedup`, `doctor` and `shim explain` take `--json`.
With it, a command prints exactly one JSON document on stdout and nothing
else there; warnings stay on stderr, and the exit status is the same as
without it. Scripts and AI agents should read these documents rather than
the text, which is written for people and may change. Previews (without
`--apply`) change nothing and work with a read-only steward home.

```sh
cargo steward status --sizes --json | jq '.build_dirs[] | {path, size_bytes}'
cargo steward shim explain --json | jq '.target_dir.free_bytes'
```

Conventions, in every document:

- `version` (integer) is the format version, `1`. Within a version,
  fields are only added, never renamed, removed or retyped.
- Field names are snake_case.
- Sizes are integers in bytes, in fields ending `_bytes` or named `bytes`.
  Sizes are counted the way `du` counts them, so blocks that copy-on-write
  clones share count in every copy; `unshared_bytes` and the measured
  `freed_on_disk` figures are the real cost and gain.
- Times are RFC 3339 strings in UTC (`2026-11-07T09:15:02Z`), next to an
  integer `..._secs_ago`.
- Paths are strings.
- Kinds and statuses are stable string ids such as `"idle_unit"`; each has a
  human-readable `description` or `message` beside it.
- A field that doesn't apply is `null` (an empty array for lists).

When a command fails, it exits nonzero, prints `error: ...` on stderr, and
prints this document in place of its own:

```json
{ "version": 1, "error": "nothing to remove" }
```

## `status --json`

The build directories cargo-steward manages, most recently used first.

| Field | Type | |
|---|---|---|
| `sizes_measured` | bool | `--sizes` was given |
| `build_dirs` | array | one object per build directory, below |
| `build_dirs[].path` | string | the build directory's root |
| `build_dirs[].last_used` | string | RFC 3339 |
| `build_dirs[].last_used_secs_ago` | integer | |
| `build_dirs[].layout` | string | `"legacy"`, `"v2"` (Cargo 1.100's layout), `"mixed"` or `"empty"` |
| `build_dirs[].pinned` | bool | `cargo steward pin`ned |
| `build_dirs[].orphaned` | bool | every package recorded building here has been deleted |
| `build_dirs[].workspaces` | array of strings | the package directories the wrapper recorded building here |
| `build_dirs[].size_bytes` | integer or null | with `--sizes`: as `du` counts it |
| `build_dirs[].unshared_bytes` | integer or null | with `--sizes`: blocks no clone shares (what removing it alone frees); null where the filesystem can't tell |
| `total_bytes` | integer or null | with `--sizes`: sum of `size_bytes` |
| `total_unshared_bytes` | integer or null | with `--sizes`: sum of `unshared_bytes` |
| `disks` | array | one per disk holding build directories |
| `disks[].path` | string | the first build directory listed on that disk |
| `disks[].free_bytes` | integer | free space on it |

```json
{
  "version": 1,
  "sizes_measured": true,
  "build_dirs": [
    {
      "path": "/Volumes/Scratch/app/cargo/main",
      "last_used": "2026-11-07T09:15:02Z",
      "last_used_secs_ago": 340,
      "layout": "v2",
      "pinned": false,
      "orphaned": false,
      "workspaces": ["/Users/me/src/app"],
      "size_bytes": 8412422144,
      "unshared_bytes": 3021504512
    }
  ],
  "total_bytes": 8412422144,
  "total_unshared_bytes": 3021504512,
  "disks": [{ "path": "/Volumes/Scratch/app/cargo/main", "free_bytes": 212849876992 }]
}
```

## `gc --json` and `remove --json`

What a cleanup removes (or with `--apply`, removed), what it kept back and
why, and what the disks gained. `gc` and `remove` print the same document;
fields that only one of them fills are empty in the other's.

| Field | Type | |
|---|---|---|
| `command` | string | `"gc"`, `"remove"`, or `"remove --orphaned"` |
| `applied` | bool | `--apply`: removed; otherwise a preview |
| `busy` | bool | `gc --apply` found another cleanup running and did nothing |
| `worktrees` | array | `remove --worktree`: `{worktree, build_dir, exists}` per worktree; a `build_dir` that doesn't exist has nothing to remove |
| `refused` | array | `remove`: `{path, reason}` per path given that doesn't exist or isn't the root of a cargo build directory |
| `actions` | array | what was or would be removed, below |
| `actions[].kind` | string | a kind id, below |
| `actions[].description` | string | the kind, in words |
| `actions[].path` | string | the directory removed; for `stale_objects`, the directory holding them |
| `actions[].files` | integer | files removed, for `stale_objects`; otherwise 0 |
| `actions[].bytes` | integer | |
| `actions[].note` | string | such as `unused for 30d` |
| `skipped` | array | due for removal but kept: `{kind, description, path, reason}`, such as a running program, a build holding the locks, or a pin; a `low_space` entry whose path is a disk's directory says why removing for space stopped |
| `totals` | array | per kind with actions: `{kind, description, count, bytes}` |
| `total_bytes` | integer | sum over `actions` |
| `freed_on_disk` | array | `gc --apply`: `{path, bytes}` per disk it removed from, the measured change in free space (an integer that other writes meanwhile also move, so it can be negative) |
| `shared` | object or null | `gc --apply` where it shares identical files afterwards (macOS, `dedup = true`): `{files, bytes}` |

Kind ids, cheapest to rebuild first (the README's
[What gets removed](../README.md#what-gets-removed) gives the rules):

| Kind | What |
|---|---|
| `stale_objects` | object files a later compile replaced |
| `old_sessions` | superseded incremental sessions |
| `legacy_layout` | old-layout files in a profile that also holds Cargo 1.100's layout |
| `idle_incremental` | incremental caches unused for `incremental-idle` |
| `idle_unit` | Cargo 1.100-layout build units unused for `unit-idle` that no unit in use was built from |
| `orphaned` | build directories whose workspaces were all deleted (also `remove --orphaned`) |
| `idle` | build directories unused for `dir-idle` |
| `low_space` | build directories removed because a disk was low on space |
| `requested` | build directories `remove` was asked to remove |
| `routed_tmp` | the TMPDIRs the `cargo` proxy routed for removed build directories |
| `leftover_tmp` | TMPDIRs the proxy routed whose build directory was already gone |

```json
{
  "version": 1,
  "command": "gc",
  "applied": true,
  "busy": false,
  "worktrees": [],
  "refused": [],
  "actions": [
    {
      "kind": "orphaned",
      "description": "build dirs of deleted workspaces",
      "path": "/Volumes/Scratch/app/cargo/fix-login",
      "files": 0,
      "bytes": 4294967296,
      "note": "workspaces deleted, unused for 3d"
    }
  ],
  "skipped": [
    {
      "kind": "idle",
      "description": "idle build dirs",
      "path": "/Volumes/Scratch/app/cargo/bench",
      "reason": "/Volumes/Scratch/app/cargo/bench/release/node is running from it (pid 4121)"
    }
  ],
  "totals": [
    { "kind": "orphaned", "description": "build dirs of deleted workspaces", "count": 1, "bytes": 4294967296 }
  ],
  "total_bytes": 4294967296,
  "freed_on_disk": [{ "path": "/Volumes/Scratch/app/cargo", "bytes": 3758096384 }],
  "shared": { "files": 0, "bytes": 0 }
}
```

## `dedup --json`

Identical files across build directories, replaced (with `--apply`) by
clones of one copy. Every group is listed, with or without `--verbose`.

| Field | Type | |
|---|---|---|
| `applied` | bool | `--apply`: replaced; otherwise a preview |
| `build_dirs` | integer | build directories examined |
| `files` | integer | duplicates replaced, or that would be |
| `bytes` | integer | their combined size: the most sharing gives back |
| `groups` | array | `{size_bytes, kept, replaced}`: each file's size, the copy kept, and the paths replaced by clones of it |
| `busy` | integer | duplicates skipped because a build held their directory's locks |
| `skipped` | integer | duplicates that changed since comparing, or couldn't be replaced |
| `unsupported_volumes` | integer | disks whose filesystem can't clone files |
| `freed_on_disk_bytes` | integer or null | `--apply`: the measured change in free space (other activity counts too) |

```json
{
  "version": 1,
  "applied": false,
  "build_dirs": 2,
  "files": 1,
  "bytes": 52428800,
  "groups": [
    {
      "size_bytes": 52428800,
      "kept": "/Volumes/Scratch/app/cargo/main/debug/deps/libserde-0f3c.rlib",
      "replaced": ["/Volumes/Scratch/app/cargo/fix-login/debug/deps/libserde-0f3c.rlib"]
    }
  ],
  "busy": 0,
  "skipped": 0,
  "unsupported_volumes": 0,
  "freed_on_disk_bytes": null
}
```

## `doctor --json`

Every check, grouped by section in the order the text shows them.

| Field | Type | |
|---|---|---|
| `counts` | object | `{ok, warn, info}`: how many checks have each level |
| `sections` | array | one per section, below |
| `sections[].id` | string | `settings`, `wrapper`, `toolchains`, `projects`, `compiler_cache`, `editors`, `disks` or `cleanup` |
| `sections[].title` | string | as the text shows it |
| `sections[].checks` | array | the section's checks, below |
| `checks[].level` | string | `"ok"`, `"warn"` (something to change) or `"info"` |
| `checks[].message` | string | what the check found; may span lines |
| `checks[].fix` | string | present when there is a fix: the command or change to make; may span lines |
| `checks[].fix_note` | string | present when the fix applies only in some cases, such as `volume roots` |

```json
{
  "version": 1,
  "counts": { "ok": 1, "warn": 1, "info": 0 },
  "sections": [
    {
      "id": "wrapper",
      "title": "Wrapper",
      "checks": [
        {
          "level": "warn",
          "message": "No rustc-wrapper is set in Cargo's user config.",
          "fix": "cargo steward setup"
        },
        { "level": "ok", "message": "Inner wrapper sccache found at /opt/homebrew/bin/sccache" }
      ]
    }
  ]
}
```

## `shim explain --json`

What the `cargo` proxy would do for a cargo command run in the current
directory. Put `--json` before the cargo arguments:
`cargo steward shim explain --json -- test -p foo`.

| Field | Type | |
|---|---|---|
| `settings` | object | `{path, exists, problems}`: the settings file (null without a home directory), whether it exists, and one message per setting it can't use (which then has its default) or key it doesn't know |
| `command` | array of strings | the cargo arguments (`["build"]` when none are given) |
| `routing_error` | string or null | why no plan could be made here; the plan's fields below are null then |
| `directory` | string or null | the directory the command works in |
| `checkout` | object or null | `{in_git, linked, worktree, git_dir}`: `linked` is a linked worktree (`git worktree add`); `worktree` is null inside a git directory |
| `namespace` | object or null | `{repo, key, path}`: the routed paths' `<repo>/<key>` below each root; empty strings when nothing is routed |
| `target_dir` | object or null | `{routed, path, reason, free_bytes}`: the `CARGO_TARGET_DIR` the proxy sets and the free space on the disk that holds it (or will, before it exists), or `reason` why it sets none |
| `tmp_dir` | object or null | `{routed, path, reason, free_bytes}`: likewise for `TMPDIR` |
| `seed` | object or null | `{status, will_seed, from, message}`, below |
| `route` | string or null | the `[[routes]]` path that applies |
| `real_cargo` | string or null | the cargo the proxy runs; null when it can't find one |
| `build_slot` | object | `{status, limit, dir, message}`, below |

`seed.status` is one of `will_seed` (`from` is the build directory it clones),
`all_busy` (builds hold every build it could clone; seeding waits for one),
`off` (`seed = false`), `nothing_routed`, `target_not_empty`, `source_missing`
(no other checkout of the repository has built yet) or `not_linked_worktree`.
`from` is null unless it will seed. `build_slot.status` is one of
`takes_slot` (with `limit` and the slot directory `dir`), `no_limit`
(`max-builds` isn't set) or `not_a_build`.

```json
{
  "version": 1,
  "settings": { "path": "/Users/me/.cargo/steward/config.toml", "exists": true, "problems": [] },
  "command": ["build"],
  "routing_error": null,
  "directory": "/Users/me/src/app/.worktrees/fix-login",
  "checkout": {
    "in_git": true,
    "linked": true,
    "worktree": "/Users/me/src/app/.worktrees/fix-login",
    "git_dir": "/Users/me/src/app/.git/worktrees/fix-login"
  },
  "namespace": { "repo": "app-3f9a1c0b2e", "key": "fix-login", "path": "app-3f9a1c0b2e/fix-login" },
  "target_dir": {
    "routed": true,
    "path": "/Volumes/Scratch/cargo/app-3f9a1c0b2e/fix-login",
    "reason": null,
    "free_bytes": 212849876992
  },
  "tmp_dir": { "routed": false, "path": null, "reason": "tmp-root isn't set", "free_bytes": null },
  "seed": {
    "status": "will_seed",
    "will_seed": true,
    "from": "/Volumes/Scratch/cargo/app-3f9a1c0b2e/main",
    "message": "yes, from /Volumes/Scratch/cargo/app-3f9a1c0b2e/main (the main checkout, last used 2h ago)"
  },
  "route": null,
  "real_cargo": "/Users/me/.cargo/bin/cargo",
  "build_slot": {
    "status": "takes_slot",
    "limit": 4,
    "dir": "/Users/me/.cargo/steward/slots",
    "message": "takes one of 4 (in /Users/me/.cargo/steward/slots)"
  }
}
```
