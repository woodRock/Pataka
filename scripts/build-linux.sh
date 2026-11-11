#!/usr/bin/env bash
# Build Pātaka for Linux (x86_64): .deb, .rpm and .AppImage, into release/linux.
# Needs (Debian/Ubuntu): sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev \
#   libayatana-appindicator3-dev librsvg2-dev libdbus-1-dev build-essential file patchelf
source "$(dirname "$0")/common.sh"
[[ "$(uname -s)" == Linux ]] || die "run this on Linux"
npm_deps
cd "$ROOT"
log "building Pātaka $VERSION for Linux"
npx tauri build --bundles deb,rpm,appimage
out="$RELEASE/linux"
rm -rf "$out" && mkdir -p "$out"
b="$ROOT/src-tauri/target/release/bundle"
cp "$b"/deb/*_"$VERSION"_*.deb "$b"/rpm/*-"$VERSION"-*.rpm "$b"/appimage/*_"$VERSION"_*.AppImage "$out"/
"$ROOT/scripts/checksums.sh"
log "done: $out"
