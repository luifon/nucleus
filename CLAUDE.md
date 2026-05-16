# Claude rules for working on Nucleus

This file is auto-loaded into every Claude Code session opened in this
workspace (including the long-lived `claude` sessions the bots run inside
tmux). Apply these rules without being asked.

## Rule 1 — Identifiers live in `.env`, not in committed files

If you're about to type any of these into a committed source/doc file, stop
and route through `.env` instead:

- Any user's name (use `${USER_NAME}` in personas/prompts; substituted at
  load via `nucleus_core::config::substitute()`)
- Any Discord/WhatsApp user, channel, or group ID
- Any specific public hostname (route via `NUCLEUS_*_PUBLIC_URL` env vars)
- Any absolute path containing a user's home dir

If the value should be identical for everyone who clones the repo (cron
schedules, ports, denylists, behavior toggles), it goes in `nucleus.toml`.
If it identifies a specific operator's setup, it goes in `.env`.

Pre-commit audit:

```bash
git ls-files | xargs grep -l "$NUCLEUS_USER_NAME\|$DISCORD_HOME_CHANNEL_ID" 2>/dev/null
```

Zero matches = clean. Full policy in `docs/SECRETS.md`.

## Rule 2 — Personal project state stays gitignored

Roadmaps, todo lists, sprint plans, references to specific third-party tools
or contracts you happen to use — all gitignored. The `docs/` folder holds
**architecture and policy docs only** (how the system works); ephemeral
planning docs live locally.

- Architecture / policy → `docs/ADR-*.md`, `docs/SECRETS.md` → committed
- Personal state → `docs/ROADMAP.md`, `docs/TODO.md`, `docs/PRIVATE.md` → gitignored (already in `.gitignore`)

If you find yourself writing "what I want to build next" into a committed
file, you're in the wrong file.

## Rule 3 — Templates ship; real configs don't

Every config file that gets a real-values copy has an `.example` template
committed alongside it:

- `.env` ← `.env.example`
- `nucleus.toml` ← `nucleus.toml.example`
- `tools/cloudflared/*.yaml` ← `*.yaml.example`
- `tools/launchd/*.plist` ← `*.plist.example`

Templates use these placeholders, substituted at install/load:

- `${USER_NAME}` → substituted by `nucleus_core::config::substitute()` when
  reading persona files at startup
- `__USER_HOME__` → substituted by `tools/launchd/install.sh` (plists) or
  manually for cloudflared yaml

When adding a new committed template, follow the same pattern. Don't bake
real values into the template "as a default."

## Rule 4 — Never use `claude -p` (headless). Use Session / SessionPool.

`-p` is API-only billing; the Max subscription only covers interactive
mode. Every Nucleus call goes through tmux-hosted long-lived sessions.

- **Rust**: `nucleus_core::claude_session::Session` for one-shot use, or
  `SessionPool` for keyed-per-chat live sessions (Discord uses this).
- **TypeScript** (`messaging/whatsapp/`): `Session` / `SessionPool` in
  `src/claude_session.ts` — mirrors the Rust API.

Centralizes permission mode, denylist plumbing, persona injection, the
tmux-window lifecycle, transcript-tail parsing. Bypassing it = lose
visibility (`tmux attach -t nucleus-<bot>`) + inconsistent security
posture + duplicate wiring.

## Rule 5 — launchd-spawned processes need explicit env

launchd does **not** inherit your shell's PATH or any user env. Any plist
that needs to spawn `claude`, `npm`, or other dev-installed tools must set
in its `EnvironmentVariables` dict:

- `NUCLEUS_CLAUDE_BIN=__USER_HOME__/.local/bin/claude`
- `PATH=__USER_HOME__/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin`

Use `__USER_HOME__` and let `tools/launchd/install.sh` substitute `$HOME`
at install time. We've hit this once — the discord bot couldn't find
`claude` when launchd-spawned despite a working shell. Don't recreate.

## Rule 6 — Outbound messages to shared audiences need explicit user authorization

Both bots can technically send anywhere their accounts are authed for. Before
sending to a destination that isn't:

- A DM from the user / a message in the user's self-group
- The configured home channel (`DISCORD_HOME_CHANNEL_ID`)
- A pre-authorized scheduled output (daily news, timesheet reminder, weekly
  distillation digest)

…ask. Don't infer "the user probably wants this." For WhatsApp specifically,
remember messages appear from the user's own identity, not a separate bot
account — accidentally posting in the wrong chat is socially expensive.

## Rule 7 — Code identity is the venue, not the persona

When adding a new messaging surface (Signal, Telegram, etc.):

- **Crate / binary / launchd service / DB / log / diary dir** → named after
  the **venue** (`discord`, `whatsapp`, `signal`, …). This is what shows up
  in `ps`, `launchctl list`, file paths.
- **Persona** (the character voice — "Jerry Lewis", "Alfred", whoever)
  lives in `messaging/<venue>/persona.md` and is decoupled. Substituted
  into the system prompt via `--append-system-prompt` at spawn time.

The bot can sign messages with the persona name in user-facing content
(diary entries, /remember footer, etc.) — that's the character. But code
identifiers stay venue-based for searchability.

We renamed `jerry` → `discord` once for this reason. Don't recreate.

## Rule 8 — WhatsApp gotchas worth remembering

- `sock.end(undefined)` to close a short-lived helper script's connection.
  **Never** `sock.logout()` — that unlinks the device and forces a fresh QR
  pair. (`messaging/whatsapp/src/send.ts` is the reference.)
- For Baileys connection setup, use `fetchLatestWaWebVersion({})` (not the
  Baileys-shipped version) and `browser: Browsers.macOS('Chrome')`. Without
  these the connection 405-loops silently.

## Rule 9 — Writing into the Obsidian vault (T3 / second brain)

The vault at `~/Documents/Obsidian/` is PARA-organized (see ADR-005).
The brain-dump pipeline writes via a multi-op plan — each capture can
emit multiple `create` / `append` / `move` ops. When you (or a bot)
write into the vault, follow these rules:

1. **Decompose by major theme.** A long capture should produce multiple
   files (one per major theme), not one big markdown. Sub-headings
   inside each file separate sub-themes. Don't atomize into
   one-idea-per-file (Zettelkasten-style is NOT what we want).

2. **Pick the right bucket** using the per-bucket README files as ground
   truth (`0-Inbox/README.md`, `1-Projects/README.md`, etc). If unsure
   between two, prefer `0-Inbox/` — better to under-classify than mis-file.

3. **Sub-folder creation is allowed but gated.** New sub-folders under
   `1-Projects/`, `2-Areas/`, or `3-Resources/` may be created ONLY when
   the capture itself explicitly directs it ("create a folder for X",
   "this is a project for Y", "Y is one of my projects, put it there").
   Speculative creation by the bot is forbidden — when in doubt,
   `0-Inbox/`. The op carries a `createsSubfolder: true` flag that the
   validator gates on; lying about the flag means your op gets rejected.

4. **Prefer APPEND over CREATE** when an existing file already covers a
   theme. Look at existing notes' titles + frontmatter; if a captured
   fragment overlaps, append to that file (the bot adds a dated
   separator) instead of creating a new duplicate.

5. **META-CORRECTIONS use `move` ops.** When a capture is correcting a
   prior misfile ("that note from earlier should be in Projects/X"),
   use a `move` op to actually relocate the prior file. Don't create a
   new note describing what should happen — the correction does the work.

6. **Link siblings.** Read the immediate sibling notes in the destination
   folder and add `[[wiki-links]]` to anything thematically related.
   Don't fabricate links to notes that don't exist.

7. **Frontmatter every NEW file.** YAML block at top:
   ```yaml
   ---
   created: 2026-05-14
   source: distiller-contemplation   # or alfred-braindump, obsidian-chat, etc.
   tags: [optional, free-form]
   ---
   ```
   Append fragments don't need their own frontmatter (the target file
   already has one).

8. **Multi-file plans get an index.** When you create multiple files in
   a NEW sub-folder, also create an `index.md` or `README.md` in that
   folder linking the siblings.

T2-vs-T3 promote rule: if the candidate is a short, recurring,
behaviorally-binding fact ("user prefers terse replies"), promote to T2
(`$NUCLEUS_TIER2_DIR/`). If it's longer-form, narrative, or
project/area-tied ("design notes for the news-fetcher rescore pipeline"),
write to T3. Both can happen for the same candidate.

## Rule 10 — Scheduling reminders via the `reminders` CLI

A reminder is the universal primitive for time-triggered notifications
(see ADR-006). Use the `reminders` binary directly via Bash whenever
the user asks to be nudged at a future time or on a schedule.

**One-shot** ("remind me at 16:45 about dentist", "in 30 min nudge me
to check the deploy", "tomorrow 9am about Q3 sync") — use `--at`:

```bash
./target/release/reminders add \
  --at "2026-05-14T16:45:00<offset>" \
  --body "dentist appointment at 17h" \
  --channels discord-home
```

**Recurring** ("every weekday at 18:30 remind me to log hours", "every
Monday morning at 9 send me the weekly review prompt") — use `--cron`
with a standard 5-field cron expression (minute hour day month dow),
evaluated in `NUCLEUS_TZ`:

```bash
./target/release/reminders add \
  --cron "30 18 * * 1-5" \
  --body "⏰ End of day — time to log your hours." \
  --channels discord-home
```

`--at` and `--cron` are mutually exclusive. `--at` accepts RFC3339 with
offset (e.g. `2026-05-14T16:45:00-03:00`) or a naive local timestamp
that's interpreted in `NUCLEUS_TZ`. You're responsible for converting
natural language to ISO — read `$NUCLEUS_TZ` (or `/etc/localtime` if
unset) to resolve the offset, and resolve relative phrases ("tomorrow",
"in N hours") against *now*, not session start.

`--channels` is plural, comma-separated, and validates against the
known set. Default is `discord-home`. Examples:

- `--channels discord-home`
- `--channels alfred`
- `--channels discord-home,alfred` (delivers to both, per-channel retry)

Other subcommands:
- `reminders list` — active/pending reminders with next fire time and
  channels. Add `--include-fired` / `--include-cancelled` to broaden.
- `reminders show <id>` — full detail incl. channel state + recent fires
- `reminders cancel <id>` — terminate (the `add` command prints the new
  id on stdout — keep it in case the user wants to cancel)
- `reminders pause <id> [--until <iso>]` — temporarily disable; with
  `--until`, the ticker auto-resumes at that time
- `reminders resume <id>` — re-activate a paused reminder
- `reminders history [--days N] [--channel c] [--reminder id]` — audit
  log of fire attempts (per channel, success/error)

Delivery is once-per-minute via launchd (`reminders-tick.plist`,
`StartInterval=60`). A reminder due at 16:45:00 fires somewhere in
[16:45:00, 16:45:59]. **Fire-late policy:** if `next_fire_at` is in
the past (laptop closed, missed minute, whatever) the next tick fires
it anyway — one delivery, then advance to the next future match.

Supported `--channels` values:
- `discord-home` (default) — posts in the configured Discord home channel
- `alfred` — WhatsApp conversational group (first entry of
  `WHATSAPP_ALLOWED_GROUP_NAMES`). Goes through the outbound_queue in
  `memory/whatsapp.db`; Alfred drains every 5s
- `braindump` — WhatsApp Brain Dump group (first entry of
  `WHATSAPP_BRAINDUMP_GROUP_NAMES`). Same queue mechanism
- `calendar` — creates a Google Calendar event via JARVIS + Claude.ai
  Calendar MCP (ADR-007). The event is created on the trash account
  (`the-trash-account`) with `NUCLEUS_PERSONAL_EMAIL` as attendee, so the
  invite lands on the user's main calendar with native phone/watch
  alerts. Event duration defaults to `[gmail].calendar_default_duration_min`
  (30 min). Use this when the user wants an actual *calendar invite*,
  not just a one-shot ping — "put dentist Monday 17h on my calendar",
  "schedule the Q3 review for tomorrow 9am". Cron-recurring reminders
  on `--channels calendar` create one event per fire — fine for
  occasional recurrence, wrong for "every weekday" (you'd flood the
  calendar). Prefer `discord-home` or `alfred` for those.

Pick the channels based on where the user asked. "Remind me on
WhatsApp" or "remind me here" (when they're already in Alfred) →
`alfred`. No default to WhatsApp — Discord is the safe default for
unattended delivery, since the WhatsApp app is on the user's phone and
could be muted/inactive. "Schedule X" / "put X on my calendar" /
"invite me to X" → `calendar` (typically combined with `discord-home`
for a same-day heads-up: `--channels calendar,discord-home`).
Multi-channel ("remind me on Discord AND WhatsApp") works: pass a
comma list. Each channel retries independently up to 3 attempts before
giving up for that fire; the others aren't redelivered while a laggard
retries.

The 18:30 weekday timesheet reminder is seeded as a `created_by =
'system'` row on binary startup — don't add it manually. If the user
cancels it, the seeder won't re-create it (cancellation is sticky).

## When in doubt

- `docs/SECRETS.md` — env-vs-toml policy + pre-commit audit
- `docs/ADR-*.md` — architecture decisions and why (especially ADR-005 for the vault)
- `README.md` — setup + operating cheatsheet
- `$NUCLEUS_TIER2_DIR/MEMORY.md` (typically `~/.claude/projects/<cwd-encoded>/memory/MEMORY.md`) — Tier 2 shared facts (auto-loaded; check the index there for what's already known about the user)
