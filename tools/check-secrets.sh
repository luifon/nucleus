#!/usr/bin/env bash
# Block leaks of personal identifiers into committed source.
#
# Auto-derives a blocklist from the current `.env` values, then greps the
# input (stdin) for any of those values. Exits 0 if clean, 1 if any match.
# Called from three places:
#   1. .git/hooks/pre-commit       — staged diff piped in
#   2. Claude PreToolUse on Bash    — staged diff piped in when git commit fires
#   3. Claude PreToolUse on Write/Edit — proposed file content piped in
#
# Skipped values: blank, shorter than 6 chars (too noisy as substrings),
# obvious non-secrets (log levels, booleans, the .env.example
# placeholder `/path/to/nucleus`).

set -euo pipefail

WORKSPACE_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
ENV_FILE="$WORKSPACE_ROOT/.env"

# Fresh clones have no .env to derive from — nothing to enforce.
[ -f "$ENV_FILE" ] || exit 0

PATTERNS=()
add_pattern() {
  local v="$1"
  [ -z "$v" ] && return
  [ ${#v} -lt 6 ] && return
  case "$v" in
    info|debug|warn|error|trace|true|false|/path/to/nucleus|claude) return ;;
  esac
  PATTERNS+=("$v")
}
while IFS= read -r line || [ -n "$line" ]; do
  case "$line" in ''|'#'*) continue ;; esac
  value="${line#*=}"
  value="${value#\"}"; value="${value%\"}"
  value="${value#\'}"; value="${value%\'}"
  [ -z "$value" ] && continue
  # Many env vars are CSV (allowlists, IDs). Split on commas so each
  # individual identifier becomes its own blocklist entry, not just the
  # whole joined string.
  IFS=',' read -ra parts <<< "$value"
  for p in "${parts[@]}"; do
    # Trim leading/trailing whitespace
    p="${p#"${p%%[![:space:]]*}"}"
    p="${p%"${p##*[![:space:]]}"}"
    add_pattern "$p"
  done
done < "$ENV_FILE"

# Also block the user's absolute home dir (leaks the macOS username).
[ -n "${HOME:-}" ] && [ ${#HOME} -ge 6 ] && PATTERNS+=("$HOME")

[ ${#PATTERNS[@]} -eq 0 ] && exit 0

HAY="$(cat)"
[ -z "$HAY" ] && exit 0

FOUND=()
for pat in "${PATTERNS[@]}"; do
  if printf '%s' "$HAY" | grep -F -- "$pat" > /dev/null 2>&1; then
    FOUND+=("$pat")
  fi
done

if [ ${#FOUND[@]} -gt 0 ]; then
  {
    echo "✖ secret-shaped values from .env detected in staged/proposed content:"
    for f in "${FOUND[@]}"; do
      echo "    - $f"
    done
    echo ""
    echo "  See .claude/rules/secrets.md for the policy."
    echo "  Bypass intentionally: git commit --no-verify"
  } >&2
  # Exit 2 = PreToolUse blocking convention (stderr surfaced to Claude + user).
  # Git pre-commit hook treats ANY non-zero as a block, so exit 2 works for both.
  exit 2
fi

exit 0
