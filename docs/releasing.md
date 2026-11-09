# Releasing Cargo Steward

The crate, command-line binary and GitHub Action share the version in
`Cargo.toml`. A release tag such as `v0.2.0` must match that version.

## Prepare and validate

1. Update `Cargo.toml`, the package entry in `Cargo.lock`, the CHANGELOG and
   versioned installation/Action examples. Keep published tags immutable.
2. Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo test` and `shellcheck scripts/install.sh`. Use the project's build
   output routing and temporary directory policy.
3. Commit the complete bundle and push `main` once. Wait for CI at that
   commit: Linux/macOS tests, beta/nightly layout checks, lint and installation.
4. After CI passes, tag that commit and push the tag:

   ```sh
   git tag -a v0.2.0 -m "Cargo Steward 0.2.0"
   git push origin v0.2.0
   ```

Pushing the tag runs the release workflow. The main-branch CI workflow does
not run again for tags.

## What the release workflow publishes

[release.yml](../.github/workflows/release.yml) builds native binaries for
Apple Silicon and Intel macOS, and x86-64 and ARM64 Linux. Each archive has
a SHA-256 sidecar file. Once every build succeeds, it publishes the crate
and attaches the binaries to the GitHub release.

crates.io Trusted Publishing exchanges the workflow's GitHub OIDC identity
for a short-lived token. The publisher configuration names this repository,
`release.yml` and the GitHub environment `release`; no long-lived crates.io
token is needed. The workflow skips publishing a version already on crates.io.

To rebuild binaries for an existing tag, manually run the release workflow
with its `tag` input. A manual run skips crate publication.

## GitHub Marketplace

The versioned tag makes the Action usable as
`0xdoublesharp/cargo-steward@v0.2.0`. To add that release to its Marketplace
listing, edit the GitHub release, select **Publish this Action to the GitHub
Marketplace**, confirm its categories and update the release. The account
owner must have accepted GitHub's Marketplace Developer Agreement.

See GitHub's [Marketplace publishing guide](https://docs.github.com/en/actions/how-tos/create-and-publish-actions/publish-in-github-marketplace).

## Verify publication

- Check that crates.io reports the new version and that it is not yanked.
- Check that the GitHub release has four binary archives and four checksum
  files, and that the Marketplace version selector offers the new version.
- Install a prebuilt binary with `scripts/install.sh --release 0.2.0
  --repo 0xdoublesharp/cargo-steward --no-setup` in an isolated Cargo home,
  then check `cargo-steward --version`.
- Use the [CI guide](ci.md) to check the Action's start/build/trim order.
  Cost-aware eviction needs direct-compiler observations from the wrapper;
  existing caches without observations use LRU.

If a published version needs a code correction, release a new version.
