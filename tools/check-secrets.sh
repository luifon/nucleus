#!/usr/bin/env bash
# Block leaks of personal information into committed source.
#
# Scans stdin (a `git diff --cached` or proposed Write/Edit content) against
# five independent layers and exits non-zero on ANY hit. Called from three
# places (all pipe through here, so strengthening this arms all of them):
#   1. .git/hooks/pre-commit              — staged diff piped in
#   2. Claude PreToolUse on Bash          — staged diff on `git commit`
#   3. Claude PreToolUse on Write/Edit    — proposed file content
#
# Layers:
#   A. Live `.env` VALUES (tokens/IDs/allowlisted names >=6 chars) — substring.
#   B. The machine-wide commit-guard denylist (~/.config/commit-guard/denylist,
#      outside every repo) — the sensitive literals the `.env`-value layer
#      can't see (third-party names, etc.). Matched by its own scanner
#      (~/.config/commit-guard/scan): whole word, case- and accent-insensitive,
#      any length. The global git hooks run the same scanner on every commit
#      on the machine; this layer adds the Write/Edit hook (before content
#      reaches disk) and the work pipeline's publish scan.
#   C. Generic PII heuristics — personal emails, phone/E.164, chat JIDs +
#      snowflake IDs, and the operator's home path. Catches personal data
#      even when it isn't registered anywhere.
#   D. Operator-personal-skill leakage — names of skills that live in
#      ~/.claude/skills or .nucleus/.claude/skills (operator-private, the
#      latter gitignored) but NOT in .claude/skills (repo-wired). That
#      content belongs in the private trees, never here.
#   E. Structural (staged diffs only) — any staged path under .nucleus/.
#      That tree is gitignored; a staged path there means it was force-added.
#
# For a diff, the scanned text is the added lines plus the new path of every
# file in the diff, so A–D also catch an identifier that appears only in a
# file or directory name.
#
# Nothing sensitive is hardcoded in this file — every literal is derived at
# runtime from .env / the commit-guard denylist / the private skill trees, so
# the guard itself is safe to commit. Bypass intentionally: git commit --no-verify.

set -euo pipefail

WORKSPACE_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
ENV_FILE="$WORKSPACE_ROOT/.env"
GUARD_SCAN="$HOME/.config/commit-guard/scan"

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

# D. operator-personal-skill names = the skill dirs in ~/.claude/skills and
# <main checkout>/.nucleus/.claude/skills, MINUS repo-wired .claude/skills.
# Whole-word. The main checkout is resolved through the common git dir so a
# linked worktree (which has no .nucleus/ of its own) still sees the private
# tree. A tiny allowlist covers generic/functional markers that legitimately
# appear as constants in infra code, plus machine-wide skill names that are
# ordinary words used throughout the codebase.
SKILL_ALLOW="test-skill handoff orchestration synced computer-use"
MAIN_CHECKOUT="$(git -C "$WORKSPACE_ROOT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)"
MAIN_CHECKOUT="${MAIN_CHECKOUT%/.git}"
{ [ -n "$MAIN_CHECKOUT" ] && [ -d "$MAIN_CHECKOUT" ]; } || MAIN_CHECKOUT="$WORKSPACE_ROOT"
# Repo-wired names come from the COMMITTED tree (HEAD), not the index: reading
# the index would let a commit that stages .claude/skills/<private name>/ make
# that name "repo-wired" and pass in the same commit. An unborn HEAD (first
# commit) yields an empty list, so every private name stays blocked.
repo_skills=""
if git -C "$WORKSPACE_ROOT" rev-parse -q --verify 'HEAD^{tree}' >/dev/null 2>&1; then
  repo_skills="$(git -C "$WORKSPACE_ROOT" ls-tree -r --name-only HEAD -- .claude/skills 2>/dev/null | cut -d/ -f3 | sort -u || true)"
fi
PRIVATE_SKILLS=()
for tree in "$HOME/.claude/skills" "$MAIN_CHECKOUT/.nucleus/.claude/skills"; do
  [ -d "$tree" ] || continue
  for d in "$tree"/*/; do
    [ -d "$d" ] || continue                                        # unmatched glob
    name="$(basename "$d")"
    case "$name" in .*) continue ;; esac
    printf '%s\n' "$repo_skills" | grep -qxF "$name" && continue   # repo-wired, fine
    printf '%s' "$SKILL_ALLOW" | grep -qwF "$name" && continue     # generic marker
    WORD+=("$name")
    PRIVATE_SKILLS+=("$name")
  done
done

# ── read the haystack; reduce a staged diff to ADDED lines ─────────────────
HAY="$(cat)"
[ -z "$HAY" ] && exit 0
case "$HAY" in
  "diff --git"*)
    # E. structural: nothing under .nucleus/ may be staged. The path check
    # reads the index directly, so it holds even for a staged file whose
    # content matches none of the literal layers.
    # `-z` output is never quoted, so non-ASCII names (which core.quotePath
    # would print as "\303\247...") still match the path patterns below.
    staged_paths="$(git -C "$WORKSPACE_ROOT" diff --cached --name-only --no-renames -z 2>/dev/null | tr '\0' '\n' || true)"
    staged_private="$(printf '%s\n' "$staged_paths" | grep -iE '^\.nucleus(/|$)' || true)"
    if [ -n "$staged_private" ]; then
      {
        echo "✖ staged paths under .nucleus/ (the gitignored operator-private tree):"
        printf '%s\n' "$staged_private" | sed 's/^/    - /'
        echo ""
        echo "  Unstage: git restore --staged -- .nucleus"
      } >&2
      exit 2
    fi
    # D (structural): a staged .claude/skills/<name>/ path whose <name> is a
    # private skill. Caught by path, so it holds even when the copied files
    # never spell the name in their content.
    staged_skill_dirs="$(printf '%s\n' "$staged_paths" | grep -E '^\.claude/skills/[^/]+/' | cut -d/ -f3 | sort -u || true)"
    leaked_skills=()
    if [ -n "$staged_skill_dirs" ]; then
      for name in ${PRIVATE_SKILLS[@]+"${PRIVATE_SKILLS[@]}"}; do
        printf '%s\n' "$staged_skill_dirs" | grep -qxF -- "$name" && leaked_skills+=("$name")
      done
    fi
    if [ ${#leaked_skills[@]} -gt 0 ]; then
      {
        echo "✖ staged .claude/skills/ directories named after operator-private skills:"
        printf '    - .claude/skills/%s/\n' "${leaked_skills[@]}"
        echo ""
        echo "  Private skills live in .nucleus/.claude/skills or ~/.claude/skills."
        echo "  Bypass intentionally: git commit --no-verify"
      } >&2
      exit 2
    fi
    # New paths of the files in the diff (`+++ b/<path>`, `rename to`,
    # `copy to`), with git's C-style quoting undone so non-ASCII names match.
    diff_paths="$(printf '%s\n' "$HAY" | perl -ne 'if (/^\+\+\+ "?b\/(.*?)"?$/ || /^(?:rename|copy) to "?(.*?)"?$/) { my $p = $1; $p =~ s/\\([0-7]{3})/chr(oct($1))/ge; print "$p\n" }' || true)"
    HAY="$(printf '%s' "$HAY" | grep '^+' | grep -v '^+++' || true)"
    [ -n "$diff_paths" ] && HAY="$HAY"$'\n'"$diff_paths"
    # grep, not ${HAY//...}: bash 3.2's pattern substitution is quadratic and
    # took minutes on a diff of a few thousand lines.
    printf '%s' "$HAY" | grep -q '[^[:space:]]' || exit 0
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

# B. commit-guard denylist, through its scanner. Exit 1 = match; any other
# non-zero exit is a scanner failure and blocks too (fail closed). The
# scanner names the denylist line, never the term, because its stderr is
# captured here. A clone on a machine without commit-guard skips the layer.
if [ -x "$GUARD_SCAN" ]; then
  guard_rc=0
  guard_out="$(printf '%s\n' "$HAY" | COMMIT_GUARD_SHOW_TERMS=0 "$GUARD_SCAN" text 2>&1 >/dev/null)" || guard_rc=$?
  if [ "$guard_rc" -eq 1 ]; then
    while IFS= read -r l; do
      case "$l" in "    - "*) FOUND+=("denylist/skill:${l##*: }") ;; esac
    done <<< "$guard_out"
  elif [ "$guard_rc" -ne 0 ]; then
    FOUND+=("denylist/skill:commit-guard scanner failed (exit $guard_rc)")
  fi
fi

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
    echo "  Route real values through .env / ~/.config/commit-guard/denylist / .nucleus/.claude/skills."
    echo "  Bypass intentionally: git commit --no-verify"
  } >&2
  exit 2
fi

exit 0
