# Nucleus

A personal-assistant stack that wires the Claude Code CLI into your Discord,
WhatsApp, and a unified operator dashboard.

Every brain call runs as a long-lived interactive `claude` session inside a
tmux window — so you can `tmux attach -t nucleus-discord` and watch a bot
think in real time. Everything runs locally on your Mac via launchd. The
brain is your existing Claude subscription — no separate API billing.

## What it does

| Surface | What you get |
|---|---|
| **Discord bot** (Jerry Lewis) | DM or @-mention → wakes Claude → replies with per-channel session continuity. Slash commands: `/status`, `/news`, `/remember`, `/forget`. |
| **WhatsApp bot** (Alfred) | Self-only group, voice memos transcribed locally via whisper.cpp, brain-dump classified and routed (TODOs → tasks, facts → memory, etc.). Iron-tight allowlist scoping. |
| **News pipeline** | Daily 9am RSS pull (HN, arXiv cs.AI, Simon Willison, Pragmatic Engineer, Latent Space, …) → Claude scores notability against your learned preferences → top items posted to Discord → full feed at `news.<your-domain>`. Upvote/downvote teaches the scorer. |
| **nucleus-dashboard** | Single operator app subsuming dashboard widgets, chat against your PARA vault, the public news API, and every admin surface (agents, skills, diary, reminders, vault writes) at `nucleus.<your-domain>`. See ADR-015/016. |
| **Distiller** | Single daily 4am pass (consolidated per ADR-016; absorbed the old preference learner) that promotes diary observations to long-term memory (PROMOTE / MERGE / ARCHIVE / DROP, Mem0-style ops). |
| **Reminders** | Ask either bot "remind me at 16:45 about dentist" → Claude schedules via the `reminders` CLI. Once-per-minute polling delivers to one or more channels (`discord-home`, `whatsapp-dm` via the bot-drained outbound queue, `calendar`). Supports `--at` (one-shot) and `--cron` (recurring) with pause/resume + per-channel retry; daily timesheet nudge is seeded as a recurring system reminder. |

## Architecture at a glance

```
              ┌────────────────────────────────────────────────────────┐
              │  Cloudflare tunnel                                     │
              │              nucleus.<dom>                             │
              └──────────────────────┬─────────────────────────────────┘
                                     │
                          ┌──────────▼───────────┐
   Discord ←─             │ nucleus-dashboard    │  ←── Obsidian vault
                          │ axum :8092           │
                          │ + React SPA          │
                          └──────────┬───────────┘
                                     │ SQLite
   WhatsApp ←─┐                      │
              │  ┌───────────────────▼──────┐
              │  │ news-fetcher (launchd 1x)│ ← claude session
              │  │ distiller (daily)        │
              │  │ gmail-metabolism         │
              │  │ skill-gap-learner (daily)│
              │  │ reminders-tick (60s)     │
   discord    │  └──────────────────────────┘
              │   (all agents declared in agents.toml — ADR-016;
              │    skill-gap-learner also reviews on-the-fly — ADR-017)
   (Rust,     │
   serenity)  │  ┌──────────────────────────────────────────────┐
              │  │  nucleus-core (Rust lib)                     │
   whatsapp ──┘  │  - claude_session::{Session, SessionPool}    │
   (TS,          │       (tmux-hosted long-lived claude — only  │
   Baileys,      │        path to the brain)                    │
   whisper.cpp)  │  - memory (Tier 2 promote/read)              │
                 │  - diary (Tier 1.5 per-agent journals)       │
                 │  - health::{Check,Registry}                  │
                 │  - mem0 client (Tier 4, deferred)            │
                 └──────────────────────────────────────────────┘
```

Every binary that needs Claude goes through `core::claude_session::Session`
(one-shot) or `SessionPool` (per-chat persistent) — the single seam for
permission mode, denylist, persona injection, the tmux window lifecycle, and
transcript-tail parsing. The TS port lives at `messaging/whatsapp/src/claude_session.ts`
and mirrors the same API.

Why tmux? `claude -p` (headless) is moving to API-only billing; the Max
subscription only covers interactive mode. Long-lived sessions also win on
latency — first message in a chat pays a ~5s cold spawn, every follow-up
in that chat is ~5s wall-time on a `--resume`d session. The 4-hour idle
reaper keeps the tmux server tidy.

## Quick start

### Prerequisites

- **macOS** (the launchd plists and shell paths assume this)
- **Rust** stable — `brew install rust` (or rustup; build is verified on 1.95.0)
- **Node 22+** — for the WhatsApp bot (uses built-in `node:sqlite`)
- **tmux** — `brew install tmux`. Every brain call runs inside a tmux window;
  no tmux, no bots.
- **claude CLI** — installed and authenticated against your Claude account.
  This is the brain. Verify with `claude --version`; then `claude` in a fresh
  shell to confirm the TUI loads (Ctrl+C to exit).
- **Claude.ai connectors enabled** — Nucleus relies on two account-level
  MCP integrations that you enable at [claude.ai](https://claude.ai)
  under Settings → Connectors (NOT via this repo's
  `.claude/settings.json`, which only covers Claude Code marketplace
  plugins):
  - **Gmail** — required for the daily inbox metabolism + label-based
    triage (ADR-007). Tools: `mcp__claude_ai_Gmail__*`.
  - **Google Calendar** — required for the `--channels calendar`
    reminder path (creates events on your trash account, invites your
    primary email; ADR-007). Tools:
    `mcp__claude_ai_Google_Calendar__create_event`.

  These auth against your personal Google account through Claude.ai
  and stay there — the tokens never enter this repo. Without them
  enabled, `gmail-metabolism` and the calendar channel fail at fire
  time with an MCP-unavailable error.
- **A Discord application + bot** — token in hand. Required intents:
  `Guilds`, `Guild Messages`, `Direct Messages`, `Message Content`.
- **A Cloudflare tunnel** (optional, but nucleus-dashboard expects to be
  reachable via a subdomain; you can also just hit `localhost:8092` if you
  don't care about remote access).
- **An Obsidian vault, PARA-organized** — required for brain-dump capture
  and the chat surface. The default vault path is
  `~/Documents/Obsidian/` (override in `nucleus.toml` under
  `[obsidian].vault_path`). The vault MUST have these eight top-level
  folders, each containing a `README.md` describing what belongs there
  (the bots read these READMEs to classify captures):
  ```
  ~/Documents/Obsidian/
  ├── 0-Inbox/README.md         capture-now-organize-later landing pad
  ├── 1-Main-Notes/README.md    hubs / MOCs / recurring-question answers
  ├── 2-Daily-Notes/README.md   time-anchored journal (YYYY-MM-DD.md)
  ├── 3-Projects/README.md      short-term efforts with deadline + outcome
  ├── 4-Areas/README.md         ongoing responsibilities to maintain
  ├── 5-Resources/README.md     reference material on topics of interest
  ├── 6-Slipbox/README.md       atomic evergreen notes (Zettelkasten)
  └── 7-Archives/README.md      inactive items from the other buckets
  ```
  See `docs/ADR-005-second-brain.md` for the PARA model and writing rules.
- **Optional, for WhatsApp voice memos:**
  - `brew install whisper-cpp ffmpeg`
  - Whisper model (~3GB for large-v3):
    ```bash
    mkdir -p ~/.cache/whisper/models
    curl -L -o ~/.cache/whisper/models/ggml-large-v3.bin \
      https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3.bin
    ```
  - Override the model path or binary via env if you keep them elsewhere:
    `WHISPER_MODEL_PATH=/path/to/model.bin`, `WHISPER_BINARY=whisper-cli`.

### 1. Clone + configure

```bash
git clone <this-repo> ~/Development/nucleus
cd ~/Development/nucleus

cp .env.example .env
cp nucleus.toml.example nucleus.toml
# Edit both — see "Configuration" below for what to fill in.
```

### 2. Build

```bash
cargo build --release
(cd messaging/whatsapp && npm install)
(cd nucleus-dashboard/web && npm install && npm run build)
(cd tools/playwright-auth && npm install && node playwright-auth.mjs init)
# init creates the (empty) browser-auth snapshot the Playwright MCP
# server seeds isolated sessions from (ADR-022). To carry real logins:
#   node playwright-auth.mjs login --url <site>   # log in, close window
```

### 3. Cloudflare tunnel (skip if running localhost-only)

```bash
# If you already have a tunnel, just add an ingress route; otherwise:
cloudflared tunnel create my-tunnel
cloudflared tunnel route dns my-tunnel nucleus.<yourdomain>

# Add an ingress entry in ~/.cloudflared/config.yml routing
# nucleus.<yourdomain> → http://localhost:8092 and run the tunnel:
cloudflared service install
```

> This exposes the whole dashboard publicly. Step 6 (Perimeter) locks the
> operator paths down to Tailscale and keeps only the public news API on
> the tunnel. See `tools/cloudflared/` for the path-scoped template.

### 4. Pair WhatsApp (one time)

```bash
cd messaging/whatsapp
npm run discover    # prints a QR code as PNG (opens in Preview) + ASCII
# WhatsApp → Settings → Linked Devices → Link a Device → scan
# Once paired, create a self-only WhatsApp group named (e.g.) "Alfred"
# and put that name in .env under WHATSAPP_ALLOWED_GROUP_NAMES.
```

### 5. Install services

```bash
./tools/launchd/install.sh
# Substitutes __USER_HOME__ → $HOME and __TZ__ → $NUCLEUS_TZ (auto-
# detected from /etc/localtime if unset) in each plist template, copies
# to ~/Library/LaunchAgents/, loads via launchctl. 8 services total
# (discord, whatsapp, nucleus-dashboard, news-fetcher, gmail-metabolism,
# distiller, reminders-tick, skill-gap-learner).
```

> **Upgrading from a pre-ADR-009 install?** The WhatsApp service was
> renamed `alfred → whatsapp` (Rule 7 — venue names in code, persona
> names only in config). Before re-running `install.sh`, unload the old
> service so launchctl doesn't keep firing it:
>
> ```bash
> launchctl bootout gui/$UID/${NUCLEUS_LAUNCHD_PREFIX:-dev.nucleus}.alfred
> # then re-run install.sh — it will load dev.nucleus.whatsapp fresh
> ```

> **Upgrading from a pre-ADR-016 install?** The distiller's two jobs were
> consolidated into one daily job, and preference-learner was sunset. Unload
> the retired services before re-running `install.sh`:
>
> ```bash
> P=gui/$UID/${NUCLEUS_LAUNCHD_PREFIX:-dev.nucleus}
> launchctl bootout $P.distiller-hourly $P.distiller-weekly $P.preference-learner 2>/dev/null
> tmux kill-session -t nucleus-jarvis 2>/dev/null   # renamed → nucleus-gmail
> # then re-run install.sh — it loads dev.nucleus.distiller fresh
> ```

Verify:
```bash
launchctl list | grep "${NUCLEUS_LAUNCHD_PREFIX:-dev.nucleus}"
curl http://127.0.0.1:8092/agents/api/list | jq   # every agent + live state (ADR-016)

# Optional: prove the Session machinery actually round-trips against your
# claude install. Burns one ~15s claude turn.
cargo test --release -p nucleus-core --test session_smoke -- --ignored --nocapture

# End-to-end: DM your Discord bot. First reply ~5–6s (cold spawn), follow-ups
# in the same channel ~5s warm. While replying, in another terminal:
tmux attach -t nucleus-discord       # detach with Ctrl-b d
```

### 6. Perimeter — make the dashboard tailnet-private at its real hostname (ADR-011)

The whole operator surface goes behind Tailscale (no public path, news
included), while keeping `$NUCLEUS_PUBLIC_URL`'s real hostname and a valid
cert via Caddy. No login — tailnet membership is the access control.

```bash
# One-time bootstrap (interactive — browser auth + admin-console steps):
brew install tailscale && sudo tailscale up   # authenticate via printed URL
#   admin console: approve device, rename it `nucleus`, ENABLE HTTPS (DNS tab).
#   Install the Tailscale client on every device you want access from
#   (macOS/iOS/Windows `winget install tailscale`/Linux install.sh).

# Caddy terminates TLS for the real hostname (cert via ACME DNS-01), so put
# a Cloudflare token (Edit zone DNS, scoped to your zone) in .env:
#   CF_API_TOKEN=...
# Fetch a Caddy build with the cloudflare DNS module → ~/.local/bin/caddy
# (caddyserver.com/download, or xcaddy build --with github.com/caddy-dns/cloudflare).

./tools/caddy/install.sh          # generates Caddyfile + LaunchDaemon plist
sudo tailscale serve --https=443 off                      # free :443 for Caddy
sudo cp tools/caddy/<prefix>.caddy.plist /Library/LaunchDaemons/
sudo chown root:wheel /Library/LaunchDaemons/<prefix>.caddy.plist
sudo launchctl bootstrap system /Library/LaunchDaemons/<prefix>.caddy.plist

# Point the hostname's public DNS at the tailnet IP, DNS-only (grey cloud):
#   A  <nucleus-host>  →  $(tailscale ip -4)     proxied = false
# (do it in the CF dashboard, or via the API with the same token)

# Finally drop the now-dead nucleus route from ~/.cloudflared/config.yml
# (keep containers) and restart the tunnel.
```

After this, `$NUCLEUS_PUBLIC_URL` resolves to your tailnet IP and serves the
dashboard with a Let's Encrypt cert — reachable from any device on the
tailnet, and nowhere else (off-tailnet the name resolves to an unroutable
`100.x` and times out). Full rationale, the exact commands, rollback, and
the `tools/caddy/` setup are in
[`docs/ADR-011-perimeter-tailscale.md`](docs/ADR-011-perimeter-tailscale.md)
and [`tools/caddy/README.md`](tools/caddy/README.md).

## Configuration

Two files, **separated by what they contain**:

### `.env` (gitignored — identifiers + secrets)

Everything personally identifying lives here so no committed file ever
encodes who you are or what your accounts are.

```env
DISCORD_BOT_TOKEN=...
NUCLEUS_USER_NAME=YourName
NUCLEUS_WORKSPACE_ROOT=/absolute/path/to/nucleus
NUCLEUS_TIER2_DIR=/absolute/path/to/.claude/projects/<encoded-cwd>/memory
NUCLEUS_TZ=Region/City           # plist scheduling TZ; auto-detected if unset
DISCORD_HOME_CHANNEL_ID=...
DISCORD_ALLOWED_USER_IDS=...     # comma-separated, your Discord user ID(s)
WHATSAPP_ALLOWED_GROUP_NAMES=Alfred
WHATSAPP_BRAINDUMP_GROUP_NAMES="Brain Dump"
MEM0_USER_ID=you
```

Values with spaces must be quoted (`"Brain Dump"`) so the install.sh
shell sourcing handles them correctly. The whatsapp config also strips
matching surrounding quotes at parse time.

See `.env.example` for the full template and `docs/SECRETS.md` for the policy.

### `nucleus.toml` (commit-safe — non-identifying tunables)

Cron schedules, retention windows, ports, permission mode, the disallowed-
tools denylist, behavior toggles.

```toml
[claude]
binary = "claude"
permission_mode = "auto"
disallowed_tools = ["Bash(rm *)", "Bash(sudo *)", ...]

[obsidian]
vault_path = "~/Documents/Obsidian"

[news]
fetch_cron = "0 9,18 * * *"

[distiller]
cron = "0 4 * * *"          # one consolidated daily pass (ADR-016)

[skill_learner]
# ADR-017: on-the-fly review after N turns/chat + a periodic learn/curate pass.
nudge_interval = 12
stale_after_days = 30
archive_after_days = 90
enabled = true

[ports]
nucleus_dashboard = 8092
```

## Memory model

Four tiers, each with a clear lifetime:

| Tier | Where | Lifetime | Who reads |
|---|---|---|---|
| **T1** Session DBs | `memory/*.db` | Per-chat continuity | The owning bot only |
| **T1.5** Diaries | `memory/diaries/<agent>/YYYY-MM-DD.md` | 7-day rolling | Distiller |
| **T2** Shared facts | `$NUCLEUS_TIER2_DIR/*.md` | Auto-loaded into every `claude` session | All Claude spawns |
| **T3** Second brain | `~/Documents/Obsidian/{0-Inbox, 1-Main-Notes, 2-Daily-Notes, 3-Projects, 4-Areas, 5-Resources, 6-Slipbox, 7-Archives}/` | Forever | User browses; bots read via `--add-dir` and write via the multi-op brain-dump pipeline (see ADR-005) |
| **T4** mem0 vector | `tools/mem0/docker-compose.yaml` (kept idle) | Deferred indefinitely | mem0 needs embedding + LLM provider, neither covered by Claude Max — T3 covers the role |

Distillation pipeline (`chores/distiller`) runs as one daily 4am pass
(extract → judge, emitting PROMOTE / MERGE / ARCHIVE / DROP operations
against T2 / T3; consolidated from the old hourly + weekly split per
ADR-016).

Full design: `docs/ADR-002-memory.md` and `docs/ADR-004-diary-and-distillation.md`.

## Skills (procedural memory)

Where memory tiers store *facts*, **skills** store *procedures* —
"when X comes up, do Y." Skills use Claude Code's native `SKILL.md`
mechanism with Nucleus-specific frontmatter on top (see ADR-008).

| Path | Audience | Committed? |
|---|---|---|
| `.claude/skills/<name>/SKILL.md` | Skills for *working on* Nucleus (debug helpers, dev workflows) | Yes |
| `~/.claude/skills/<name>/SKILL.md` | Skills for the operator's own recurring flows (pre-meeting prep, weekly review, etc.) | No — operator-personal |

Skills run two ways:

1. **Manually**: invoke from any session with `/<skill-name>`.
2. **From a reminder**: pass `--system-prompt` (instead of `--body`) to
   `reminders add`. At fire time the worker spawns a one-shot
   interactive Claude session, sends the prompt as the first message,
   captures the reply, and forwards it to the configured channels.

The session sees every skill's description in its tool listing, so the
prompt can compose (`"Run skill-A, then skill-B, summarize both."`) or
go free-form (`"Read today's diary, post a one-line blockers summary."`).

### Authoring

The bot **should not author a skill in one shot.** The recommended flow
(per ADR-008):

1. **Explore** interactively in a regular Claude Code session — walk
   the surface together, hit failures, work around them.
2. **Capture** what worked: trigger, steps, observed failure modes.
3. **Formalize** with the [skill-creator plugin](https://claude.com/plugins/skill-creator).
   It's declared in this repo's `.claude/settings.json` under
   `enabledPlugins`, so Claude Code prompts you to install it the
   first time you `cd` into the repo and trust the folder — accept.
   If you ever land in a session without it, install manually with
   `/plugin install skill-creator@claude-plugins-official` then
   `/reload-plugins`. Invoke `/skill-creator create` and let it
   scaffold `SKILL.md`. Hand-edit the Nucleus-specific frontmatter
   additions (`flavor: recipe`, `mcp_needed`, `notify_on_failure`,
   `last_used` / `last_failure` / `failure_count_30d`).
4. **Test** from a fresh session — close the exploratory one, open a
   new one, invoke the skill or fire a one-shot reminder against it.
5. **Iterate** via `/skill-creator improve` after observing real fires.

### Sensitivity defaults

Skill bodies that name real tools, contacts, URLs, or recurring routines
**must** live in `~/.claude/skills/` (not the repo). Per `CLAUDE.md`
Rule 1, anything identifying belongs in `.env`-substituted strings or
operator-personal files. The `.claude/skills/` tree is for generic
dev/debug workflows that any contributor could use.

If you're unsure where a skill belongs, default to `~/.claude/skills/`.

## Folder layout

```
nucleus/
├── Cargo.toml              workspace manifest
├── nucleus.toml            non-identifying tunables (gitignored copy)
├── .env                    identifiers + secrets (gitignored)
├── agents.toml             agent registry — single source of truth (ADR-016)
├── core/                   shared Rust lib — claude wrapper, config, memory, diary, agents, runlog, …
├── messaging/
│   ├── discord/            Discord bot — bin `discord` (Rust, serenity)
│   ├── whatsapp/           WhatsApp bot (TS, Baileys, whisper.cpp)
│   └── gmail/              gmail-metabolism — inbox triage via JARVIS persona (ADR-007)
├── news/
│   └── fetcher/            launchd-driven RSS pull + scorer
├── nucleus-dashboard/      unified operator app (ADR-015) — axum API + React SPA;
│                           subsumes the old dashboard/, chat/, news/api/ crates
├── chores/
│   ├── distiller/          one daily distillation pass (ADR-016; was hourly+weekly)
│   ├── reminders/          ad-hoc reminders CLI + once-per-minute polling tick
│   └── skill-gap-learner/  autonomous skill learner — on-the-fly + periodic (ADR-017)
├── tools/
│   ├── launchd/            plist templates + install.sh
│   └── cloudflared/        tunnel config templates
├── docs/                   ADRs + roadmap + secrets policy
└── memory/                 runtime state (DBs, diaries, logs) — gitignored
```

## Docs index

- `docs/ADR-001-architecture.md` — workspace layout, stack, slice roadmap
- `docs/ADR-002-memory.md` — tier model + path conventions
- `docs/ADR-003-permissions.md` — Discord/WhatsApp bot security posture
- `docs/ADR-004-diary-and-distillation.md` — journal pattern, promotion ops
- `docs/ADR-005-second-brain.md` — T3 = PARA-organized Obsidian vault; multi-op brain-dump pipeline
- `docs/ADR-005a-braindump-review.md` — brain-dump review-before-apply (WhatsApp, in-band)
- `docs/ADR-005b-whatsapp-dm-mode.md` — WhatsApp DM mode: operator-only conversational channel
- `docs/ADR-006-reminders.md` — reminders as the universal time-triggered notification primitive
- `docs/ADR-007-gmail-calendar-via-mcp.md` — Gmail + Calendar via Claude.ai MCP (JARVIS persona)
- `docs/ADR-008-skills.md` — procedural memory via Claude Code's native `SKILL.md` mechanism + the reminders `system_prompt` extension
- `docs/ADR-009-persona-configurability.md` — venue→persona mapping via `NUCLEUS_PERSONA_<VENUE>`; venue names in code, persona names in config
- `docs/ADR-010-setup-wizard.md` — guided one-shot Nucleus install (proposed)
- `docs/ADR-011-perimeter-tailscale.md` — perimeter: Tailscale + Caddy, dashboard at its real hostname
- `docs/ADR-012-canvas.md` — agent-rendered interactive components in dashboard chat (proposed)
- `docs/ADR-013-vault-ingestion.md` — PDFs/Word/HTML → markdown in the PARA tree (deferred)
- `docs/ADR-014-obsidian-vault-customization.md` — vault appearance/config for read-mostly operator use
- `docs/ADR-015-nucleus-dashboard-unified-operator-app.md` — the single operator app + aesthetic guardrails
- `docs/ADR-016-agent-registry-and-log-capture.md` — `agents.toml` registry, run-log capture, `/agents` front door, distiller consolidation
- `docs/ADR-017-skill-gap-learner.md` — autonomous skill learner (on-the-fly review + periodic gap-detection/curator), the validation gate
- `docs/ADR-018-whatsapp-media.md` — WhatsApp media + personal document library, encrypted Drive (proposed)
- `docs/ADR-019-image-generation-surface.md` — local Bonsai image gen + dashboard gallery
- `docs/ADR-020-architecture-hardening.md` — hardening pass: session profiles, migrations, DB ownership rule, ops pruning/rotation, typegen — and the rejected alternatives
- `docs/ADR-021-agent-session-messaging.md` — `session-send`: the one sanctioned agent-to-agent session injection primitive (attributed, idle-gated, logged)
- `docs/ADR-022-concurrent-browser-automation.md` — Playwright MCP isolated contexts + shared storage state; `tools/playwright-auth/` owns logins
- `docs/ADR-023-session-search.md` — FTS5 transcript retrieval between T1 and T2, junk-session gate + pruning (`session-search` CLI)
- `docs/ADR-024-reminder-condition-watchers.md` — cheap script gates on reminder fires; model only on state change (`--condition`)
- `docs/ADR-025-pre-rotation-memory-flush.md` — persist-before-recycle DURABLE section in the rotation ask
- `docs/ADR-026-heartbeat.md` — HEARTBEAT.md checklist sweep + reply-gated silent fires
- `docs/ADR-027-adapter-circuit-breaker.md` — WhatsApp connection supervisor: close-reason taxonomy, backoff ladder, open-circuit alerts (proposed)
- `agents.toml` — the agent registry (single source of truth); add/remove an agent by editing it
- `docs/SECRETS.md` — env-vs-toml policy + pre-commit audit
- `CLAUDE.md` — workspace-level rules auto-loaded into every claude session

## Operating cheatsheet

```bash
# See every agent + its live state at a glance (the front door, ADR-016):
#   open http://127.0.0.1:8092/agents   — or:  curl …/agents/api/list | jq

# Tail any service's log
tail -f memory/discord.log
tail -f memory/whatsapp.log
tail -f memory/news-fetcher.log
tail -f memory/nucleus-dashboard.log

# Watch a bot think live (the tmux window claude is running in)
tmux attach -t nucleus-discord            # detach: Ctrl-b d
tmux attach -t nucleus-whatsapp
tmux attach -t nucleus-chat               # Obsidian chat service
# One-shot scheduled jobs only have a tmux session while they're running:
tmux attach -t nucleus-news-fetcher       # if a run is in flight
tmux attach -t nucleus-distiller          # daily distillation pass
tmux attach -t nucleus-skill-gap-learner  # skill review (on-the-fly) + daily learn/curate (ADR-017)
tmux attach -t nucleus-gmail              # gmail metabolism + calendar fires (was nucleus-jarvis)

# Reload a specific service after a code change
cargo build --release && ./tools/launchd/install.sh discord

# Stop everything
./tools/launchd/install.sh --uninstall

# Manually trigger a scheduled job (re-uses the launchd-configured environment)
launchctl kickstart -k gui/$(id -u)/${NUCLEUS_LAUNCHD_PREFIX:-dev.nucleus}.news-fetcher
launchctl kickstart -k gui/$(id -u)/${NUCLEUS_LAUNCHD_PREFIX:-dev.nucleus}.distiller       # daily pass (prunes diaries + writes memory/vault)
launchctl kickstart -k gui/$(id -u)/${NUCLEUS_LAUNCHD_PREFIX:-dev.nucleus}.gmail-metabolism

# Force a stuck per-chat session to respawn fresh (next message cold-spawns,
# but with --resume so prior turns are still there)
tmux kill-window -t nucleus-discord:<window-prefix>

# Ad-hoc reminders (the bots usually do this for you via natural language)
./target/release/reminders add \
  --at "2026-05-14T16:45:00<your-tz-offset>" \
  --body "dentist appointment" \
  --channels discord-home         # or whatsapp-dm | calendar

# Skill-fire reminder (ADR-008): spawns a one-shot Claude session at fire
# time, executes the prompt (possibly invoking a skill), forwards the
# reply to the channels. Use --system-prompt instead of --body.
./target/release/reminders add \
  --cron "20 8 * * 1-5" \
  --system-prompt "Run pre-meeting-prep skill, post results to discord-home." \
  --channels discord-home

./target/release/reminders list   # see pending

# Search past session transcripts (ADR-023; index refreshes on every run)
./target/release/session-search "what did we decide about X" --days 30
./target/release/session-search --prune            # junk-transcript report (dry-run)
./target/release/reminders cancel <id>

# One-shot WhatsApp send (uses the paired session)
cd messaging/whatsapp && npm run send -- <phone-or-jid> "<message>"

# Re-pair WhatsApp (e.g. moving to a new number)
rm -rf messaging/whatsapp/auth
launchctl bootout gui/$(id -u)/"${NUCLEUS_LAUNCHD_PREFIX:-dev.nucleus}".whatsapp
cd messaging/whatsapp && npm run discover
# scan QR, update .env if the group changed, then re-install via install.sh
```

## License

MIT — see [`LICENSE`](./LICENSE). Fork freely, contribute back if it makes sense.
