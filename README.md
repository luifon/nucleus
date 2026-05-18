# Nucleus

A personal-assistant stack that wires the Claude Code CLI into your Discord,
WhatsApp, news, and dashboard surfaces.

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
| **Dashboard** | Live health of all local services + container drill-down (`docker top`, ports, logs). At `dashboard.<your-domain>`. |
| **Obsidian chat** | Persistent multi-chat against your PARA vault. Standalone service behind its own tunnel at `chat.<your-domain>`. |
| **Distiller** | Hourly + weekly passes that promote diary observations to long-term memory (PROMOTE / MERGE / ARCHIVE / DROP, Mem0-style ops). |
| **Preference learner** | Weekly pass that reads news votes and rewrites a preferences file the next news fetch reads back. |
| **Reminders** | Ask either bot "remind me at 16:45 about dentist" → Claude schedules via the `reminders` CLI. Once-per-minute polling delivers to one or more channels (Discord home, Alfred / Brain Dump WhatsApp groups via the Alfred-drained outbound queue). Supports `--at` (one-shot) and `--cron` (recurring) with pause/resume + per-channel retry; daily timesheet nudge is seeded as a recurring system reminder. |

## Architecture at a glance

```
              ┌────────────────────────────────────────────────────────┐
              │  Cloudflare tunnels (existing or new)                  │
              │  news.<dom>   dashboard.<dom>   chat.<dom>              │
              └─────────┬─────────────┬──────────────┬──────────────────┘
                        │             │              │
              ┌─────────▼──┐    ┌─────▼─────┐   ┌────▼─────┐
   Discord ←─ │  news-api  │    │ dashboard │   │   chat   │  ←── Obsidian vault
              │ axum :8080 │    │axum :8090 │   │axum :8091│
              └────┬───────┘    └────┬──────┘   └──────────┘
                   │ SQLite           │ collectors
   WhatsApp ←─┐    │  ↑               │ (bollard, http,
              │ ┌──▼──┴─────────┐     │  launchctl)
              │ │ news-fetcher  │ ← claude session
              │ │ (launchd 1x)  │   (one-shot)
   discord    │ └───────────────┘
   (Rust,     │
   serenity)  │ ┌─────────────────────┐
              │ │ distiller-hourly    │ ← claude
              │ │ distiller-weekly    │   sessions
              │ │ preference-learner  │   (one-shot)
              │ │ reminders-tick      │ ← every 60s
              │ └─────────────────────┘
              │
   whatsapp ──┘  ┌──────────────────────────────────────────────┐
   (TS,          │  nucleus-core (Rust lib)                     │
   Baileys,      │  - claude_session::{Session, SessionPool}    │
   whisper.cpp)  │       (tmux-hosted long-lived claude — only  │
                 │        path to the brain)                    │
                 │  - memory (Tier 2 promote/read)              │
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
- **A Discord application + bot** — token in hand. Required intents:
  `Guilds`, `Guild Messages`, `Direct Messages`, `Message Content`.
- **A Cloudflare tunnel** (optional, but the news + dashboard + chat front-
  ends expect to be reachable via subdomains; you can also just hit
  `localhost:8080 / :8090 / :8091` if you don't care about remote access).
- **An Obsidian vault, PARA-organized** — required for brain-dump capture
  and the Obsidian chat service. The default vault path is
  `~/Documents/Obsidian/` (override in `nucleus.toml` under
  `[obsidian].vault_path`). The vault MUST have these five top-level
  folders, each containing a `README.md` describing what belongs there
  (the bots read these READMEs to classify captures):
  ```
  ~/Documents/Obsidian/
  ├── 0-Inbox/README.md       capture-now-organize-later landing pad
  ├── 1-Projects/README.md    short-term efforts with a deadline + outcome
  ├── 2-Areas/README.md       ongoing responsibilities to maintain
  ├── 3-Resources/README.md   reference material on topics of interest
  └── 4-Archives/README.md    inactive items from the other three
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
```

### 3. Cloudflare tunnel (skip if running localhost-only)

```bash
# If you already have a tunnel, just add ingress routes; otherwise:
cloudflared tunnel create my-tunnel
cloudflared tunnel route dns my-tunnel news.<yourdomain>
cloudflared tunnel route dns my-tunnel dashboard.<yourdomain>
cloudflared tunnel route dns my-tunnel chat.<yourdomain>

# Generate per-service yamls from the templates — substitutes placeholders
# from .env (hostname, UUID, $HOME). Each yaml routes ONE hostname.
TUNNEL_UUID=<tunnel-uuid> ./tools/cloudflared/install.sh

# Run the tunnel. Two patterns work:
#  (a) one cloudflared process per yaml — simplest, but uses more sockets:
cloudflared service install --config "$PWD/tools/cloudflared/news.yaml"
#  (b) one combined ~/.cloudflared/config.yml whose `ingress:` block
#      multiplexes news / dashboard / chat onto a single tunnel (lighter,
#      preferred). Copy the ingress entries from the generated yamls into
#      one config and `cloudflared service install` once.
```

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
# to ~/Library/LaunchAgents/, loads via launchctl. 10 services total
# (discord, alfred, news-api, news-fetcher, dashboard, chat,
# distiller-hourly, distiller-weekly, preference-learner,
# reminders-tick).
```

Verify:
```bash
launchctl list | grep "${NUCLEUS_LAUNCHD_PREFIX:-dev.nucleus}"
curl http://127.0.0.1:8090/api/services | jq

# Optional: prove the Session machinery actually round-trips against your
# claude install. Burns one ~15s claude turn.
cargo test --release -p nucleus-core --test session_smoke -- --ignored --nocapture

# End-to-end: DM your Discord bot. First reply ~5–6s (cold spawn), follow-ups
# in the same channel ~5s warm. While replying, in another terminal:
tmux attach -t nucleus-discord       # detach with Ctrl-b d
```

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
metabolism_cron = "0 * * * *"
contemplation_cron = "0 4 * * 0"

[ports]
news_api = 8080
dashboard = 8090
```

## Memory model

Four tiers, each with a clear lifetime:

| Tier | Where | Lifetime | Who reads |
|---|---|---|---|
| **T1** Session DBs | `memory/*.db` | Per-chat continuity | The owning bot only |
| **T1.5** Diaries | `memory/diaries/<agent>/YYYY-MM-DD.md` | 7-day rolling | Distiller |
| **T2** Shared facts | `$NUCLEUS_TIER2_DIR/*.md` | Auto-loaded into every `claude` session | All Claude spawns |
| **T3** Second brain | `~/Documents/Obsidian/{0-Inbox, 1-Projects, 2-Areas, 3-Resources, 4-Archives}/` | Forever | User browses; bots read via `--add-dir` and write via the multi-op brain-dump pipeline (see ADR-005) |
| **T4** mem0 vector | `tools/mem0/docker-compose.yaml` (kept idle) | Deferred indefinitely | mem0 needs embedding + LLM provider, neither covered by Claude Max — T3 covers the role |

Distillation pipeline (`chores/distiller`) runs hourly (cheap extraction
into a `_pending.md` queue) and weekly (heavy judge that emits PROMOTE /
MERGE / ARCHIVE / DROP operations against T2 / T3).

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
├── core/                   shared Rust lib — claude wrapper, config, memory, diary, …
├── messaging/
│   ├── discord/            Discord bot — bin `discord` (Rust, serenity)
│   └── whatsapp/           Alfred (TS, Baileys, whisper.cpp)
├── news/
│   ├── fetcher/            launchd-driven RSS pull + scorer
│   └── api/                axum, day-bucketed feed + votes
├── dashboard/              axum, health collectors + container drill-down
├── chat/                   axum, Obsidian chat against the PARA vault
├── chores/
│   ├── distiller/          metabolism + contemplation passes
│   ├── preference-learner/ weekly votes → preferences file
│   └── reminders/          ad-hoc reminders CLI + once-per-minute polling tick
├── tools/
│   ├── launchd/            plist templates + install.sh
│   └── cloudflared/        tunnel config templates
├── assets/icons/           favicons / page logos (RSS arcs + pulse line)
├── docs/                   ADRs + roadmap + secrets policy
└── memory/                 runtime state (DBs, diaries, logs) — gitignored
```

## Docs index

- `docs/ADR-001-architecture.md` — workspace layout, stack, slice roadmap
- `docs/ADR-002-memory.md` — tier model + path conventions
- `docs/ADR-003-permissions.md` — Discord/WhatsApp bot security posture
- `docs/ADR-004-diary-and-distillation.md` — journal pattern, promotion ops
- `docs/ADR-005-second-brain.md` — T3 = PARA-organized Obsidian vault; multi-op brain-dump pipeline
- `docs/ADR-008-skills.md` — procedural memory via Claude Code's native `SKILL.md` mechanism + the reminders `system_prompt` extension
- `docs/SECRETS.md` — env-vs-toml policy + pre-commit audit
- `CLAUDE.md` — workspace-level rules auto-loaded into every claude session

## Operating cheatsheet

```bash
# Tail any service's log
tail -f memory/discord.log
tail -f memory/alfred.log
tail -f memory/news-fetcher.log
tail -f memory/dashboard.log

# Watch a bot think live (the tmux window claude is running in)
tmux attach -t nucleus-discord            # detach: Ctrl-b d
tmux attach -t nucleus-whatsapp
tmux attach -t nucleus-chat               # Obsidian chat service
# One-shot scheduled jobs only have a tmux session while they're running:
tmux attach -t nucleus-news-fetcher       # if a run is in flight
tmux attach -t nucleus-distiller          # hourly + weekly passes
tmux attach -t nucleus-preference-learner

# Reload a specific service after a code change
cargo build --release && ./tools/launchd/install.sh discord

# Stop everything
./tools/launchd/install.sh --uninstall

# Manually trigger a scheduled job (re-uses the launchd-configured environment)
launchctl kickstart -k gui/$(id -u)/${NUCLEUS_LAUNCHD_PREFIX:-dev.nucleus}.news-fetcher
launchctl kickstart -k gui/$(id -u)/${NUCLEUS_LAUNCHD_PREFIX:-dev.nucleus}.distiller-hourly
launchctl kickstart -k gui/$(id -u)/${NUCLEUS_LAUNCHD_PREFIX:-dev.nucleus}.preference-learner

# Force a stuck per-chat session to respawn fresh (next message cold-spawns,
# but with --resume so prior turns are still there)
tmux kill-window -t nucleus-discord:<window-prefix>

# Ad-hoc reminders (the bots usually do this for you via natural language)
./target/release/reminders add \
  --at "2026-05-14T16:45:00<your-tz-offset>" \
  --body "dentist appointment" \
  --channels discord-home         # or alfred | braindump | calendar

# Skill-fire reminder (ADR-008): spawns a one-shot Claude session at fire
# time, executes the prompt (possibly invoking a skill), forwards the
# reply to the channels. Use --system-prompt instead of --body.
./target/release/reminders add \
  --cron "20 8 * * 1-5" \
  --system-prompt "Run pre-meeting-prep skill, post results to discord-home." \
  --channels discord-home

./target/release/reminders list   # see pending
./target/release/reminders cancel <id>

# One-shot WhatsApp send (uses the paired session)
cd messaging/whatsapp && npm run send -- <phone-or-jid> "<message>"

# Re-pair WhatsApp (e.g. moving to a new number)
rm -rf messaging/whatsapp/auth
launchctl unload ~/Library/LaunchAgents/"${NUCLEUS_LAUNCHD_PREFIX:-dev.nucleus}".alfred.plist
cd messaging/whatsapp && npm run discover
# scan QR, update .env if the group changed, then re-install via install.sh
```

## License

MIT — see [`LICENSE`](./LICENSE). Fork freely, contribute back if it makes sense.
