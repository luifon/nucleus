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
- **Personal information of any kind** — anything identifying a person,
  account, contact, or external party
- **Operator-personal-skill content** — anything belonging to a skill in the
  `~/.claude/skills/` tree (as opposed to the repo-wired `.claude/skills/`).
  That tooling and everything it names lives in the personal tree, never in
  a tracked file.

**This repo is public.** Nothing above may appear in tracked source, docs,
comments, or test fixtures. If the value should be identical for everyone
who clones the repo (cron schedules, ports, denylists, behavior toggles), it
goes in `nucleus.toml`. If it identifies a specific operator's setup, it goes
in `.env`. The specific real literals a value-scan can't infer (names that
aren't env values) go in the gitignored `.claude/secret-strings` denylist.

**Placement rule:** anything that integrates or names a specific external
product/service is operator-personal → `~/.claude/skills/` + `.env`, never
under `tools/`, `core/`, `chores/`, or `docs/` (those are venue/
infrastructure code that refers to any external party by role, not name).

`tools/check-secrets.sh` (wired into Write/Edit, `git commit`, and the git
pre-commit hook) enforces this: it scans for `.env` values, the
`.claude/secret-strings` denylist, generic PII (emails/JIDs/phones/home
paths), and operator-personal-skill names. Bypass intentionally only with
`git commit --no-verify`. Full policy in `docs/SECRETS.md`.

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
- A WhatsApp DM from the operator's JID (per `WHATSAPP_ALLOWED_DM_JIDS`,
  ADR-005b) — replying in the same DM thread is pre-authorized
- The configured home channel (`DISCORD_HOME_CHANNEL_ID`)
- A pre-authorized scheduled output (daily news, timesheet reminder, weekly
  distillation digest)

…ask. Don't infer "the user probably wants this." For WhatsApp specifically,
remember messages appear from the user's own identity, not a separate bot
account — accidentally posting in the wrong chat is socially expensive.

## Rule 7 — Code identity is the venue, not the persona

The two existing messaging surfaces follow this split:

- **Crate / binary / launchd service / DB / log / diary dir** → named after
  the **venue** (`discord`, `whatsapp`). This is what shows up in `ps`,
  `launchctl list`, file paths.
- **Persona** (the character voice configured per bot) lives in
  `messaging/<venue>/persona.md` and is decoupled. Substituted into the
  system prompt via `--append-system-prompt` at spawn time.

The bot can sign messages with the persona name in user-facing content
(diary entries, /remember footer, etc.) — that's the character. But code
identifiers stay venue-based for searchability.

We renamed `jerry` → `discord` once for this reason. Don't recreate. No
further venues are planned; if that ever changes, keep the split.

## Rule 8 — WhatsApp gotchas worth remembering

- `sock.end(undefined)` to close a short-lived helper script's connection.
  **Never** `sock.logout()` — that unlinks the device and forces a fresh QR
  pair. (`messaging/whatsapp/src/send.ts` is the reference.)
- For Baileys connection setup, use `fetchLatestWaWebVersion({})` (not the
  Baileys-shipped version) and `browser: Browsers.macOS('Chrome')`. Without
  these the connection 405-loops silently.

## Rule 9 — Writing into the Obsidian vault (T3 / second brain)

The vault at `~/Documents/Obsidian/` is PARA-organized with three
extension buckets (see ADR-005). 8 top-level folders, renumbered
2026-05-21 to a rainbow scheme:

| # | Folder | Belongs here |
|---|---|---|
| 0 | `0-Inbox` | Unclassified captures; "I'll figure out where this goes later" |
| 1 | `1-Main-Notes` | Hubs / MOCs / recurring-question answers (curated by user) |
| 2 | `2-Daily-Notes` | Time-anchored journal entries (`YYYY-MM-DD.md`) |
| 3 | `3-Projects` | Short-term efforts with deadline + outcome |
| 4 | `4-Areas` | Ongoing responsibilities, no end date |
| 5 | `5-Resources` | Reference material on topics of interest |
| 6 | `6-Slipbox` | Atomic evergreen ideas (Zettelkasten, flat, no sub-folders) |
| 7 | `7-Archives` | Inactive items from any of the above |

The brain-dump pipeline writes via a multi-op plan — each capture can
emit multiple `create` / `append` / `move` ops. When you (or a bot)
write into the vault, follow these rules:

1. **Decompose by major theme.** A long capture should produce multiple
   files (one per major theme), not one big markdown. Sub-headings
   inside each file separate sub-themes. Don't atomize into
   one-idea-per-file unless you're writing to `6-Slipbox` (where one
   idea per note IS the convention).

2. **Pick the right bucket** using the per-bucket README files as ground
   truth (`0-Inbox/README.md`, `1-Main-Notes/README.md`,
   `2-Daily-Notes/README.md`, `3-Projects/README.md`,
   `4-Areas/README.md`, `5-Resources/README.md`,
   `6-Slipbox/README.md`, `7-Archives/README.md`). Quick routing rules:
     - Time-anchored ("log this for today", "today I learned...") → `2-Daily-Notes/YYYY-MM-DD.md`
     - Atomic evergreen idea, not tied to a Project/Area → `6-Slipbox/`
     - Concrete project work → `3-Projects/<X>/` (X must already exist)
     - Ongoing responsibility → `4-Areas/<X>/` (X must exist)
     - Reference material → `5-Resources/<X>/` (X must exist)
     - Hub / index / "main notes" (ONLY when explicitly directed) → `1-Main-Notes/`
     - Explicit archive → `7-Archives/`
     - Can't classify → `0-Inbox/` (better under-classify than mis-file)

3. **Sub-folder creation is allowed but gated.** New sub-folders under
   `3-Projects/`, `4-Areas/`, or `5-Resources/` may be created ONLY when
   the capture itself explicitly directs it ("create a folder for X",
   "this is a project for Y", "Y is one of my projects, put it there").
   Speculative creation by the bot is forbidden — when in doubt,
   `0-Inbox/` (or `6-Slipbox/` for atomic ideas). The op carries a
   `createsSubfolder: true` flag that the validator gates on; lying
   about the flag means your op gets rejected. `6-Slipbox` is flat —
   never create sub-folders there.

4. **Prefer APPEND over CREATE** when an existing file already covers a
   theme. Look at existing notes' titles + frontmatter; if a captured
   fragment overlaps, append to that file (the bot adds a dated
   separator) instead of creating a new duplicate. For `2-Daily-Notes`,
   if today's note already exists, ALWAYS append with a `## HH:MM`
   sub-heading rather than creating a new file.

5. **META-CORRECTIONS use `move` ops.** When a capture is correcting a
   prior misfile ("that note from earlier should be in Projects/X"),
   use a `move` op to actually relocate the prior file. Don't create a
   new note describing what should happen — the correction does the work.

6. **Link siblings.** Read the immediate sibling notes in the destination
   folder and add `[[wiki-links]]` to anything thematically related.
   Don't fabricate links to notes that don't exist. `6-Slipbox` notes
   especially: link liberally to other slipbox notes or to the relevant
   Area/Resource so they don't orphan.

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
- `--channels whatsapp-dm`
- `--channels discord-home,whatsapp-dm` (delivers to both, per-channel retry)

**Condition watchers (ADR-024).** `--condition "<shell cmd>"` gates the
fire: at each due tick the command runs (`sh -c`, 5s timeout) and only
exit 0 lets the reminder fire. Gated cron ticks skip to the next match;
gated one-shots keep watching every tick ("fire as soon as X" — e.g.
`--at now --condition "test -f /tmp/done"` watches until a marker
appears). `--condition-mode change` fires only on a false→true
transition (a persistently-true condition alerts once, not every
match). Truthy stdout of the form `{"context":"..."}` is appended to
the fire payload as evidence. Use this instead of scheduling a
skill-fire that spawns a session just to discover there's nothing to
do — the check costs a subprocess, the session only spawns on change.
A broken watcher (timeout/spawn failure) is recorded as a failure;
one-shots auto-pause on it, fix the script and `reminders resume`.

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
- `whatsapp-dm` — WhatsApp DM to the operator's JID (first entry of
  `WHATSAPP_ALLOWED_DM_JIDS`, ADR-005b). Goes through the outbound_queue
  in `memory/whatsapp.db`; the WhatsApp bot drains every 1s. The
  delivery target may be supplied as a bare digit string or a full
  `<digits>@s.whatsapp.net` JID — the bot normalizes either form.
- (The brain-dump group is NOT a reminder destination. The brain-dump
  pipeline owns that surface for capture only; personal reminders go
  to DM.)
- `calendar` — creates a Google Calendar event via JARVIS + Claude.ai
  Calendar MCP (ADR-007). The event is created on the trash account
  (`$NUCLEUS_GMAIL_ACCOUNT`) with `$NUCLEUS_PERSONAL_EMAIL` as attendee,
  so the invite lands on the user's main calendar with native phone/watch
  alerts. Event duration defaults to `[gmail].calendar_default_duration_min`
  (30 min). Use this when the user wants an actual *calendar invite*,
  not just a one-shot ping — "put dentist Monday 17h on my calendar",
  "schedule the Q3 review for tomorrow 9am". Cron-recurring reminders
  on `--channels calendar` create one event per fire — fine for
  occasional recurrence, wrong for "every weekday" (you'd flood the
  calendar). Prefer `discord-home` or `whatsapp-dm` for those.

Pick the channels based on where the user asked. "Remind me on
WhatsApp" → `whatsapp-dm`. No default to WhatsApp — Discord is the
safe default for
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

### Skill-fire reminders (`--system-prompt`, ADR-008)

For a reminder whose job is to *do something at fire time* (run a skill,
read state and summarize, orchestrate a multi-step task) rather than
just post a static body, use `--system-prompt` instead of `--body`:

```bash
./target/release/reminders add \
  --cron "20 8 * * 1-5" \
  --system-prompt "Run pre-meeting-prep skill, post results to discord-home." \
  --channels discord-home
```

At fire time the worker spawns a one-shot interactive Claude session
inside the `nucleus-reminders-fire` tmux session, sends the
`system_prompt` as the first message (with a small routing-hint
preamble), and forwards the session's reply to the listed channels.
All skills (project `.claude/skills/` + operator `~/.claude/skills/`)
auto-load, so the prompt can name a skill by `/<name>` or describe an
ad-hoc task. The session's final reply IS the post — don't add
preamble like "Here is the summary:".

Rules:

- `--body` and `--system-prompt` are **mutually exclusive**. Pick one.
- `--channels` is **optional** for `--system-prompt` reminders. Falls
  back to `[reminders].default_channels` in `nucleus.toml`, then to
  `discord-home` if unset. For `--body` reminders `--channels` keeps
  its original default of `discord-home`.
- Outer-error alerts (spawn failure, empty reply, ask() timeout) go to
  the listed channels with a `⚠️ Reminder #N fire failed: …` body.
- Per-tick file lock at `memory/reminders-tick.lock` serializes ticks
  so a long fire doesn't get duplicated by the next minute's launchd
  tick. Stale (>10min) lockfiles are reclaimed automatically.

Prefer `--body` for simple text pings. Reserve `--system-prompt` for
fires that genuinely need a Claude session — they cost a tmux+session
spawn each time. The 18:30 timesheet stays on `--body` forever.

## Rule 11 — Skills (ADR-008): never author in one shot

A skill (`SKILL.md`) is **procedural memory** — a memorized "when X
comes up, do Y" the bot can invoke. See ADR-008. Two storage trees:

- `.claude/skills/<name>/` — **developer/repo** workflows. Committed.
- `~/.claude/skills/<name>/` — **operator-personal** routines. Not
  committed; the operator owns this tree.

Default to `~/.claude/skills/` for anything that names a real tool,
contact, URL, or routine. Rule 1 still applies — identifiers don't go
into committed files even via skill bodies.

**Skills are now also written autonomously** by the `skill-gap-learner`
(ADR-017): an on-the-fly reviewer after conversations + a daily curator.
Its skills land in `~/.claude/skills/` with `flavor: learned` +
`created_by: agent`, validated against the SKILL.md contract (the
`# Failure modes` requirement below is enforced mechanically — malformed
writes are quarantined to `.rejected/`). So expect agent-authored skills
to appear there; the curator archives stale ones to `.archive/` (never
deletes; skips `pinned: true`). `/skill-creator` remains the operator's
*manual* path. The authoring discipline below governs both.

When invoking `/skill-creator create`, **name the destination path in
the prompt** rather than letting the model infer it. Example:
`/skill-creator create daily-digest at ~/.claude/skills/daily-digest` (personal)
vs `/skill-creator create rust-build-check at .claude/skills/rust-build-check`
(generic, repo-committed). If a SKILL.md appears unexpectedly in
`git status`, that's the signal — the model placed it in the committed
tree when you meant personal. Move it before committing.

Authoring discipline:

1. **Do not author a new skill in a single bot turn.** Too many wrong
   turns get baked in. The right pattern is exploratory session →
   capture what worked → formalize via skill-creator → test from a
   fresh session.
2. **Every skill must include a `# Failure modes` section.** An empty
   one signals the skill hasn't been thought through. Reviewers
   (the user, or future-you) should bounce skills that skip it.
3. **Frontmatter additions on top of Claude Code's contract** (per
   ADR-008): `flavor: recipe|learned`, `mcp_needed`, `last_used`,
   `last_failure`, `failure_count_30d`, `notify_on_failure`. These
   are bot/operator-edited; Claude Code ignores unknown frontmatter,
   the bot reads them as part of its system context.
4. **Fires from a reminder**: the spawned session's reply is what
   posts. The persona at `chores/reminders/persona.md` enforces the
   ready-to-send-reply contract — don't undo it.

## Rule 12 — Dashboard wire types are generated, not hand-written

`nucleus-dashboard/web/src/lib/api/generated/` is ts-rs output derived
from the Rust DTO structs (ADR-020). Never edit those files. When you
change a `#[derive(ts_rs::TS)]` struct in the API (or the shared types in
core / reminders), regenerate and commit the result:

```bash
cd nucleus-dashboard/web && npm run generate:api
```

`npm run check:api` is the drift gate (regenerate + `git diff
--exit-code`) — run it before committing API-shape changes. Hand-written
files under `lib/api/` keep only fetch functions and documented UI-layer
narrowings (status unions etc.) layered over the generated wire types.
New `i64`/`u64` DTO fields need `#[ts(type = "number")]` (or
`"number | null"`) — ts-rs defaults them to `bigint`, which breaks
JSON-parsed numbers.

## When in doubt

- `docs/SECRETS.md` — env-vs-toml policy + pre-commit audit
- `docs/ADR-*.md` — architecture decisions and why (especially ADR-005 for the vault, ADR-008 for skills)
- `README.md` — setup + operating cheatsheet
- `$NUCLEUS_TIER2_DIR/MEMORY.md` (typically `~/.claude/projects/<cwd-encoded>/memory/MEMORY.md`) — Tier 2 shared facts (auto-loaded; check the index there for what's already known about the user)
