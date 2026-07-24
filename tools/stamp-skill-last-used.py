#!/usr/bin/env python3
"""PostToolUse hook for the `Skill` tool — stamp `last_used: <today>` on the
invoked skill's SKILL.md frontmatter.

Why this exists: the dashboard's Skills page and the skill-gap-learner read
`last_used` straight from frontmatter, but the ONLY writer of that field was
the skill-gap-learner when it *patches* a skill. A skill that merely *runs*
(a skill firing every weekday, say) never bumped `last_used`, so it showed
as "never fired" forever. This hook closes that gap deterministically: it
fires on every `Skill` invocation, in every Nucleus surface, because all
bot/fire sessions `cd` into the repo and load `.claude/settings.json`.

Contract: reads the Claude Code PostToolUse hook JSON on stdin. No-op (exit 0)
for anything that isn't a `Skill` invocation or a skill we can't locate in the
two ADR-008 skill trees. NEVER blocks or errors out — always exits 0.

Skill trees (ADR-008):
  - operator-personal: $HOME/.claude/skills/<name>/SKILL.md
  - repo-committed:     $CLAUDE_PROJECT_DIR/.claude/skills/<name>/SKILL.md

Scope note: this only writes `last_used`. Failure tracking
(`last_failure` / `failure_count_30d`) needs the skill's *work* outcome, which
the Skill PostToolUse payload doesn't carry (it reports only that dispatch
succeeded) — that's a separate mechanism, intentionally not done here.
"""
import datetime
import json
import os
import re
import sys
import tempfile


def main() -> int:
    try:
        data = json.load(sys.stdin)
    except Exception:
        return 0
    if data.get("tool_name") != "Skill":
        return 0

    tool_input = data.get("tool_input") or {}
    tool_response = data.get("tool_response") or {}
    name = tool_input.get("skill") or tool_response.get("commandName")
    if not name or not isinstance(name, str):
        return 0
    # Plugin-namespaced skills (e.g. "skill-creator:skill-creator") live in a
    # plugin tree, not our two roots. Strip the namespace and try the bare dir;
    # if it doesn't resolve below it's a harmless no-op.
    bare = name.split(":")[-1].strip()
    if not bare or "/" in bare or bare.startswith("."):
        return 0  # defensive: never let a weird name escape the skill trees

    home = os.environ.get("HOME") or os.path.expanduser("~")
    repo = os.environ.get("CLAUDE_PROJECT_DIR") or data.get("cwd") or ""
    candidates = [os.path.join(home, ".claude", "skills", bare, "SKILL.md")]
    if repo:
        candidates.append(os.path.join(repo, ".claude", "skills", bare, "SKILL.md"))

    today = datetime.date.today().isoformat()  # local date, YYYY-MM-DD
    for path in candidates:
        if os.path.isfile(path):
            stamp(path, today)
    return 0


def stamp(path: str, today: str) -> None:
    """Set `last_used: <today>` in the SKILL.md frontmatter, atomically.

    Updates the existing line if present, else appends it inside the
    frontmatter block. Skips the write if it's already today (avoids needless
    mtime churn and write races between concurrent sessions). Refuses to touch
    a file with no frontmatter rather than fabricate one.
    """
    try:
        with open(path, "r", encoding="utf-8") as f:
            text = f.read()
    except Exception:
        return

    m = re.match(r"^---\n(.*?\n)---\n?", text, re.DOTALL)
    if not m:
        return  # no frontmatter block — leave it alone
    fm = m.group(1)
    rest = text[m.end():]

    new_line = f"last_used: {today}"
    line_re = re.compile(r"^last_used:.*$", re.MULTILINE)
    if line_re.search(fm):
        if re.search(rf"^last_used:\s*{re.escape(today)}\s*$", fm, re.MULTILINE):
            return  # already stamped today
        fm = line_re.sub(new_line, fm, count=1)
    else:
        if not fm.endswith("\n"):
            fm += "\n"
        fm += new_line + "\n"

    out = f"---\n{fm}---\n{rest}"
    tmp = None
    try:
        fd, tmp = tempfile.mkstemp(dir=os.path.dirname(path), prefix=".skill-stamp-")
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(out)
        os.replace(tmp, path)  # atomic on POSIX
    except Exception:
        if tmp:
            try:
                os.unlink(tmp)
            except Exception:
                pass


if __name__ == "__main__":
    sys.exit(main())
