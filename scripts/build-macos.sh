#!/usr/bin/env bash
# Build Pātaka for macOS on a Mac: a universal (Apple silicon + Intel)
# .app and .dmg into release/macos. Apple does not allow building macOS
# apps on other systems, so this one has to run on a Mac.
# Needs: Xcode command line tools (xcode-select --install), Rust, Node.js 20+.
source "$(dirname "$0")/common.sh"
[[ "$(uname -s)" == Darwin ]] || die "run this on macOS"
rustup target add aarch64-apple-darwin x86_64-apple-darwin
npm_deps
cd "$ROOT"
log "building Pātaka $VERSION for macOS (universal)"
# Without an Apple Developer ID, sign ad hoc so the app runs on this Mac.
# Set APPLE_SIGNING_IDENTITY (and notarisation variables) to sign for others.
export APPLE_SIGNING_IDENTITY="${APPLE_SIGNING_IDENTITY:--}"
npx tauri build --target universal-apple-darwin --bundles app,dmg
out="$RELEASE/macos"
rm -rf "$out" && mkdir -p "$out"
b="$ROOT/target/universal-apple-darwin/release/bundle"
cp "$b"/dmg/*_"$VERSION"_*.dmg "$out"/
(cd "$b/macos" && ditto -c -k --keepParent Pataka.app "$out/Pataka_${VERSION}_universal.app.zip")
"$ROOT/scripts/checksums.sh"
log "done: $out"
