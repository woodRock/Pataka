#!/usr/bin/env bash
# Build Pātaka for Linux (x86_64): .deb, .rpm and .AppImage, into release/linux.
# Needs (Debian/Ubuntu): sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev \
#   libayatana-appindicator3-dev librsvg2-dev libdbus-1-dev build-essential file patchelf
source "$(dirname "$0")/common.sh"
[[ "$(uname -s)" == Linux ]] || die "run this on Linux"
npm_deps
cd "$ROOT"
log "building the pataka command"
cargo build --release --locked -p pataka-cli
mkdir -p "$ROOT/src-tauri/binaries"
cp "$ROOT/target/release/pataka" "$ROOT/src-tauri/binaries/pataka-cli-x86_64-unknown-linux-gnu"
log "building Pātaka $VERSION for Linux"
npx tauri build --bundles deb,rpm,appimage
out="$RELEASE/linux"
rm -rf "$out" && mkdir -p "$out"
b="$ROOT/target/release/bundle"
cp "$b"/deb/*_"$VERSION"_*.deb "$b"/rpm/*-"$VERSION"-*.rpm "$b"/appimage/*_"$VERSION"_*.AppImage "$out"/
mkdir -p "$RELEASE/cli"
rm -f "$RELEASE"/cli/pataka-*-x86_64-unknown-linux-gnu
cp "$ROOT/target/release/pataka" "$RELEASE/cli/pataka-$VERSION-x86_64-unknown-linux-gnu"
"$ROOT/scripts/checksums.sh"
log "done: $out"
