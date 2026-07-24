#!/usr/bin/env bash
# Block leaks of personal information into committed source.
#
# Scans stdin (a `git diff --cached` or proposed Write/Edit content) against
# four independent layers and exits non-zero on ANY hit. Called from three
# places (all pipe through here, so strengthening this arms all of them):
#   1. .git/hooks/pre-commit              — staged diff piped in
#   2. Claude PreToolUse on Bash          — staged diff on `git commit`
#   3. Claude PreToolUse on Write/Edit    — proposed file content
#
# Layers:
#   A. Live `.env` VALUES (tokens/IDs/allowlisted names >=6 chars) — substring.
#   B. Curated denylist `.claude/secret-strings` (gitignored) — the sensitive
#      literals the `.env`-value layer can't see (third-party names, etc.).
#      Whole-word, case-insensitive, any length.
#   C. Generic PII heuristics — personal emails, phone/E.164, chat JIDs +
#      snowflake IDs, and the operator's home path. Catches personal data
#      even when it isn't registered anywhere.
#   D. Operator-personal-skill leakage — names of skills that live in
#      ~/.claude/skills (operator-personal) but NOT in .claude/skills
#      (repo-wired). That content belongs in the personal tree, never here.
#
# Nothing sensitive is hardcoded in this file — every literal is derived at
# runtime from .env / the gitignored denylist / the personal-skill tree, so
# the guard itself is safe to commit. Bypass intentionally: git commit --no-verify.

set -euo pipefail

WORKSPACE_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
ENV_FILE="$WORKSPACE_ROOT/.env"
DENYLIST="$WORKSPACE_ROOT/.claude/secret-strings"

# ── build the exact-literal blocklists ──────────────────────────────────────
# SUBSTR: matched as substrings (grep -F). WORD: matched whole-word (grep -iwF).
SUBSTR=()
WORD=()

add_substr() {
  local v="$1"
  [ -z "$v" ] && return
  [ ${#v} -lt 6 ] && return
  case "$v" in
    info|debug|warn|error|trace|true|false|/path/to/nucleus|claude) return ;;
  esac
  SUBSTR+=("$v")
}

# A. .env values (skip if absent — fresh clones have no .env).
if [ -f "$ENV_FILE" ]; then
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in ''|'#'*) continue ;; esac
    value="${line#*=}"
    value="${value#\"}"; value="${value%\"}"
    value="${value#\'}"; value="${value%\'}"
    [ -z "$value" ] && continue
    IFS=',' read -ra parts <<< "$value"
    for p in "${parts[@]}"; do
      p="${p#"${p%%[![:space:]]*}"}"; p="${p%"${p##*[![:space:]]}"}"
      add_substr "$p"
    done
  done < "$ENV_FILE"
  # operator home dir (leaks the local username via absolute paths)
  [ -n "${HOME:-}" ] && [ ${#HOME} -ge 6 ] && SUBSTR+=("$HOME")
fi

# B. curated denylist (gitignored) — whole-word so distinctive names can be
# listed without false-positiving as substrings.
if [ -f "$DENYLIST" ]; then
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in ''|'#'*) continue ;; esac
    line="${line#"${line%%[![:space:]]*}"}"; line="${line%"${line##*[![:space:]]}"}"
    [ -n "$line" ] && WORD+=("$line")
  done < "$DENYLIST"
fi

# D. operator-personal-skill names = ls ~/.claude/skills MINUS repo-wired
# .claude/skills. Whole-word. A tiny allowlist covers generic/functional
# markers that legitimately appear as constants in infra code.
SKILL_ALLOW="test-skill"
if [ -d "$HOME/.claude/skills" ]; then
  repo_skills="$(git -C "$WORKSPACE_ROOT" ls-files .claude/skills 2>/dev/null | cut -d/ -f2 | sort -u)"
  for d in "$HOME"/.claude/skills/*/; do
    name="$(basename "$d")"
    case "$name" in .*) continue ;; esac
    printf '%s\n' "$repo_skills" | grep -qxF "$name" && continue   # repo-wired, fine
    printf '%s' "$SKILL_ALLOW" | grep -qwF "$name" && continue     # generic marker
    WORD+=("$name")
  done
fi

# ── read the haystack; reduce a staged diff to ADDED lines ─────────────────
HAY="$(cat)"
[ -z "$HAY" ] && exit 0
case "$HAY" in
  "diff --git"*)
    HAY="$(printf '%s' "$HAY" | grep '^+' | grep -v '^+++' || true)"
    [ -z "$HAY" ] && exit 0
    ;;
esac

FOUND=()

# A/B/D exact-literal matches
for pat in ${SUBSTR[@]+"${SUBSTR[@]}"}; do
  printf '%s' "$HAY" | grep -F -- "$pat" >/dev/null 2>&1 && FOUND+=("value:$pat")
done
for pat in ${WORD[@]+"${WORD[@]}"}; do
  printf '%s' "$HAY" | grep -iwF -- "$pat" >/dev/null 2>&1 && FOUND+=("denylist/skill:$pat")
done

# C. PII heuristics (regex). Skip obvious placeholders.
pii() {
  # $1 = extended-regex, $2 = label. MUST return 0 (called under set -e as a
  # plain command — a non-zero return would abort the whole script).
  local hit
  hit="$(printf '%s' "$HAY" | grep -InE "$1" 2>/dev/null | grep -viE 'example\.(com|org|net)|5511999999999|you@|@example|/path/to/' | head -1 || true)"
  if [ -n "$hit" ]; then FOUND+=("pii-$2"); fi
  return 0
}
pii '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}' 'email'
pii '\+?[0-9]{6,}@(s\.whatsapp\.net|g\.us|c\.us|lid)' 'whatsapp-jid'
pii '(\+55|\+1)[0-9]{9,}' 'phone-e164'
pii '/(Users|home)/[a-z][a-z0-9_-]{2,}/' 'home-path'

if [ ${#FOUND[@]} -gt 0 ]; then
  {
    echo "✖ possible personal/sensitive information in staged/proposed content:"
    printf '    - %s\n' "${FOUND[@]}"
    echo ""
    echo "  Personal info, third-party identifiers, and operator-personal-skill"
    echo "  content do not belong in this public repo. See .claude/rules/secrets.md."
    echo "  Route real values through .env / .claude/secret-strings / ~/.claude/skills."
    echo "  Bypass intentionally: git commit --no-verify"
  } >&2
  exit 2
fi

exit 0
