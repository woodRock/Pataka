#!/usr/bin/env bash
# Install a git pre-push hook that runs scripts/check.sh, standing in for CI.
source "$(dirname "$0")/common.sh"
hook="$ROOT/.git/hooks/pre-push"
cat > "$hook" <<'HOOK'
#!/usr/bin/env bash
exec "$(git rev-parse --show-toplevel)/scripts/check.sh"
HOOK
chmod +x "$hook"
log "pre-push hook installed: $hook"
