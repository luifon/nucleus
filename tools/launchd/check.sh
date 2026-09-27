#!/usr/bin/env bash
# Check that the installed Nucleus launchd jobs can start.
#
# Jobs: every ${PREFIX}.*.plist in ~/Library/LaunchAgents except caddy.
# PREFIX is NUCLEUS_LAUNCHD_PREFIX from .env (default dev.nucleus), read the
# way install.sh reads it.
#
# Checks, in this order:
#   --repair       for each job whose `launchctl print` shows LWCR flags and
#                  no running process, run `install.sh <service>` (bootout +
#                  bootstrap). A flagged job with a running process (a
#                  KeepAlive bot, or a periodic job in the middle of a run) is
#                  not booted out: that stops the run, and a stopped reminders
#                  tick leaves its lock behind. The fix command is printed.
#   (always)       each job is loaded, and `launchctl print` shows no LWCR
#                  flags. launchd refuses to start a job flagged "needs LWCR
#                  update" with "Unable to get updated LWCR ... error 0x3";
#                  booting it out and bootstrapping it again removes the flags.
#   --since EPOCH  no nucleus*.ips crash report newer than EPOCH names
#                  CODESIGNING or "Code Signature Invalid" (macOS stopped the
#                  binary at launch because its signature did not match).
#   --start        for each job whose installed plist has StartInterval <= 60,
#                  wait for a running run to end, start the job with
#                  `launchctl kickstart` (no -k) and check that it ran and
#                  exited 0, or is still running at the timeout. A reminders
#                  tick sends only reminders that are already due and an intake
#                  tick only polls, so an early run has no other effect.
#
# Without flags the script only reads state.
#
# Environment:
#   NUCLEUS_JOB_START_TIMEOUT   seconds to wait for a run (default 20)
#   NUCLEUS_JOB_POLL_INTERVAL   seconds between `launchctl print` polls (default 2)
#
# Output: one PASS / WARN / FAIL line per check, then a summary line.
# Exit 0 if there is no FAIL, 1 otherwise, 2 on a usage error.
#
# Usage:
#   ./tools/launchd/check.sh
#   ./tools/launchd/check.sh --since "$(date -v-1H +%s)" --repair --start

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
AGENTS_DIR="$HOME/Library/LaunchAgents"
REPORTS_DIR="$HOME/Library/Logs/DiagnosticReports"
INSTALL_CMD="$SCRIPT_DIR/install.sh"

usage() { sed -n '/^# Usage:/,/^$/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2; exit 2; }

as_int() { case "$1" in ''|*[!0-9]*) echo 0 ;; *) echo "$1" ;; esac; }

SINCE="" REPAIR=0 START=0
while [ $# -gt 0 ]; do
  case "$1" in
    --since)
      [ $# -ge 2 ] || usage
      SINCE="$2"; shift ;;
    --since=*) SINCE="${1#--since=}" ;;
    --repair) REPAIR=1 ;;
    --start) START=1 ;;
    -h|--help) usage ;;
    *) echo "unknown argument: $1" >&2; usage ;;
  esac
  shift
done
if [ -n "$SINCE" ] && [ "$SINCE" != "$(as_int "$SINCE")" ]; then
  echo "--since takes epoch seconds, got '$SINCE'" >&2; exit 2
fi

TIMEOUT="${NUCLEUS_JOB_START_TIMEOUT:-20}"
POLL="${NUCLEUS_JOB_POLL_INTERVAL:-2}"
for v in "$TIMEOUT" "$POLL"; do
  if ! awk -v v="$v" 'BEGIN { exit !(v ~ /^[0-9]+(\.[0-9]+)?$/ && v + 0 > 0) }'; then
    echo "NUCLEUS_JOB_START_TIMEOUT and NUCLEUS_JOB_POLL_INTERVAL take positive seconds, got '$v'" >&2
    exit 2
  fi
done
# Number of polls that covers the timeout.
POLLS="$(awk -v t="$TIMEOUT" -v p="$POLL" 'BEGIN { n = int(t / p); if (n * p < t) n++; print n }')"

if [ -f "$WORKSPACE_ROOT/.env" ]; then
  set -a
  # shellcheck disable=SC1091
  source "$WORKSPACE_ROOT/.env"
  set +a
fi
PREFIX="${NUCLEUS_LAUNCHD_PREFIX:-dev.nucleus}"
DOMAIN="gui/$(id -u)"

pass=0 warn=0 fail=0
PASS() { printf '  PASS  %s\n' "$1"; pass=$((pass + 1)); }
WARN() { printf '  WARN  %s\n' "$1"; warn=$((warn + 1)); }
FAIL() { printf '  FAIL  %s\n' "$1"; fail=$((fail + 1)); }
detail() { printf '%s\n' "$1" | sed 's/^[[:space:]]*/          /'; }

LABELS=()
for plist in "$AGENTS_DIR/${PREFIX}".*.plist; do
  [ -f "$plist" ] || continue
  label="$(basename "$plist" .plist)"
  [ "$label" = "${PREFIX}.caddy" ] && continue
  LABELS+=("$label")
done
if [ ${#LABELS[@]} -eq 0 ]; then
  echo "no ${PREFIX}.*.plist installed in $AGENTS_DIR — nothing to check"
  exit 0
fi

service_of() { printf '%s\n' "${1#"${PREFIX}".}"; }
fix_cmd() { printf './tools/launchd/install.sh %s\n' "$(service_of "$1")"; }

# `launchctl print` of a label; non-zero when the job is not loaded.
job_state() { launchctl print "$DOMAIN/$1" 2>/dev/null; }

# Value of the first `key = value` line in a `launchctl print` output.
field() {
  printf '%s\n' "$1" | awk -v k="$2" '
    { line = $0; sub(/^[ \t]+/, "", line)
      if (index(line, k " = ") == 1) { print substr(line, length(k) + 4); exit } }'
}

# A job counts as running when its state shows a pid or `state = running`.
# Either one is enough: a wrong "running" only skips a repair or a start,
# while a wrong "not running" would boot out or start a job during a run.
is_running() {
  printf '%s\n' "$1" | grep -qE '^[[:space:]]*(pid = [0-9]+|state = running)[[:space:]]*$'
}

lwcr_lines() { printf '%s\n' "$1" | grep 'LWCR'; }

status_lines() {
  printf '%s\n' "$1" | grep -E '^[[:space:]]*(state|runs|pid|last exit code|last terminating signal) = '
}


# Services whose template name contains $1: the ones `install.sh $1` installs.
templates_matching() {
  local t s
  for t in "$SCRIPT_DIR"/*.plist.example; do
    [ -f "$t" ] || continue
    s="$(basename "$t" .plist.example)"
    case "$s" in
      *"$1"*) echo "$s" ;;
    esac
  done
}

# Poll until the job has no running process. Returns non-zero when it is still
# running after the timeout.
wait_idle() {
  local label="$1" i=0 out
  while [ "$i" -lt "$POLLS" ]; do
    sleep "$POLL"
    out="$(job_state "$label")" || return 0
    is_running "$out" || return 0
    i=$((i + 1))
  done
  return 1
}

# --- repair ---------------------------------------------------------------
if [ "$REPAIR" = 1 ]; then
  echo "== repair (LWCR flags) =="
  for label in "${LABELS[@]}"; do
    out="$(job_state "$label")" || continue
    lwcr_lines "$out" >/dev/null || continue
    service="$(service_of "$label")"
    if is_running "$out"; then
      WARN "$label — LWCR flags, but it has a running process; not booted out. When it is idle: $(fix_cmd "$label")"
      continue
    fi
    # install.sh matches its argument as a substring of the template names;
    # a repair must reinstall this job and no other.
    matches="$(templates_matching "$service")"
    if [ "$matches" != "$service" ]; then
      WARN "$label — LWCR flags; not repaired: 'install.sh $service' would install [$(echo "$matches" | xargs)]. Reinstall it by hand."
      continue
    fi
    if log="$("$INSTALL_CMD" "$service" 2>&1)"; then
      PASS "$label — LWCR flags, reinstalled ($(fix_cmd "$label"))"
    else
      FAIL "$label — LWCR flags, reinstall failed ($(fix_cmd "$label"))"
      detail "$log"
    fi
  done
fi

# --- loaded + LWCR ----------------------------------------------------------
echo "== jobs (loaded, no LWCR flags) =="
OK_LABELS=" "
for label in "${LABELS[@]}"; do
  if ! out="$(job_state "$label")"; then
    FAIL "$label — not loaded (fix: $(fix_cmd "$label"))"
    continue
  fi
  if lines="$(lwcr_lines "$out")"; then
    FAIL "$label — LWCR flags; launchd may refuse to start it (fix: $(fix_cmd "$label"))"
    detail "$lines"
    continue
  fi
  PASS "$label — loaded"
  OK_LABELS="$OK_LABELS$label "
done

# --- crash reports ------------------------------------------------------------
if [ -n "$SINCE" ]; then
  echo "== crash reports (code signature, since $SINCE) =="
  found=0
  for report in "$REPORTS_DIR"/nucleus*.ips; do
    [ -f "$report" ] || continue
    mtime="$(stat -f %m "$report" 2>/dev/null || stat -c %Y "$report")"
    [ "$mtime" -gt "$SINCE" ] || continue
    grep -qE 'CODESIGNING|Code Signature Invalid' "$report" || continue
    found=1
    FAIL "$(basename "$report") — macOS stopped nucleus for an invalid code signature"
    if args="$(grep -E '"(arguments|args|argv)"' "$report")"; then detail "$args"; fi
  done
  [ "$found" = 1 ] || PASS "no nucleus code-signature crash report"
fi

# --- start --------------------------------------------------------------------
if [ "$START" = 1 ]; then
  echo "== start (jobs with StartInterval <= 60) =="
  for label in "${LABELS[@]}"; do
    interval="$(plutil -extract StartInterval raw -o - "$AGENTS_DIR/$label.plist" 2>/dev/null)" || continue
    case "$interval" in ''|*[!0-9]*) continue ;; esac
    [ "$interval" -le 60 ] || continue
    case "$OK_LABELS" in
      *" $label "*) ;;
      *) WARN "$label — not started: it failed the check above"; continue ;;
    esac

    out="$(job_state "$label")"
    if is_running "$out" && ! wait_idle "$label"; then
      WARN "$label — running, not started again (still running after ${TIMEOUT}s)"
      continue
    fi
    out="$(job_state "$label")"
    before="$(as_int "$(field "$out" runs)")"

    if ! kick="$(launchctl kickstart "$DOMAIN/$label" 2>&1)"; then
      FAIL "$label — launchctl kickstart failed"
      detail "$kick"
      continue
    fi

    i=0 runs="$before"
    while [ "$i" -lt "$POLLS" ]; do
      sleep "$POLL"
      out="$(job_state "$label")"
      runs="$(as_int "$(field "$out" runs)")"
      if [ "$runs" -gt "$before" ] && ! is_running "$out"; then break; fi
      i=$((i + 1))
    done

    if [ "$runs" -le "$before" ]; then
      FAIL "$label — kickstart did not start a run (runs = $runs after ${TIMEOUT}s)"
      detail "$(status_lines "$out")"
    elif is_running "$out"; then
      PASS "$label — started, still running after ${TIMEOUT}s"
    else
      code="$(field "$out" "last exit code")"
      signal="$(field "$out" "last terminating signal")"
      if [ -z "$signal" ] && [ "$code" = 0 ]; then
        PASS "$label — started, exited 0"
      else
        FAIL "$label — started, run failed (last exit code = ${code:-?}${signal:+, signal $signal})"
        detail "$(status_lines "$out")"
      fi
    fi
  done
fi

printf 'job check: %d pass / %d warn / %d fail\n' "$pass" "$warn" "$fail"
[ "$fail" -eq 0 ]
