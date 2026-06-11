#!/usr/bin/env bash
# Nucleus runtime health check.
#
# Verifies every long-running Nucleus component is up and the grants it
# depends on are intact. Built to be run after a reboot / OS update (which
# tear down tmux + processes and frequently reset macOS TCC grants).
#
# Secret-free by design: service labels derive from agents.toml (the
# ADR-016 single source of truth), paths from $HOME / nucleus.toml / .env
# at runtime. Operator-specific extra services come from the gitignored
# .env (HEALTHCHECK_EXTRA_*) — never hardcoded here (Rule 1 / SECRETS.md).
#
# Usage:  ./tools/healthcheck.sh
# Exit 0 if no FAILs, 1 otherwise. WARNs don't fail the run.

set -uo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"

# .env for NUCLEUS_LAUNCHD_PREFIX / NUCLEUS_BONSAI_DIR / HEALTHCHECK_EXTRA_*.
if [ -f .env ]; then set -a; . ./.env; set +a; fi
PREFIX="${NUCLEUS_LAUNCHD_PREFIX:-dev.nucleus}"

pass=0 warn=0 fail=0
PASS(){ printf '  \033[32m✓ PASS\033[0m  %s\n' "$1"; pass=$((pass+1)); }
WARN(){ printf '  \033[33m! WARN\033[0m  %s\n' "$1"; warn=$((warn+1)); }
FAIL(){ printf '  \033[31m✗ FAIL\033[0m  %s\n' "$1"; fail=$((fail+1)); }
HEAD(){ printf '\n\033[1m== %s ==\033[0m\n' "$1"; }

# --- launchd services ---------------------------------------------------
# KeepAlive bots must have a live PID; periodic jobs (StartInterval /
# StartCalendarInterval) normally have no PID between fires — absence there
# is fine, we only flag a nonzero last-exit status.
#
# Lists derive from agents.toml (ADR-020): launch = "launchd-daemon" →
# persistent, "launchd-cron" → periodic; in-process / on-demand agents are
# skipped. Registry labels are canonical dev.nucleus.*; rewritten to the
# operator's prefix when customized.
labels_for() {  # $1 = launchd-daemon | launchd-cron
  awk -v want="$1" '
    function flush() { if (label != "" && kind == want) print label; label = ""; kind = "" }
    /^\[\[agent\]\]/              { flush() }
    /^launchd_label[[:space:]]*=/ { split($0, a, "\""); label = a[2] }
    /^launch[[:space:]]*=/        { split($0, a, "\""); kind = a[2] }
    END                           { flush() }
  ' agents.toml | sed "s/^dev\.nucleus\./$PREFIX./"
}
PERSISTENT="$(labels_for launchd-daemon)"
PERIODIC="$(labels_for launchd-cron)"

# Bonsai (ADR-019) is an external image-gen backend, not an agents.toml
# entry — checked iff the operator opted in (mirrors install.sh's gate).
[ -n "${NUCLEUS_BONSAI_DIR:-}" ] && PERSISTENT="$PERSISTENT ${PREFIX}.bonsai"

# Operator-specific extra services (tunnel daemons, sidecars) live in .env,
# never in this committed file. Space-separated launchd labels.
PERSISTENT="$PERSISTENT ${HEALTHCHECK_EXTRA_PERSISTENT:-}"
PERIODIC="$PERIODIC ${HEALTHCHECK_EXTRA_PERIODIC:-}"

svc_line(){ launchctl list 2>/dev/null | awk -v l="$1" '$3==l{print $1" "$2}'; }

HEAD "launchd — persistent bots (need a live PID + KeepAlive)"
for l in $PERSISTENT; do
  line="$(svc_line "$l")"; pid="${line%% *}"; st="${line##* }"
  if [ -z "$line" ]; then FAIL "$l — not loaded (run tools/launchd/install.sh?)"
  elif [ "$pid" = "-" ]; then FAIL "$l — loaded but NOT running (last exit $st)"
  else PASS "$l — running (pid $pid)"; fi
done

HEAD "launchd — periodic jobs (loaded; PID may be absent between fires)"
for l in $PERIODIC; do
  line="$(svc_line "$l")"; pid="${line%% *}"; st="${line##* }"
  if [ -z "$line" ]; then WARN "$l — not loaded"
  elif [ "$pid" != "-" ]; then PASS "$l — currently firing (pid $pid)"
  elif [ "$st" = "0" ]; then PASS "$l — loaded, last exit clean"
  else WARN "$l — loaded, last exit $st (check ~/Library/Logs or memory/logs/$l*)"; fi
done

# --- voice dictation (ADR/memory: Hammerspoon + nucleus-dictate) --------
HEAD "voice dictation (⌥-Space push-to-talk)"
if pgrep -qf "Hammerspoon.app"; then PASS "Hammerspoon running"; else FAIL "Hammerspoon NOT running (login item) — dictation dead"; fi
if [ -x "$HOME/.local/bin/nucleus-dictate" ]; then PASS "nucleus-dictate present + executable"; else FAIL "nucleus-dictate missing at ~/.local/bin"; fi
[ -f "$HOME/.hammerspoon/init.lua" ] && PASS "hammerspoon init.lua present" || WARN "~/.hammerspoon/init.lua missing"

# --- cloudflared tunnels ------------------------------------------------
HEAD "cloudflared tunnels"
if pgrep -qf "cloudflared tunnel run"; then PASS "cloudflared tunnel process up"; else FAIL "no cloudflared tunnel running (perimeter URLs down)"; fi

# --- timezone (memory: launchd TZ pitfall) ------------------------------
HEAD "timezone (launchd TZ pitfall)"
WANT="$(grep -E '^NUCLEUS_TZ=' .env 2>/dev/null | cut -d= -f2)"
SYS="$(readlink /etc/localtime | sed 's#.*zoneinfo/##')"
if [ -n "$WANT" ] && [ "$WANT" = "$SYS" ]; then PASS "system TZ $SYS matches NUCLEUS_TZ"
elif [ -n "$WANT" ]; then FAIL "TZ mismatch: system=$SYS NUCLEUS_TZ=$WANT (plists may fire N hours off — LOG OUT/IN, reload won't fix)"
else WARN "NUCLEUS_TZ not set in .env (system TZ=$SYS)"; fi

# --- Full Disk Access (memory: FDA revoked on binary upgrade) -----------
HEAD "Full Disk Access — Obsidian vault read"
VP="$(awk -F'"' '/^[[:space:]]*vault_path[[:space:]]*=/{print $2; exit}' nucleus.toml 2>/dev/null)"
VP="${VP/#\~/$HOME}"
if [ -z "$VP" ]; then WARN "vault_path not found in nucleus.toml"
elif ! ls "$VP" >/dev/null 2>&1; then FAIL "vault not listable at \$vault_path"
elif find "$VP" -maxdepth 2 -name '*.md' -print -quit 2>/dev/null | head -1 | xargs -I{} head -n1 {} >/dev/null 2>&1; then PASS "vault file read OK (FDA intact)"
else FAIL "vault read EPERM — FDA revoked (System Settings ▸ Privacy ▸ Full Disk Access ▸ re-grant terminal/claude)"; fi

# --- reminders ----------------------------------------------------------
HEAD "reminders (ticker primitive)"
if [ -x ./target/release/reminders ]; then
  if ./target/release/reminders list >/dev/null 2>&1; then PASS "reminders binary + db OK ($(./target/release/reminders list 2>/dev/null | grep -c '^#') active)"
  else FAIL "reminders list errored (db locked / corrupt?)"; fi
else WARN "reminders binary not built (cargo build --release -p reminders)"; fi

# --- whatsapp reconnect (no fresh QR) -----------------------------------
HEAD "whatsapp link state"
[ -f messaging/whatsapp/auth/creds.json ] && PASS "baileys auth on disk (no QR re-pair needed)" || FAIL "messaging/whatsapp/auth/creds.json missing — will demand a new QR"
if [ -f memory/whatsapp.log ]; then
  tail -n 200 memory/whatsapp.log 2>/dev/null | grep -qiE "connection (open|opened)|connected" && PASS "whatsapp log shows a recent open connection" || WARN "no recent 'connection open' in whatsapp.log — confirm it linked (tmux attach -t nucleus-whatsapp-dm)"
fi

# --- can't auto-verify: TCC grants OS updates love to reset -------------
HEAD "MANUAL VERIFY (TCC grants — not machine-readable)"
echo "  • Hammerspoon: Accessibility + Microphone (System Settings ▸ Privacy)."
echo "    → Test: press ⌥-Space, speak, confirm text pastes into a field."
echo "  • Full Disk Access for the terminal/claude binary (covered above if PASS)."
echo "  • If whatsapp/discord show no PID after login, give launchd ~30s, then:"
echo "    launchctl kickstart -k gui/\$(id -u)/dev.nucleus.whatsapp"

# --- summary ------------------------------------------------------------
printf '\n\033[1m== summary ==\033[0m  \033[32m%d pass\033[0m / \033[33m%d warn\033[0m / \033[31m%d fail\033[0m\n' "$pass" "$warn" "$fail"
[ "$fail" -eq 0 ]
