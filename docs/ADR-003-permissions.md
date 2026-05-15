# ADR-003 — Discord Bot Security Model

**Status:** Accepted (2026-05-12)

## Context

Jerry Lewis (and Alfred on WhatsApp) drives long-lived `claude` sessions inside tmux windows on the host machine in response to inbound messages. The messaging surfaces widen the attack surface — a stolen token or compromised account becomes shell access through the bot. We need rails that don't cripple capability.

## Decisions

### Permission mode: `auto` + denylist

The session is spawned with `--permission-mode auto` so claude's auto-permission classifier handles routine tool use without prompting; there's no human watching the tmux window in real time. Destructive shell commands are blocked by an explicit `--disallowedTools` list:

```
Bash(rm:*), Bash(sudo:*), Bash(diskutil:*), Bash(dd:*),
Bash(shutdown:*), Bash(reboot:*), Bash(launchctl:*)
```

Maintained in `nucleus.toml` under `[claude.disallowed_tools]`, applied to every session spawn via `core::claude_session::Session`.

**Rejected: `default` (interactive prompts).** Sessions run unattended; a denial that waits for a human Yes/No just hangs the bot. The auto-classifier closes that loop while still respecting the denylist for hard nos.

**Rejected: `bypassPermissions`.** A Discord message saying "delete the news db" actually does it. Hard no.

### User allowlist

Bot ignores everyone whose Discord user ID is not in `nucleus.toml::discord.allowed_user_ids`. Default empty = bot does nothing. Same posture Hermes used (`DISCORD_ALLOWED_USERS`).

### Trigger model

- **DMs:** always respond.
- **Channels:** respond only when the bot is `@`-mentioned.

Rationale: DMs are 1:1, intent is unambiguous. Channels are shared, so mention-gating prevents Jerry from chiming in on every message.

### Persona injection

`messaging/discord/persona.md` is read at startup, passed via `--append-system-prompt`. Hot-reloadable on the next message — no bot restart required, just edit the file.

## Operational

- Bot runs as the workspace user account via launchd. The plist template is `tools/launchd/discord.plist.example`; `install.sh` generates the deployed file at `~/Library/LaunchAgents/<NUCLEUS_LAUNCHD_PREFIX>.discord.plist` (default prefix: `dev.nucleus`).
- Logs go to `~/Development/nucleus/memory/discord.log` (gitignored).
- Stop with `launchctl unload ~/Library/LaunchAgents/<prefix>.discord.plist`.

## Threat model in plain English

If the Discord bot token leaks, an attacker can DM the bot pretending to be you. They can:
- Read files in the workspace (and any `--add-dir` paths Jerry uses)
- Edit those files
- Run shell commands not in the denylist (so most reads, but not `rm`, `sudo`, etc.)

They cannot (with this config):
- Reboot or shut down the machine
- Delete files (without finding a workaround for `rm` — `find -delete` is a real risk we accept for v1)
- Touch other users' Discord activity

If the bot token leaks: rotate via Discord developer portal, update `.env`, restart the launchd service. The `allowed_user_ids` check is the last line of defense — even a stolen token can't impersonate the operator to *Jerry* if the attacker isn't messaging from a Discord account on the allowlist.
