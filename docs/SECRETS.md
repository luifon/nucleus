# Secrets, identifiers, and what goes where

**Hard rule:** anything that personally identifies a specific human, machine,
account, channel, path, or external party lives in `.env` (gitignored) — and
**this repo is public**, so personal information of any kind, and any content
belonging to an operator-personal skill (the gitignored
`.nucleus/.claude/skills/`, as opposed to the repo-wired `.claude/skills/`;
see ADR-032), must never appear in a tracked file.
Sensitive literals that aren't `.env` values go in the machine-wide
commit-guard denylist `~/.config/commit-guard/denylist`, outside every repo.
The global git hooks (`~/.config/git/hooks/`) scan every commit on the
machine against it, and `tools/check-secrets.sh` enforces the rest (see
`.claude/rules/secrets.md`). Nothing committed to the repo should
let a reader figure out who runs Nucleus, where, or for whom.

**Second hard rule:** personal project state (roadmaps, todo lists, sprint
plans, "things I'm currently postponing", references to specific contracts
or third-party tools you happen to use) doesn't belong in version control
either. Architecture and policy docs (`ADR-*.md`, `SECRETS.md`) are fine —
they describe how the system works. Roadmaps describe what you, personally,
are doing right now, and that drifts daily. Keep those local + gitignored.

## What lives in `.env` (never committed)

- `DISCORD_BOT_TOKEN` — bot auth
- `NUCLEUS_USER_NAME` — substituted into personas + prompts at load
- `NUCLEUS_WORKSPACE_ROOT` — absolute path to this repo on disk
- `NUCLEUS_TIER2_DIR` — absolute path to shared-memory dir
- `DISCORD_HOME_CHANNEL_ID`, `DISCORD_ALLOWED_USER_IDS`
- `WHATSAPP_ALLOWED_CHAT_IDS`

Template lives in `.env.example` with placeholders.

## What lives in `nucleus.toml` (commit-safe)

Non-identifying tunables only:
- Cron schedules
- Retention windows
- Port numbers
- `disallowed_tools` denylist
- Permission mode
- Behavior toggles (`mention_only_in_channels`, etc.)

Template lives in `nucleus.toml.example`.

## Templated files (`*.example`)

Use `__USER_HOME__` for paths and `${USER_NAME}` for the user's name.
Substitution happens at install/load time:

- `tools/launchd/*.plist.example` — `install.sh` substitutes `__USER_HOME__` → `$HOME`
  before copying to `~/Library/LaunchAgents/`
- `tools/cloudflared/*.yaml.example` — same placeholder, manual substitution
- Persona files (`messaging/*/persona.md`) — `${USER_NAME}` substituted at config load
- Docs and prompts in source — use `${USER_NAME}` or generic terms ("the user", "you")

## Code rules

- **Never** hardcode a username, channel ID, user ID, file path containing a
  user's home dir, or other identifier in committed `.rs` / `.ts` / `.md` /
  `.toml` files.
- Always read identifying values via `env_required("VAR_NAME")` (Rust:
  `nucleus_core::config`) or `envRequired("VAR_NAME")` (TS).
- Generic prompts must use the templated user name from settings, not a
  hardcoded literal.

## Commit guard (machine-wide)

- `~/.config/commit-guard/denylist` — one term per line, `#` comments. Mode
  600, directory 700. Matched as whole words, case-insensitive and
  accent-insensitive (`Joao` matches `João`), with no minimum length. The
  words of a multi-word term match across any run of spaces, `_`, `-` or `.`.
- `~/.config/commit-guard/scan` — the scanner (Python 3). Modes: `staged`
  (added lines and new paths of the staged changes), `message <file>`,
  `text` (stdin), `files` (every tracked file of the current repo), `count`.
  `COMMIT_GUARD_DENYLIST` overrides the denylist path (tests use it). A hit
  names the location and the denylist line number; the term itself is
  printed only when stderr is a terminal.
- `~/.config/commit-guard/hook` — the dispatcher. Every file in
  `~/.config/git/hooks/` (`git config --global core.hooksPath`) is a symlink
  to it. `pre-commit` and `commit-msg` scan; every hook name then runs the
  repository's own hook from `<git common dir>/hooks/` with the same
  arguments and stdin.
- A repository with its own `core.hooksPath` (husky, a tracked `.githooks/`)
  does not run the global hooks; its tracked `pre-commit` and `commit-msg`
  call the scanner directly and skip it with a warning when it is missing.
- A term that legitimately belongs in one repository:
  `git config --local --add commit-guard.allow '<term>'` (untracked
  `.git/config`).

## Checking before a commit

```bash
# Quick audit — these should return zero matches in committed files:
git ls-files | xargs grep -l "$NUCLEUS_USER_NAME\|$DISCORD_HOME_CHANNEL_ID" 2>/dev/null
```

If anything shows up, route it through env vars before committing.
