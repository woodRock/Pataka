#!/bin/sh
# Install the `pataka` command: Claude Code with Pātaka's Tūmataiti
# redaction in the middle. Usage:
#
#   curl -fsSL https://raw.githubusercontent.com/woodRock/Pataka/main/install.sh | sh
#
# It downloads the prebuilt binary for this computer from the repository's
# release/ folder, checks it against release/SHA256SUMS, and puts it in
# ~/.local/bin (or $PATAKA_INSTALL_DIR). Nothing else is changed: run
# `pataka setup` afterwards.
set -eu

REPO="${PATAKA_REPO:-woodRock/Pataka}"
BASE="${PATAKA_BASE:-https://github.com/$REPO/raw/main/release}"
DEST="${PATAKA_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf 'pataka install: %s\n' "$*" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || die "curl is needed."
if command -v sha256sum >/dev/null 2>&1; then
  sha() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  die "sha256sum or shasum is needed to check the download."
fi

os=$(uname -s)
arch=$(uname -m)
case "$os-$arch" in
  Linux-x86_64 | Linux-amd64) target=x86_64-unknown-linux-gnu ;;
  Darwin-arm64 | Darwin-x86_64) target=universal-apple-darwin ;;
  *) die "there is no prebuilt pataka for $os $arch yet. With Rust installed: cargo install --git https://github.com/$REPO pataka-cli" ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

curl -fsSL "$BASE/SHA256SUMS" -o "$tmp/SHA256SUMS" || die "could not download the checksums."
line=$(grep " cli/pataka-[0-9][0-9.]*-$target\$" "$tmp/SHA256SUMS" | tail -n 1 || true)
[ -n "$line" ] || die "no pataka build for $target is published yet."
expected=$(printf '%s' "$line" | cut -d' ' -f1)
file=$(printf '%s' "$line" | awk '{print $2}')
version=$(printf '%s' "$file" | sed -n 's|cli/pataka-\([0-9.]*\)-.*|\1|p')

say "Downloading pataka $version for $target…"
curl -fSL --progress-bar "$BASE/$file" -o "$tmp/pataka" || die "the download failed."
actual=$(sha "$tmp/pataka")
[ "$actual" = "$expected" ] || die "the download does not match its SHA-256, so it was not installed."

mkdir -p "$DEST"
cp "$tmp/pataka" "$DEST/pataka.new"
chmod 755 "$DEST/pataka.new"
mv -f "$DEST/pataka.new" "$DEST/pataka"
say "Installed $DEST/pataka (SHA-256 checked)."

case ":$PATH:" in
  *":$DEST:"*) ;;
  *)
    profile="$HOME/.profile"
    [ -n "${ZSH_VERSION:-}" ] || [ "$(basename "${SHELL:-}")" = zsh ] && profile="$HOME/.zshrc"
    [ "$(basename "${SHELL:-}")" = bash ] && profile="$HOME/.bashrc"
    say ""
    say "$DEST is not on your PATH. Add it with:"
    say "  echo 'export PATH=\"$DEST:\$PATH\"' >> $profile"
    say "then open a new terminal."
    ;;
esac
say ""
say "Next: pataka setup"
