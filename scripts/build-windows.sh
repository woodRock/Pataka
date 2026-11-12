#!/usr/bin/env bash
# Cross-compile Pātaka for Windows (x86_64) from Linux: an NSIS installer
# into release/windows. On Windows itself, use scripts/build-windows.ps1.
# Needs: rustup target add x86_64-pc-windows-gnu
#        sudo apt install mingw-w64 nsis
source "$(dirname "$0")/common.sh"
need x86_64-w64-mingw32-gcc "Install mingw-w64."
need makensis "Install nsis."
rustup target list --installed | grep -q x86_64-pc-windows-gnu || rustup target add x86_64-pc-windows-gnu
npm_deps
cd "$ROOT"
log "building the pataka command for Windows"
cargo build --release --locked -p pataka-cli --target x86_64-pc-windows-gnu
mkdir -p "$ROOT/src-tauri/binaries"
cp "$ROOT/target/x86_64-pc-windows-gnu/release/pataka.exe" "$ROOT/src-tauri/binaries/pataka-cli-x86_64-pc-windows-gnu.exe"
log "building Pātaka $VERSION for Windows (cross-compiled)"
npx tauri build --target x86_64-pc-windows-gnu --bundles nsis
out="$RELEASE/windows"
rm -rf "$out" && mkdir -p "$out"
cp "$ROOT"/target/x86_64-pc-windows-gnu/release/bundle/nsis/*_"$VERSION"_*.exe "$out"/
mkdir -p "$RELEASE/cli"
rm -f "$RELEASE"/cli/pataka-*-x86_64-pc-windows-gnu.exe
cp "$ROOT/target/x86_64-pc-windows-gnu/release/pataka.exe" "$RELEASE/cli/pataka-$VERSION-x86_64-pc-windows-gnu.exe"
"$ROOT/scripts/checksums.sh"
log "done: $out"
