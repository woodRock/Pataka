#!/usr/bin/env bash
# Write release/SHA256SUMS for every built installer.
source "$(dirname "$0")/common.sh"
cd "$RELEASE"
find . -type f \( -name '*.deb' -o -name '*.rpm' -o -name '*.AppImage' -o -name '*.exe' -o -name '*.msi' -o -name '*.dmg' -o -name '*.zip' \) \
  | sed 's|^\./||' | sort | while read -r f; do
  if command -v sha256sum >/dev/null; then sha256sum "$f"; else shasum -a 256 "$f"; fi
done > SHA256SUMS
log "wrote $RELEASE/SHA256SUMS"
cat SHA256SUMS
