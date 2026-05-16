---
description: Sensitive values stay in .env — committed source must never contain personal identifiers. Three enforcement layers backstop the rule.
---

# Secrets stay in `.env`

Any value that identifies a specific operator, account, contact, or
location goes in `.env` (gitignored) and is referenced via env var. The
incident on 2026-05-16 — phone number, LID, trash-account email leaked
into committed files — is the reason this rule has teeth, not just text.

## What counts as sensitive

- Phone numbers, LIDs, WhatsApp / Discord JIDs
- Email addresses (yours and any third party's)
- API tokens, OAuth credentials, keys
- Absolute paths under `/Users/<your-user>/` or `/home/<your-user>/`
- Public hostnames you control
- Anyone's name (use `${USER_NAME}` substitution)
- Anything that would be regrettable in public `git blame`

## How to route it

| You're writing | Right move |
|---|---|
| Code that needs an identifier | Read from env (e.g., `settings.gmail.account`) |
| Persona / prompt template | Use `${USER_NAME}` / `${GMAIL_ACCOUNT}` placeholder, substituted at load via `nucleus_core::config::substitute*()` |
| `.env.example` | Obviously-fake placeholders (`5511999999999`, `you@example.com`), never real values |
| ADR / docs | Refer by role ("the trash account", `$NUCLEUS_PERSONAL_EMAIL`), never the literal |
| Test data | Synthetic values, not redacted real values |

## Enforcement layers

All three call `tools/check-secrets.sh`, which **auto-derives the
blocklist from your live `.env` values**. Add a new env var → all three
layers cover it without code changes.

| Layer | Where | Fires on |
|---|---|---|
| Claude `PreToolUse` on `Write` / `Edit` | `.claude/settings.local.json` (gitignored) | The earliest point — before a leaked value even hits disk |
| Claude `PreToolUse` on `Bash` (matching `git commit`) | same | Before I invoke `git commit` |
| Git `pre-commit` hook | `.git/hooks/pre-commit` (not tracked by git ever) | Universal backstop — any `git commit` from any source |

Bypass when intentional (e.g., committing a fixture with an env-shaped
value that genuinely belongs in the test): `git commit --no-verify` for
the shell-level hook, or explicitly tell me what to commit and accept
the block.

## Pre-commit audit (manual)

```bash
git diff --cached --no-color | tools/check-secrets.sh
```

Zero output + exit 0 = clean. Same logic the hooks run.
