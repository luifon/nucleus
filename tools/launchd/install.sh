#!/usr/bin/env bash
# Install Nucleus launchd plists.
#
# Each plist template at tools/launchd/<service>.plist.example is rendered
# by substituting __USER_HOME__ → $HOME and __LAUNCHD_PREFIX__ → the value
# of NUCLEUS_LAUNCHD_PREFIX (default: dev.nucleus). The output is written
# to ~/Library/LaunchAgents/<prefix>.<service>.plist and loaded via
# `launchctl bootout` + `bootstrap` (the modern API; `load`/`unload` are
# deprecated).
#
# Orphan pruning (ADR-020, hard-cut policy): on every full run, any
# installed ${PREFIX}.*.plist with no matching .plist.example template is
# booted out and deleted — a service removed from tools/launchd/ dies on
# the next install run instead of lingering loaded forever (the fate of
# distiller-hourly/-weekly and preference-learner after ADR-016/017).
#
# Log rotation (ADR-020): a newsyslog policy for memory/*.log is rendered
# from tools/newsyslog/nucleus.conf.example and installed to
# /etc/newsyslog.d/nucleus.conf (sudo, only when the content changed).
#
# The Caddy perimeter (ADR-011) is also (re)installed when configured — it's
# a root LaunchDaemon, not a user agent, so that step uses sudo (it prompts).
# It's skipped unless the perimeter is set up (CF_API_TOKEN in .env +
# ~/.local/bin/caddy present), so installs on non-perimeter hosts are
# unaffected.
#
# Usage:
#   ./tools/launchd/install.sh                   # install all (+ prune orphans, newsyslog, caddy)
#   ./tools/launchd/install.sh discord           # install one (substring match)
#   ./tools/launchd/install.sh caddy             # just the caddy perimeter daemon
#   ./tools/launchd/install.sh newsyslog         # just the log-rotation config
#   ./tools/launchd/install.sh --uninstall       # boot out + remove all (incl. caddy, newsyslog)
#   NUCLEUS_LAUNCHD_PREFIX=tech.mycompany ./tools/launchd/install.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
DEST="$HOME/Library/LaunchAgents"
DOMAIN="gui/$(id -u)"
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
#  2. install.sh always boots out + re-bootstraps on every run, so even if
#     the system TZ has changed, re-running install.sh re-binds the schedule.
# Auto-detect from /etc/localtime if NUCLEUS_TZ isn't set.
if [ -z "${NUCLEUS_TZ:-}" ]; then
  NUCLEUS_TZ="$(readlink /etc/localtime 2>/dev/null | sed 's|.*zoneinfo/||')"
  NUCLEUS_TZ="${NUCLEUS_TZ:-UTC}"
fi
echo "using timezone: $NUCLEUS_TZ"

# Caddy is a root LaunchDaemon (binds :443), so it's managed separately from
# the user LaunchAgents above: rendered via tools/caddy/install.sh and
# bootstrapped into the system domain with sudo. Only touched when the
# perimeter is actually set up.
CADDY_SYS="/Library/LaunchDaemons/${PREFIX}.caddy.plist"

# Log-rotation policy for memory/*.log (ADR-020). newsyslog runs as root
# hourly at :30 via com.apple.newsyslog and reads /etc/newsyslog.d/*.conf.
NEWSYSLOG_SYS="/etc/newsyslog.d/nucleus.conf"

caddy_configured() {
  [ -n "${CF_API_TOKEN:-}" ] && [ -x "$HOME/.local/bin/caddy" ] \
    && [ -f "$WORKSPACE_ROOT/tools/caddy/caddy.plist.example" ]
}

install_caddy() {
  if ! caddy_configured; then
    echo "skipping caddy — perimeter not configured (need CF_API_TOKEN + ~/.local/bin/caddy)"
    return 0
  fi
  echo "installing caddy perimeter (root LaunchDaemon on :443 — sudo required)"
  # Re-render Caddyfile + plist from current .env + live tailnet IP.
  if ! "$WORKSPACE_ROOT/tools/caddy/install.sh" >/dev/null; then
    echo "  caddy config generation failed (is tailscale up?) — perimeter not reloaded"
    return 1
  fi
  local plist="$WORKSPACE_ROOT/tools/caddy/${PREFIX}.caddy.plist"
  sudo cp "$plist" "$CADDY_SYS" \
    && sudo chown root:wheel "$CADDY_SYS" \
    && { sudo launchctl bootout system "$CADDY_SYS" 2>/dev/null || true; } \
    && sudo launchctl bootstrap system "$CADDY_SYS" \
    && echo "installed ${PREFIX}.caddy" \
    || { echo "  caddy load failed"; return 1; }
}

# Render + install the newsyslog rotation config. Content-diff guard so the
# sudo prompt only appears when something actually changed.
install_newsyslog() {
  local tmpl="$WORKSPACE_ROOT/tools/newsyslog/nucleus.conf.example" rendered
  [ -f "$tmpl" ] || return 0
  rendered="$(sed -e "s|__WORKSPACE_ROOT__|$WORKSPACE_ROOT|g" \
                  -e "s|__USER__|$(id -un)|g" "$tmpl")"
  if [ -f "$NEWSYSLOG_SYS" ] && [ "$rendered" = "$(cat "$NEWSYSLOG_SYS")" ]; then
    echo "newsyslog rotation config up to date"
    return 0
  fi
  echo "installing log rotation → $NEWSYSLOG_SYS (sudo required)"
  printf '%s\n' "$rendered" | sudo tee "$NEWSYSLOG_SYS" >/dev/null \
    && sudo chown root:wheel "$NEWSYSLOG_SYS" \
    && sudo chmod 644 "$NEWSYSLOG_SYS" \
    && echo "installed newsyslog policy"
}

# Remove installed ${PREFIX}.* agents that no longer have a template —
# hard-cut policy (ADR-020): a service deleted from tools/launchd/ is
# unloaded and its plist removed on the next full install run. Caddy is
# exempt (root LaunchDaemon in /Library/LaunchDaemons, never under $DEST,
# but guard anyway). Bonsai is NOT an orphan even when its install is
# skipped: its template exists.
prune_orphans() {
  local removed=0 plist base service
  for plist in "$DEST/${PREFIX}".*.plist; do
    [ -f "$plist" ] || continue
    base="$(basename "$plist" .plist)"          # e.g. dev.nucleus.distiller-hourly
    service="${base#"${PREFIX}".}"              # e.g. distiller-hourly
    [ "$service" = "caddy" ] && continue
    if [ ! -f "$SCRIPT_DIR/$service.plist.example" ]; then
      echo "pruning orphan $base (no template — removed service)"
      launchctl bootout "$DOMAIN" "$plist" 2>/dev/null || true
      rm -f "$plist"
      removed=$((removed + 1))
    fi
  done
  [ "$removed" -gt 0 ] && echo "pruned $removed orphan plist(s)"
  return 0
}

uninstall() {
  for plist in "$DEST"/${PREFIX}.*.plist; do
    [ -f "$plist" ] || continue
    echo "booting out $(basename "$plist")"
    launchctl bootout "$DOMAIN" "$plist" 2>/dev/null || true
    rm "$plist"
  done
  if [ -f "$CADDY_SYS" ]; then
    echo "booting out ${PREFIX}.caddy (sudo)"
    sudo launchctl bootout system "$CADDY_SYS" 2>/dev/null || true
    sudo rm -f "$CADDY_SYS"
  fi
  if [ -f "$NEWSYSLOG_SYS" ]; then
    echo "removing $NEWSYSLOG_SYS (sudo)"
    sudo rm -f "$NEWSYSLOG_SYS"
  fi
}

if [ "${1:-}" = "--uninstall" ]; then
  uninstall
  exit 0
fi

FILTER="${1:-}"
[ -z "$FILTER" ] && prune_orphans

for template in "$SCRIPT_DIR"/*.plist.example; do
  service="$(basename "$template" .plist.example)"
  if [ -n "$FILTER" ] && [[ "$service" != *"$FILTER"* ]]; then
    continue
  fi
  # The bonsai image-gen backend (ADR-019) is opt-in: skip it unless the
  # operator has pointed NUCLEUS_BONSAI_DIR at a Bonsai-Image-Demo checkout.
  if [ "$service" = "bonsai" ] && [ -z "${NUCLEUS_BONSAI_DIR:-}" ]; then
    echo "skipping bonsai — NUCLEUS_BONSAI_DIR not set in .env"
    continue
  fi
  dest="$DEST/${PREFIX}.${service}.plist"
  sed \
    -e "s|__USER_HOME__|$HOME|g" \
    -e "s|__LAUNCHD_PREFIX__|$PREFIX|g" \
    -e "s|__TZ__|$NUCLEUS_TZ|g" \
    -e "s|__NUCLEUS_BONSAI_DIR__|${NUCLEUS_BONSAI_DIR:-}|g" \
    "$template" > "$dest"
  # bootout first — bootstrap fails (EEXIST) if the service is already
  # loaded; bootout of a not-loaded service exits nonzero (tolerated). The
  # one-retry covers the rare race where a KeepAlive daemon hasn't fully
  # reaped before the re-bootstrap.
  launchctl bootout "$DOMAIN" "$dest" 2>/dev/null || true
  launchctl bootstrap "$DOMAIN" "$dest" \
    || { sleep 1; launchctl bootstrap "$DOMAIN" "$dest"; }
  echo "installed ${PREFIX}.${service}"
done

# Log rotation (ADR-020) — when no filter, or when the filter matches.
if [ -z "$FILTER" ] || [[ "newsyslog" == *"$FILTER"* ]]; then
  install_newsyslog || echo "warning: newsyslog policy not installed — see above"
fi

# Caddy perimeter (ADR-011) — when no filter, or when the filter matches "caddy".
if [ -z "$FILTER" ] || [[ "caddy" == *"$FILTER"* ]]; then
  install_caddy || echo "warning: caddy perimeter not (re)installed — see above"
fi
