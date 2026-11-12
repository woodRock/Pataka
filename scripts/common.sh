# Shared helpers for the build scripts. Source, don't run.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
RELEASE="$ROOT/release"
log() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "$1 is required. $2"; }
npm_deps() {
  need node "Install Node.js 20 or later."
  need npm "Install Node.js 20 or later."
  need cargo "Install Rust from https://rustup.rs"
  (cd "$ROOT" && npm ci --no-audit --no-fund)
}
