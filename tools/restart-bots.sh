#!/usr/bin/env bash
# Nucleus daily bot recycle.
#
# Resident bots (launch = "launchd-daemon") accumulate in-process state:
# wedged message handlers, stale session-pool entries, and the spawn-day
# "today" anchor that long-lived claude sessions drift on. Nothing recycled
# them — a WhatsApp worker ran 7d15h and silently stopped handling inbound
# DMs for ~18h while still looking connected (queue empty, PID alive,
# reconnects logging). This job is the periodic clean slate that class of
# bug needs.
#
# Uses bootout + bootstrap, never `kickstart -k`: only bootout/bootstrap
# rereads the plist, so an env or plist edit actually takes effect.
#
# Secret-free by design: labels derive from agents.toml (ADR-016 single
# source of truth), paths from $HOME / .env at runtime (Rule 1).
#
# The bots compact their chat sessions at 04:00 too (ADR-025), the same
# minute this job runs, and a compaction takes minutes. Each bot holds
# memory/compaction/<agent>.lock (pid=…, started=<unix secs>) while it
# compacts, taken one minute before 04:00. This job first waits until no
# live lock is left: a lock whose pid is dead, or that is older than
# COMPACTION_MAX_AGE_SECS, is stale and ignored. It waits at most that long.
#
# Usage:  ./tools/restart-bots.sh [--dry-run] [--force]
#   --dry-run  print what would happen, change nothing
#   --force    restart even when a delivery queue is mid-drain or a bot is
#              compacting its sessions
# Exit 0 if every targeted service came back, 1 otherwise.

set -uo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"

if [ -f .env ]; then set -a; . ./.env; set +a; fi
PREFIX="${NUCLEUS_LAUNCHD_PREFIX:-dev.nucleus}"
DOMAIN="gui/$(id -u)"
AGENTS_DIR="$HOME/Library/LaunchAgents"

DRY_RUN=0; FORCE=0
for a in "$@"; do
  case "$a" in
    --dry-run) DRY_RUN=1 ;;
    --force)   FORCE=1 ;;
    *) echo "unknown arg: $a" >&2; exit 2 ;;
  esac
done

log() { printf '%s  %s\n' "$(date '+%Y-%m-%dT%H:%M:%S%z')" "$1"; }

# Resident bots only. Periodic jobs (launchd-cron) spawn fresh per fire and
# exit, so they're immune to state accumulation and must NOT be bounced —
# bootstrapping one mid-fire would kill work in flight.
labels_for() {
  awk -v want="$1" '
    function flush() { if (label != "" && kind == want) print label; label = ""; kind = "" }
    /^\[\[agent\]\]/              { flush() }
    /^launchd_label[[:space:]]*=/ { split($0, a, "\""); label = a[2] }
    /^launch[[:space:]]*=/        { split($0, a, "\""); kind = a[2] }
    END                           { flush() }
  ' agents.toml | sed "s/^dev\.nucleus\./$PREFIX./"
}
TARGETS="$(labels_for launchd-daemon)"
TARGETS="$TARGETS ${RESTART_EXTRA_PERSISTENT:-}"

# --- drain guard ---------------------------------------------------------
# Restarting mid-drain can delay or duplicate a delivery. Only the WhatsApp
# bot owns an outbound queue; if it has pending work, skip just that service
# (the others still recycle) unless --force.
whatsapp_busy() {
  local db="memory/whatsapp.db"
  [ -f "$db" ] || return 1
  local n
  n="$(sqlite3 "$db" "select count(*) from outbound_queue where status='pending';" 2>/dev/null || echo 0)"
  [ "${n:-0}" -gt 0 ]
}

# --- compaction guard ------------------------------------------------------
# Same bound as COMPACTION_LOCK_MAX_AGE in core/src/compaction_lock.rs and
# messaging/whatsapp/src/compaction_lock.ts.
COMPACTION_MAX_AGE_SECS=2700
COMPACTION_LOCK_DIR="memory/compaction"

# Agents whose compaction lock is live, space-separated (empty = none).
live_compactions() {
  local now f pid started out=""
  now="$(date +%s)"
  for f in "$COMPACTION_LOCK_DIR"/*.lock; do
    [ -f "$f" ] || continue
    pid="$(sed -n 's/^pid=\([0-9][0-9]*\)$/\1/p' "$f")"
    started="$(sed -n 's/^started=\([0-9][0-9]*\)$/\1/p' "$f")"
    [ -n "$pid" ] && [ -n "$started" ] || continue
    kill -0 "$pid" 2>/dev/null || continue
    [ $((now - started)) -le "$COMPACTION_MAX_AGE_SECS" ] || continue
    out="$out $(basename "$f" .lock)"
  done
  printf '%s' "${out# }"
}

wait_for_compactions() {
  local deadline active
  deadline=$(( $(date +%s) + COMPACTION_MAX_AGE_SECS ))
  active="$(live_compactions)"
  [ -n "$active" ] || return 0
  log "WAIT  session compaction running ($active) — waiting up to ${COMPACTION_MAX_AGE_SECS}s"
  while [ -n "$active" ]; do
    if [ "$(date +%s)" -ge "$deadline" ]; then
      log "WAIT  compaction still running ($active) after ${COMPACTION_MAX_AGE_SECS}s — restarting anyway"
      return 0
    fi
    sleep 10
    active="$(live_compactions)"
  done
  log "WAIT  compaction finished"
}

if [ "$FORCE" -eq 0 ]; then
  if [ "$DRY_RUN" -eq 1 ]; then
    active="$(live_compactions)"
    [ -z "$active" ] || log "DRY   would wait for the session compaction ($active)"
  else
    wait_for_compactions
  fi
fi

rc=0 done_n=0 skipped_n=0
for label in $TARGETS; do
  [ -n "$label" ] || continue
  plist="$AGENTS_DIR/$label.plist"

  if [ ! -f "$plist" ]; then
    log "SKIP  $label — no plist at $plist"
    skipped_n=$((skipped_n+1)); continue
  fi

  if [ "$FORCE" -eq 0 ] && [ "$label" = "${PREFIX}.whatsapp" ] && whatsapp_busy; then
    log "SKIP  $label — outbound queue mid-drain (use --force to override)"
    skipped_n=$((skipped_n+1)); continue
  fi

  if [ "$DRY_RUN" -eq 1 ]; then
    log "DRY   $label — would bootout + bootstrap"
    continue
  fi

  launchctl bootout "$DOMAIN/$label" >/dev/null 2>&1
  sleep 2
  if launchctl bootstrap "$DOMAIN" "$plist" >/dev/null 2>&1; then
    sleep 3
    # KeepAlive services must come back with a live PID; a '-' here means the
    # service registered but the process died immediately (crash-loop).
    pid="$(launchctl list 2>/dev/null | awk -v l="$label" '$3==l{print $1}')"
    if [ -n "$pid" ] && [ "$pid" != "-" ]; then
      log "OK    $label — restarted (pid $pid)"
      done_n=$((done_n+1))
    else
      log "FAIL  $label — bootstrapped but no live pid"
      rc=1
    fi
  else
    log "FAIL  $label — bootstrap failed"
    rc=1
  fi
done

log "done: $done_n restarted, $skipped_n skipped, exit $rc"
exit $rc
