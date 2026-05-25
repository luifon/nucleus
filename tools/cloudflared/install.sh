#!/usr/bin/env bash
# Substitute placeholders into the combined cloudflared template and write the
# real config. Pulls hostnames from `.env` and the TUNNEL_UUID env var.
#
# Usage:
#   TUNNEL_UUID=<uuid> ./tools/cloudflared/install.sh
#
# Placeholders substituted:
#   __USER_HOME__          → $HOME
#   __TUNNEL_UUID__        → $TUNNEL_UUID (required)
#   __NUCLEUS_HOSTNAME__   → host portion of NUCLEUS_PUBLIC_URL
#   __CONTAINERS_HOSTNAME__→ host portion of NUCLEUS_CONTAINERS_PUBLIC_URL
#
# Output: tools/cloudflared/nucleus.yaml (gitignored). Copy it to the location
# cloudflared reads (the combined ~/.cloudflared/config.yml) and restart the
# tunnel:
#   cp tools/cloudflared/nucleus.yaml ~/.cloudflared/config.yml

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Load .env if present so the *_PUBLIC_URL vars are visible without manual export.
if [ -f "$WORKSPACE_ROOT/.env" ]; then
  set -a
  # shellcheck disable=SC1091
  source "$WORKSPACE_ROOT/.env"
  set +a
fi

: "${TUNNEL_UUID:?TUNNEL_UUID is required (cloudflared tunnel create <name>; look up the UUID)}"

# Extract `host` from a URL like https://foo.bar/path → foo.bar.
host_from_url() {
  local url="$1"
  [ -z "$url" ] && return 1
  echo "${url}" | sed -E 's#^[a-z]+://##; s#/.*$##'
}

NUCLEUS_HOSTNAME="$(host_from_url "${NUCLEUS_PUBLIC_URL:-}")"
CONTAINERS_HOSTNAME="$(host_from_url "${NUCLEUS_CONTAINERS_PUBLIC_URL:-}")"

: "${NUCLEUS_HOSTNAME:?NUCLEUS_PUBLIC_URL not set in .env}"
: "${CONTAINERS_HOSTNAME:?NUCLEUS_CONTAINERS_PUBLIC_URL not set in .env}"

template="$SCRIPT_DIR/nucleus.yaml.example"
dest="$SCRIPT_DIR/nucleus.yaml"
sed \
  -e "s|__USER_HOME__|$HOME|g" \
  -e "s|__TUNNEL_UUID__|$TUNNEL_UUID|g" \
  -e "s|__NUCLEUS_HOSTNAME__|$NUCLEUS_HOSTNAME|g" \
  -e "s|__CONTAINERS_HOSTNAME__|$CONTAINERS_HOSTNAME|g" \
  "$template" > "$dest"
echo "wrote $dest (nucleus=$NUCLEUS_HOSTNAME, containers=$CONTAINERS_HOSTNAME, tunnel=$TUNNEL_UUID)"
echo "next: cp $dest ~/.cloudflared/config.yml && restart cloudflared"
