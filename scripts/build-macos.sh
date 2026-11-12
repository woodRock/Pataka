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
log "building the pataka command (universal)"
cargo build --release --locked -p pataka-cli --target aarch64-apple-darwin
cargo build --release --locked -p pataka-cli --target x86_64-apple-darwin
mkdir -p "$ROOT/src-tauri/binaries"
lipo -create -output "$ROOT/src-tauri/binaries/pataka-cli-universal-apple-darwin" \
  "$ROOT/target/aarch64-apple-darwin/release/pataka" "$ROOT/target/x86_64-apple-darwin/release/pataka"
codesign --force --sign "${APPLE_SIGNING_IDENTITY:--}" "$ROOT/src-tauri/binaries/pataka-cli-universal-apple-darwin"
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
mkdir -p "$RELEASE/cli"
rm -f "$RELEASE"/cli/pataka-*-universal-apple-darwin
cp "$ROOT/src-tauri/binaries/pataka-cli-universal-apple-darwin" "$RELEASE/cli/pataka-$VERSION-universal-apple-darwin"
"$ROOT/scripts/checksums.sh"
log "done: $out"
