# Changelog

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- `rustc-wrapper` mode that removes the codegen-unit object files each
  incremental rebuild replaces (rust-lang/rust#161824), for Rust 1.88 and
  later in both build-dir layouts, and runs rustc through an inner wrapper
  such as sccache.
- `cargo steward gc`: removes stale object files, old-layout build files after
  an upgrade to Cargo 1.100, idle incremental caches and build units, build
  directories of deleted workspaces or unused for weeks, and least recently
  used build directories when a disk runs low. Reports unless given `--apply`.
- Automatic background cleanup after builds, with no daemon.
- `cargo steward setup` (and `--undo`), `doctor`, `status`, `pin` and `unpin`.
