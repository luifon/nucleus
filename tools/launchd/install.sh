#!/usr/bin/env bash
# Install Nucleus launchd plists.
#
# Each plist template at tools/launchd/<service>.plist.example is rendered
# by substituting __USER_HOME__ → $HOME and __LAUNCHD_PREFIX__ → the value
# of NUCLEUS_LAUNCHD_PREFIX (default: dev.nucleus). The output is written
# to ~/Library/LaunchAgents/<prefix>.<service>.plist and loaded via launchctl.
#
# Usage:
#   ./tools/launchd/install.sh                   # install all
#   ./tools/launchd/install.sh discord           # install one (substring match)
#   ./tools/launchd/install.sh --uninstall       # unload + remove all
#   NUCLEUS_LAUNCHD_PREFIX=tech.mycompany ./tools/launchd/install.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
DEST="$HOME/Library/LaunchAgents"
mkdir -p "$DEST"

# Load .env if present so NUCLEUS_LAUNCHD_PREFIX is visible without manual export.
if [ -f "$WORKSPACE_ROOT/.env" ]; then
  set -a
  # shellcheck disable=SC1091
  source "$WORKSPACE_ROOT/.env"
  set +a
fi

PREFIX="${NUCLEUS_LAUNCHD_PREFIX:-dev.nucleus}"

# Resolve the timezone for plists' StartCalendarInterval scheduling.
# launchd locks in the timezone when it loads a plist — if you change
# /etc/localtime later, scheduled jobs keep firing on the OLD timezone
# until you reload them. To make this less of a footgun:
#  1. We inject TZ into each plist's EnvironmentVariables, so the spawned
#     process always has the right TZ for any time formatting it does.
#  2. install.sh always unloads + reloads on every run, so even if the
#     system TZ has changed, re-running install.sh re-binds the schedule.
# Auto-detect from /etc/localtime if NUCLEUS_TZ isn't set.
if [ -z "${NUCLEUS_TZ:-}" ]; then
  NUCLEUS_TZ="$(readlink /etc/localtime 2>/dev/null | sed 's|.*zoneinfo/||')"
  NUCLEUS_TZ="${NUCLEUS_TZ:-UTC}"
fi
echo "using timezone: $NUCLEUS_TZ"

uninstall() {
  for plist in "$DEST"/${PREFIX}.*.plist; do
    [ -f "$plist" ] || continue
    echo "unloading $(basename "$plist")"
    launchctl unload "$plist" 2>/dev/null || true
    rm "$plist"
  done
}

if [ "${1:-}" = "--uninstall" ]; then
  uninstall
  exit 0
fi

FILTER="${1:-}"
for template in "$SCRIPT_DIR"/*.plist.example; do
  service="$(basename "$template" .plist.example)"
  if [ -n "$FILTER" ] && [[ "$service" != *"$FILTER"* ]]; then
    continue
  fi
  dest="$DEST/${PREFIX}.${service}.plist"
  sed \
    -e "s|__USER_HOME__|$HOME|g" \
    -e "s|__LAUNCHD_PREFIX__|$PREFIX|g" \
    -e "s|__TZ__|$NUCLEUS_TZ|g" \
    "$template" > "$dest"
  launchctl unload "$dest" 2>/dev/null || true
  launchctl load "$dest"
  echo "installed ${PREFIX}.${service}"
done
