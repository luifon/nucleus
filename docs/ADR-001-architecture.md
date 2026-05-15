# ADR-001 — Architecture & Workspace Layout

**Status:** Accepted (2026-05-12)

## Context

Nucleus replaces Hermes Agent as the brain behind the user's personal Discord/WhatsApp bots, news aggregator, and local-services dashboard. Hermes did the discovery work — Discord gateway, mem0 stack, persona file pattern, daily-news cron — but routed every message through DeepSeek via OpenRouter (separate billing, mediocre output). Nucleus puts `claude` in the brain seat instead, leveraging the existing Claude Max subscription.

The brain runs as long-lived interactive `claude` sessions hosted in tmux windows (one per chat for the messaging surfaces, one per scheduled job). Headless `-p` mode is moving to API-only billing, so the entire stack went through a one-time migration to tmux-hosted sessions — see `core::claude_session::Session` / `SessionPool`. Live tmux sessions are attachable for debugging (`tmux attach -t nucleus-discord`).

Hermes stays installed-but-dormant as a reference implementation; we lift patterns (Discord, voice transcription, future Signal) as we need them.

## Workspace

Single Cargo workspace at `~/Development/nucleus/`. Crates:

| Crate | Kind | Responsibility |
|-------|------|---------------|
| `core` | lib | `claude_session::{Session, SessionPool}`, `claude::PermissionMode`, `config::Settings`, `db`, `health`, `memory`, `diary`, `discord_sdk` — shared by every binary |
| `messaging/discord` (`discord`) | bin | Discord bot — persona "Jerry Lewis" by default. Per-channel `SessionPool`, replies via the resumed session for that channel. |
| `messaging/whatsapp` | TS bin | WhatsApp bot (Alfred persona) on Baileys. Voice memos transcribed locally via whisper.cpp, then routed through a TS port of `SessionPool`. |
| `news/fetcher` (`news-fetcher`) | bin | launchd-driven, twice-daily fetch + score (one-shot Session), writes SQLite, posts to Discord |
| `news/api` (`news-api`) | bin | axum HTTP server for the news feed UI + upvote/downvote endpoints, served at the URL in `NUCLEUS_NEWS_PUBLIC_URL` |
| `dashboard` (`dashboard`) | bin | axum, health collectors for Docker / launchd / tunnels / news job, plus `/obsidian` chat (its own per-chat `SessionPool` over the vault). Served at the URL in `NUCLEUS_DASHBOARD_PUBLIC_URL` |
| `chores/distiller` (`distiller`) | bin | Hourly metabolism + weekly contemplation passes (one Session reused across agents). |
| `chores/preference-learner` (`preference-learner`) | bin | Weekly: read news votes, ask Claude to derive preferences, write `news_preferences.md`. |
| `chores/reminders` (`timesheet-reminder`) | bin | Daily timesheet nudge to Discord (extensible to other reminders). |

## Stack

- **Rust** primary, **JS/TS** only when Rust is genuinely the wrong tool.
- async runtime: `tokio`
- HTTP server: `axum`
- Discord: `serenity`
- DB: `sqlx` (SQLite, compile-time-checked queries)
- HTTP client: `reqwest` (rustls)
- Docker API: `bollard`
- Logging: `tracing` + `tracing-subscriber`
- Config: `figment` (TOML + env)

## Tunnels

Two Cloudflare tunnel ingress routes are expected:

- `$NUCLEUS_NEWS_PUBLIC_URL` → `localhost:<ports.news_api>` (default 8080).
- `$NUCLEUS_DASHBOARD_PUBLIC_URL` → `localhost:<ports.dashboard>` (default 8090).

Both URLs are env-sourced and optional — leave a `NUCLEUS_*_PUBLIC_URL`
unset and the dashboard skips that tunnel's health check while hiding any
cross-link to it in the UI. Configs live in `tools/cloudflared/` (gitignored,
templates checked in).

## Slice roadmap

| Slice | Deliverable | Status |
|-------|-------------|--------|
| **S1** | Jerry Lewis Discord bot operational. Hermes gateway stopped. launchd plist installed. | ✅ shipped |
| **S2** | News fetcher + news API + Discord notification. Day-partitioned SQLite, upvote/downvote endpoints, twice-daily launchd job. | ✅ shipped |
| **S3** | Dashboard with health collectors (Docker, news job, tunnels) + `/obsidian` chat with persistent multi-chat history. | ✅ shipped |
| **S4** | mem0 wired as MCP server for Tier 4 vector recall. | ❌ deferred indefinitely (mem0 needs an embedding + LLM provider; neither is covered by Claude Max. T3 = PARA-Obsidian replaces this role — see ADR-002.) |
| **S5** | Preference learning loop — weekly job reads news upvote table, updates `news_preferences.md`. | ✅ shipped |
| **S6** | WhatsApp bot (Alfred): Baileys session, allowlist scoping, whisper.cpp voice transcription, brain-dump router. | ✅ shipped |
| **S7** | Tmux-hosted long-lived Sessions across every surface, replacing `claude -p`. | ✅ shipped |
| **S8** | T3 redesign as PARA-organized Obsidian second brain. Distiller contemplation re-routes weekly digests into PARA buckets with sibling linking. WhatsApp gets a separate brain-dump channel that classifies captures into PARA. Classification-escalation state machine for low-confidence cases. See ADR-005. | ✅ shipped |
| **S9** | Ad-hoc reminders. `chores/reminders` extended with `add` / `list` / `cancel` / `due` subcommands backed by `memory/reminders.db`. Once-per-minute `launchctl` tick polls for due rows and delivers to Discord. Bots invoke `reminders add --at <ISO> --body <text>` via Bash when the user asks to be reminded. V1 is Discord-only; WhatsApp delivery awaits a fix for the Baileys single-client constraint. See CLAUDE.md Rule 10. | ✅ shipped |

## Hermes status

Dormant-but-installed at `~/.hermes/`. Gateway service was stopped when S1 shipped. Don't uninstall — we keep it as a pattern library (Discord gateway flow, mem0 docker compose, voice transcription wiring) for whatever surface lands next (Signal, etc.).
