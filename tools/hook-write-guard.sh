#!/usr/bin/env bash
# PreToolUse guard for Write/Edit. Reads the full tool-call JSON on stdin.
#
# Skips the secret scan when the target file is GITIGNORED: gitignored files
# (.env, .claude/secret-strings, .claude/settings.local.json, …) never get
# committed, and the secret-holding ones must legitimately contain real
# values — scanning them here would block editing the very files that feed
# the guard. The commit-time layers (git-commit PreToolUse + git pre-commit)
# are the real enforcement, and they never see gitignored content anyway.
#
# Skips it likewise for a target OUTSIDE the repo (~/.claude/skills/…, the
# operator-personal tree): this repo can never commit those, and they must
# legitimately name the very literals the denylist carries.
#
# For a tracked (committable) target, pipes the proposed content through
# tools/check-secrets.sh exactly as before. Any state where that scan can't
# be run at all is a block, never a pass.

set -euo pipefail
ROOT="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"

# Fail CLOSED on a bad ROOT. Every decision below trusts ROOT to be this repo:
# a wrong one makes every in-repo path look "outside the repo", and the skip
# right after would wave the write through with no scan at all — silently.
# A guard that can't verify must block, not shrug.
if ! git -C "$ROOT" rev-parse --show-toplevel >/dev/null 2>&1 \
   || [ ! -x "$ROOT/tools/check-secrets.sh" ]; then
  echo "✖ hook-write-guard: bad project root ($ROOT)" >&2
  echo "  Can't reach tools/check-secrets.sh, so the content is unverified." >&2
  echo "  Point CLAUDE_PROJECT_DIR at the repo root and retry." >&2
  exit 2
fi

input="$(cat)"
fp="$(printf '%s' "$input" | jq -r '.tool_input.file_path // empty')"

# Target outside the repo (e.g. ~/.claude/skills/…, the operator-personal
# tree) → this repo can never commit it → don't scan. Same rationale as the
# gitignored case below.
case "$fp" in
  "$ROOT"/*) ;;                 # inside the repo → keep checking
  /*) exit 0 ;;                 # absolute path elsewhere → out of scope
esac

# Gitignored target → never committed → don't scan.
if [ -n "$fp" ] && git -C "$ROOT" check-ignore -q -- "$fp" 2>/dev/null; then
  exit 0
fi

printf '%s' "$input" \
  | jq -r '(.tool_input.content // .tool_input.new_string // empty)' \
  | (cd "$ROOT" && ./tools/check-secrets.sh)
