#!/usr/bin/env bash
# The checks a CI server would run. Run before every commit or push
# (scripts/install-hooks.sh adds a pre-push hook that runs this).
source "$(dirname "$0")/common.sh"
cd "$ROOT/src-tauri"
log "rustfmt"
cargo fmt --check
log "clippy (warnings are errors)"
cargo clippy --all-targets --locked -- -D warnings
log "tests"
cargo test --locked
if command -v cargo-audit >/dev/null 2>&1; then
  log "cargo audit (known vulnerabilities in dependencies)"
  cargo audit
else
  log "skipping cargo audit: install it with 'cargo install cargo-audit --locked'"
fi
if command -v cargo-deny >/dev/null 2>&1; then
  log "cargo deny (licences, advisories, sources)"
  cargo deny check
else
  log "skipping cargo deny: install it with 'cargo install cargo-deny --locked'"
fi
log "UI: no inline scripts or styles (the CSP forbids them)"
if grep -nE '<script>|<style|style="|onclick=|javascript:' "$ROOT/ui/index.html"; then
  die "inline script or style found in ui/index.html"
fi
log "UI: model output is never parsed as HTML"
if grep -nE 'innerHTML|outerHTML|insertAdjacentHTML|document\.write|eval\(|new Function' "$ROOT"/ui/*.js; then
  die "unsafe DOM API found in ui/"
fi
log "all checks passed"
