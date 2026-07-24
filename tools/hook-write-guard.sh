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
# For a tracked (committable) target, pipes the proposed content through
# tools/check-secrets.sh exactly as before.

set -euo pipefail
ROOT="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"

input="$(cat)"
fp="$(printf '%s' "$input" | jq -r '.tool_input.file_path // empty')"

# Gitignored target → never committed → don't scan.
if [ -n "$fp" ] && git -C "$ROOT" check-ignore -q -- "$fp" 2>/dev/null; then
  exit 0
fi

printf '%s' "$input" \
  | jq -r '(.tool_input.content // .tool_input.new_string // empty)' \
  | (cd "$ROOT" && ./tools/check-secrets.sh)
