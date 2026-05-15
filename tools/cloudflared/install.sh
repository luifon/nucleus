#!/usr/bin/env bash
# Substitute placeholders into the cloudflared yaml templates and write the
# real configs to the same dir. Pulls values from `.env` and a per-template
# TUNNEL_UUID env var.
#
# Usage:
#   TUNNEL_UUID=<uuid> ./tools/cloudflared/install.sh news
#   TUNNEL_UUID=<uuid> ./tools/cloudflared/install.sh dashboard
#   TUNNEL_UUID=<uuid> ./tools/cloudflared/install.sh        # all templates
#
# Placeholders substituted:
#   __USER_HOME__         → $HOME
#   __TUNNEL_UUID__       → $TUNNEL_UUID (required)
#   __NEWS_HOSTNAME__     → host portion of NUCLEUS_NEWS_PUBLIC_URL
#   __DASHBOARD_HOSTNAME__→ host portion of NUCLEUS_DASHBOARD_PUBLIC_URL
#   __CHAT_HOSTNAME__     → host portion of NUCLEUS_CHAT_PUBLIC_URL
#
# Output: real *.yaml files alongside the *.yaml.example. These are
# gitignored (see .gitignore). After generating, point cloudflared at them:
#   cloudflared tunnel --config tools/cloudflared/news.yaml run

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
  # strip scheme + path
  echo "${url}" | sed -E 's#^[a-z]+://##; s#/.*$##'
}

NEWS_HOSTNAME="$(host_from_url "${NUCLEUS_NEWS_PUBLIC_URL:-}")"
DASHBOARD_HOSTNAME="$(host_from_url "${NUCLEUS_DASHBOARD_PUBLIC_URL:-}")"
CHAT_HOSTNAME="$(host_from_url "${NUCLEUS_CHAT_PUBLIC_URL:-}")"

FILTER="${1:-}"
for template in "$SCRIPT_DIR"/*.yaml.example; do
  base="$(basename "$template" .example)"
  stem="${base%.yaml}"
  if [ -n "$FILTER" ] && [[ "$stem" != *"$FILTER"* ]]; then
    continue
  fi
  case "$stem" in
    news)
      if [ -z "$NEWS_HOSTNAME" ]; then
        echo "skipping $base — NUCLEUS_NEWS_PUBLIC_URL not set" >&2
        continue
      fi
      hostname="$NEWS_HOSTNAME"
      placeholder="__NEWS_HOSTNAME__"
      ;;
    dashboard)
      if [ -z "$DASHBOARD_HOSTNAME" ]; then
        echo "skipping $base — NUCLEUS_DASHBOARD_PUBLIC_URL not set" >&2
        continue
      fi
      hostname="$DASHBOARD_HOSTNAME"
      placeholder="__DASHBOARD_HOSTNAME__"
      ;;
    chat)
      if [ -z "$CHAT_HOSTNAME" ]; then
        echo "skipping $base — NUCLEUS_CHAT_PUBLIC_URL not set" >&2
        continue
      fi
      hostname="$CHAT_HOSTNAME"
      placeholder="__CHAT_HOSTNAME__"
      ;;
    *)
      echo "skipping $base — no hostname mapping known" >&2
      continue
      ;;
  esac

  dest="$SCRIPT_DIR/$base"
  sed \
    -e "s|__USER_HOME__|$HOME|g" \
    -e "s|__TUNNEL_UUID__|$TUNNEL_UUID|g" \
    -e "s|$placeholder|$hostname|g" \
    "$template" > "$dest"
  echo "wrote $dest (hostname=$hostname, tunnel=$TUNNEL_UUID)"
done
