#!/usr/bin/env bash
# Generate the real Caddyfile + LaunchDaemon plist from the templates,
# substituting values from .env and the live tailnet IP. See ADR-011.
#
# Usage:
#   ./tools/caddy/install.sh
#
# Requires in .env (or the environment):
#   NUCLEUS_PUBLIC_URL   — e.g. https://nucleus.<yourdomain>
#   CF_API_TOKEN         — Cloudflare token with Zone:DNS:Edit on the zone
# Optional:
#   NUCLEUS_LAUNCHD_PREFIX (default dev.nucleus)
#
# Output (both gitignored):
#   tools/caddy/Caddyfile
#   tools/caddy/<prefix>.caddy.plist
# Then load per the instructions this prints.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

if [ -f "$WORKSPACE_ROOT/.env" ]; then
  set -a
  # shellcheck disable=SC1091
  source "$WORKSPACE_ROOT/.env"
  set +a
fi

host_from_url() { echo "${1:-}" | sed -E 's#^[a-z]+://##; s#/.*$##'; }

NUCLEUS_HOSTNAME="$(host_from_url "${NUCLEUS_PUBLIC_URL:-}")"
TAILNET_IP="$(tailscale ip -4 2>/dev/null | head -1 || true)"
PREFIX="${NUCLEUS_LAUNCHD_PREFIX:-dev.nucleus}"

: "${NUCLEUS_HOSTNAME:?NUCLEUS_PUBLIC_URL not set in .env}"
: "${TAILNET_IP:?could not read tailnet IP — is 'tailscale up' done?}"
: "${CF_API_TOKEN:?CF_API_TOKEN not set (Cloudflare token, Zone:DNS:Edit)}"

# Caddyfile (no secrets — token stays in the plist env).
sed \
  -e "s|__NUCLEUS_HOSTNAME__|$NUCLEUS_HOSTNAME|g" \
  -e "s|__TAILNET_IP__|$TAILNET_IP|g" \
  "$SCRIPT_DIR/Caddyfile.example" > "$SCRIPT_DIR/Caddyfile"
echo "wrote $SCRIPT_DIR/Caddyfile (host=$NUCLEUS_HOSTNAME, bind=$TAILNET_IP)"

# LaunchDaemon plist (carries the token in its EnvironmentVariables; lands in
# a gitignored file + /Library/LaunchDaemons, never the repo).
plist_dest="$SCRIPT_DIR/$PREFIX.caddy.plist"
sed \
  -e "s|__LAUNCHD_PREFIX__|$PREFIX|g" \
  -e "s|__USER_HOME__|$HOME|g" \
  -e "s|__CF_API_TOKEN__|$CF_API_TOKEN|g" \
  "$SCRIPT_DIR/caddy.plist.example" > "$plist_dest"
chmod 600 "$plist_dest"
echo "wrote $plist_dest (chmod 600 — contains the CF token)"

cat <<EOF

next (needs sudo — Caddy binds :443):
  sudo cp $plist_dest /Library/LaunchDaemons/$PREFIX.caddy.plist
  sudo chown root:wheel /Library/LaunchDaemons/$PREFIX.caddy.plist
  sudo launchctl bootstrap system /Library/LaunchDaemons/$PREFIX.caddy.plist
  # tail: tail -f $WORKSPACE_ROOT/memory/caddy.log
EOF
