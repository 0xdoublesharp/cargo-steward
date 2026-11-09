# Agent guidance

`AGENTS.md` links here; keep the shared file rather than duplicating it.
See [README.md](README.md) for what the tool does and how it is tested.

## Code

- The wrapper (`src/wrapper.rs`) runs in front of every compile on the machines
  that install it. It must never fail, slow, or change a build: every step
  after rustc is best effort, and anything that compiles nothing is `exec`ed
  straight to rustc.
- Remove nothing from a build directory without holding cargo's locks on it
  (`fsutil::lock_profile`). Remove directories with `fsutil::remove`, which
  renames them aside first.
- Treat `incremental/<crate>-<id>/` as indivisible; deleting part of a session
  can crash rustc.
- Cargo's build-dir layout is internal and changes (1.100 reorganized it).
  Support each known layout explicitly in `src/scan.rs`, and in a layout we
  don't recognize, only act on whole directories.
- No `unwrap()` outside tests. No `unsafe` without a `// SAFETY:` comment.
- Add a regression test for each bug fix. Run `cargo fmt`, `cargo clippy
  --all-targets -- -D warnings` and `cargo test` before committing.

## Working preferences

- Preserve unrelated local work and commit by cohesive change.
- Keep build output and temporary files off the repository: machine-specific
  settings stay untracked.
- Release history belongs in `CHANGELOG.md`.
- Releases include verifying the GitHub Action's Marketplace listing. After
  the release workflow succeeds, confirm the new version is listed as latest;
  the existing listing can pick up published releases automatically. If the
  version is missing, use the release editor to enable Marketplace publication.
  Do not report a release complete until the listing is verified; see
  `docs/releasing.md`.
