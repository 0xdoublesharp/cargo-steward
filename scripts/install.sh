#!/bin/sh
# Installs cargo-steward, then runs `cargo steward setup` and
# `cargo steward doctor`. macOS and Linux.
#
# Usage: scripts/install.sh [SOURCE] [--no-setup] [--] [SETUP OPTIONS...]
#
# SOURCE is one of:
#   --from-source DIR       cargo install --locked --path DIR
#                           (default: the checkout this script is in)
#   --git URL [--rev REV]   cargo install --locked --git URL [--rev REV]
#   --release VERSION --repo OWNER/NAME
#                           download the prebuilt binary for this machine from
#                           that GitHub repository's release vVERSION (or
#                           `latest`), check its SHA-256 and install it to
#                           ${CARGO_HOME:-~/.cargo}/bin
#
# Other options:
#   --no-setup              install only; don't run `cargo steward setup`
#   -h, --help              show this help
#
# Anything else is passed to `cargo steward setup`, for example
#   scripts/install.sh --scan-root /Volumes/Scratch
#   scripts/install.sh --release 0.1.0 --repo OWNER/cargo-steward -- --volume Scratch
#
# CARGO_STEWARD_RELEASE_URL replaces https://github.com/OWNER/NAME/releases
# (for a mirror).

set -eu

say() { printf '%s\n' "$*"; }
die() {
    printf 'install.sh: error: %s\n' "$*" >&2
    exit 1
}

usage() {
    if [ -f "$0" ]; then
        sed -n '2,/^$/s/^# \{0,1\}//p' "$0"
    else
        say "Usage: install.sh [--from-source DIR | --git URL [--rev REV] | --release VERSION --repo OWNER/NAME] [--no-setup] [SETUP OPTIONS...]"
    fi
}

need_value() {
    [ "$2" -gt 0 ] || die "$1 needs a value"
}

mode=
source_dir=
git_url=
git_rev=
version=
repo=
run_setup=1

# Options this script knows are consumed; everything else is kept, in
# order, as the arguments for `cargo steward setup`.
n=$#
while [ "$n" -gt 0 ]; do
    arg=$1
    shift
    n=$((n - 1))
    case $arg in
        --from-source)
            need_value "$arg" "$n"
            mode=source source_dir=$1
            shift
            n=$((n - 1))
            ;;
        --git)
            need_value "$arg" "$n"
            mode=git git_url=$1
            shift
            n=$((n - 1))
            ;;
        --rev)
            need_value "$arg" "$n"
            git_rev=$1
            shift
            n=$((n - 1))
            ;;
        --release)
            need_value "$arg" "$n"
            mode=release version=$1
            shift
            n=$((n - 1))
            ;;
        --repo)
            need_value "$arg" "$n"
            repo=$1
            shift
            n=$((n - 1))
            ;;
        --no-setup)
            run_setup=
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        --)
            while [ "$n" -gt 0 ]; do
                set -- "$@" "$1"
                shift
                n=$((n - 1))
            done
            ;;
        *)
            set -- "$@" "$arg"
            ;;
    esac
done

if [ -n "$git_rev" ] && [ "$mode" != git ]; then
    die "--rev only applies to --git"
fi
if [ -n "$repo" ] && [ "$mode" != release ]; then
    die "--repo only applies to --release"
fi

# Run from a checkout, install that checkout.
if [ -z "$mode" ]; then
    script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
    checkout=$(dirname -- "$script_dir")
    if [ -f "$checkout/Cargo.toml" ] && grep -q '^name = "cargo-steward"' "$checkout/Cargo.toml"; then
        mode=source source_dir=$checkout
    else
        die "not run from a cargo-steward checkout: pass --from-source DIR, --git URL, or --release VERSION --repo OWNER/NAME"
    fi
fi

cargo_home=${CARGO_HOME:-$HOME/.cargo}

# The target triple of this machine's prebuilt binary.
detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case $arch in
        arm64 | aarch64) arch=aarch64 ;;
        x86_64 | amd64)
            # An x86_64 shell under Rosetta on Apple silicon.
            if [ "$os" = Darwin ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = 1 ]; then
                arch=aarch64
            else
                arch=x86_64
            fi
            ;;
        *) die "no prebuilt binary for $arch; use --from-source or --git" ;;
    esac
    case $os in
        Darwin) echo "$arch-apple-darwin" ;;
        Linux) echo "$arch-unknown-linux-gnu" ;;
        *) die "no prebuilt binary for $os; use --from-source or --git" ;;
    esac
}

download() {
    if command -v curl >/dev/null 2>&1; then
        curl --proto '=https,file' --tlsv1.2 -fsSL -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        die "neither curl nor wget is installed"
    fi
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        die "neither sha256sum nor shasum is installed"
    fi
}

need_cargo() {
    command -v cargo >/dev/null 2>&1 || die "cargo isn't installed (see https://rustup.rs), or use --release"
}

case $mode in
    source)
        need_cargo
        [ -f "$source_dir/Cargo.toml" ] || die "$source_dir has no Cargo.toml"
        say "Installing cargo-steward from $source_dir"
        cargo install --locked --path "$source_dir"
        bin_dir=${CARGO_INSTALL_ROOT:-$cargo_home}/bin
        ;;
    git)
        need_cargo
        say "Installing cargo-steward from $git_url${git_rev:+ at $git_rev}"
        if [ -n "$git_rev" ]; then
            cargo install --locked --git "$git_url" --rev "$git_rev" cargo-steward
        else
            cargo install --locked --git "$git_url" cargo-steward
        fi
        bin_dir=${CARGO_INSTALL_ROOT:-$cargo_home}/bin
        ;;
    release)
        [ -n "$repo" ] || die "--release needs --repo OWNER/NAME"
        target=$(detect_target)
        base=${CARGO_STEWARD_RELEASE_URL:-https://github.com/$repo/releases}
        case $version in
            latest) url=$base/latest/download ;;
            *) url=$base/download/v${version#v} ;;
        esac
        asset=cargo-steward-$target.tar.gz
        tmp=$(mktemp -d)
        trap 'rm -rf "$tmp"' EXIT
        say "Downloading $url/$asset"
        download "$url/$asset" "$tmp/$asset"
        download "$url/$asset.sha256" "$tmp/$asset.sha256"
        expected=$(cut -d ' ' -f 1 <"$tmp/$asset.sha256")
        actual=$(sha256_of "$tmp/$asset")
        if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
            die "checksum mismatch for $asset: expected ${expected:-nothing}, got $actual"
        fi
        mkdir -p "$tmp/unpacked"
        tar -xzf "$tmp/$asset" -C "$tmp/unpacked"
        [ -f "$tmp/unpacked/cargo-steward" ] || die "$asset has no cargo-steward binary"
        bin_dir=$cargo_home/bin
        mkdir -p "$bin_dir"
        # Replace through a rename, so a build running the old binary isn't disturbed.
        cp "$tmp/unpacked/cargo-steward" "$bin_dir/.cargo-steward.new"
        chmod 755 "$bin_dir/.cargo-steward.new"
        mv -f "$bin_dir/.cargo-steward.new" "$bin_dir/cargo-steward"
        say "Installed $bin_dir/cargo-steward (checksum verified)"
        ;;
esac

bin=$bin_dir/cargo-steward
[ -x "$bin" ] || die "$bin wasn't installed"
"$bin" --version

case ":${PATH:-}:" in
    *":$bin_dir:"*) ;;
    *) say "Note: $bin_dir isn't on PATH; add it so \`cargo steward\` can find cargo-steward." ;;
esac

if [ -n "$run_setup" ]; then
    say ""
    "$bin" setup "$@"
elif [ $# -gt 0 ]; then
    say "Not running setup (--no-setup), so these were ignored: $*"
fi

say ""
"$bin" doctor || say "doctor failed; run \`cargo steward doctor\` again once the problem above is fixed"
