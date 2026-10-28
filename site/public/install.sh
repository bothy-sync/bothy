#!/bin/sh
# Installs the `bothy` command-line tool from the latest GitHub release.
#
#   curl -fsSL https://bothysync.dev/install.sh | sh
#
# It picks the archive for this OS and CPU, checks its SHA-256 against the release's
# SHA256SUMS, and puts `bothy` in ~/.local/bin (or $BOTHY_INSTALL_DIR). No root needed.
# $BOTHY_DOWNLOAD_URL replaces the release download URL (mirrors, tests).
# Source: https://github.com/bothy-sync/bothy/blob/main/site/public/install.sh
set -eu

repo="bothy-sync/bothy"
dir="${BOTHY_INSTALL_DIR:-$HOME/.local/bin}"

fail() {
    echo "install.sh: $*" >&2
    exit 1
}

case "$(uname -s)" in
    Darwin) target="universal-apple-darwin" ;;
    Linux)
        case "$(uname -m)" in
            x86_64 | amd64) target="x86_64-unknown-linux-gnu" ;;
            aarch64 | arm64) target="aarch64-unknown-linux-gnu" ;;
            armv7l | armv7 | armv8l) target="armv7-unknown-linux-gnueabihf" ;;
            *) fail "no prebuilt bothy for $(uname -m); install with: cargo install bothy-cli" ;;
        esac
        ;;
    *) fail "no prebuilt bothy for $(uname -s); on Windows, download the .zip from https://github.com/$repo/releases/latest" ;;
esac

if command -v curl > /dev/null 2>&1; then
    get() { curl -fsSL -o "$2" "$1"; }
elif command -v wget > /dev/null 2>&1; then
    get() { wget -q -O "$2" "$1"; }
else
    fail "needs curl or wget"
fi

if command -v sha256sum > /dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum > /dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
    fail "needs sha256sum or shasum"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

archive="bothy-$target.tar.gz"
base="${BOTHY_DOWNLOAD_URL:-https://github.com/$repo/releases/latest/download}"
echo "Downloading $archive ..."
get "$base/$archive" "$tmp/$archive" ||
    fail "cannot download $base/$archive (no release with prebuilt binaries yet?)"
get "$base/SHA256SUMS" "$tmp/SHA256SUMS" || fail "cannot download $base/SHA256SUMS"

want="$(awk -v f="$archive" '$2 == f { print $1 }' "$tmp/SHA256SUMS")"
[ -n "$want" ] || fail "$archive is not listed in SHA256SUMS"
[ "$(sha256 "$tmp/$archive")" = "$want" ] || fail "checksum mismatch for $archive; not installing"

tar -xzf "$tmp/$archive" -C "$tmp" bothy
mkdir -p "$dir"
mv "$tmp/bothy" "$dir/bothy"
chmod 755 "$dir/bothy"
echo "Installed $("$dir/bothy" --version) to $dir/bothy"

case ":$PATH:" in
    *":$dir:"*) ;;
    *) echo "Add $dir to your PATH, e.g.: echo 'export PATH=\"$dir:\$PATH\"' >> ~/.profile" ;;
esac
echo "Next: bothy auth login"
