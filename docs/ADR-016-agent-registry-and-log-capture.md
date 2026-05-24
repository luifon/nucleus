# ADR-016 — Agent registry + consolidation

**Status:** Accepted (2026-05-23) — Implemented (2026-05-24)

**Drives change in:**
- [[ADR-001]] — the per-crate topology stays, but every agent now also has a
  declarative registry entry.
- [[ADR-004]] — diaries continue as the summary surface; this ADR adds the
  raw-output complement (run-log) and **consolidates the distiller** from two
  jobs (hourly + weekly) into one daily pass. The "SOUL slot" persona
  auto-evolution is deferred out of the distiller to the future learner.
- [[ADR-009]] — completes that ADR's deferred "related cleanups": the
  `nucleus-jarvis` tmux identifier and the persisted "by Jerry" footer.
- [[ADR-015]] — the dashboard's `/sessions` surface is **deleted**; `/agents`
  is the front door, and the `/` landing gains the agents-health tile that
  shipped deliberately empty pending this work.

## Context

Surfacing "what is each agent doing, and where is its output?" in the
nucleus-dashboard exposed a real gap: **agents ran in inconsistent ways with
no central definition of what an agent is**, and several overlapping or
deprecated mechanisms had accumulated. What started as "registry + log
capture" became an **agent consolidation**.

A pre-implementation survey corrected several premises in the original draft:

- **launchd log capture was already done.** Every plist already writes
  `StandardOutPath`/`StandardErrorPath` to `memory/<name>.log` — not the
  `~/Library/Logs/dev.nucleus.*` the draft assumed. The registry just points
  at the existing files.
- **tmux+claude raw output was never lost — only un-indexed.** A session's
  transcript at `~/.claude/projects/<cwd>/<session-id>.jsonl` **survives the
  window being killed** (`claude_session.rs`). The fix is to *index* it, not
  to `tee` the TUI (ANSI redraw garbage) or copy transcripts (OpenClaw's
  landfill footgun, [[ADR-004]]).
- **Runtime "kind" is two axes.** `news-fetcher` / `distiller` /
  `gmail-metabolism` are launchd-cron-*triggered* but *execute* in tmux+claude.
- **Everything is an agent.** The differences (daemon vs cron vs pool vs fire)
  are *how it's launched* and *how you probe liveness*, not different kinds of
  thing. So the registry is one uniform record, not a tagged union.

## Decision

### 1. Agent registry — `agents.toml`

A single committed file at the workspace root, loaded by
`nucleus_core::agents::Registry`. Committed (not gitignored like
`nucleus.toml`) because it's canonical system topology, identical for every
clone, with no identifiers — venue names, relative paths, `dev.nucleus.*`
labels only (Rule 1 / Rule 7). Hand-edited; there's no daemon maintaining it.

One **uniform `Agent` record** with descriptive attributes (required:
`name`, `class`, `launch`; the rest optional and resolved per attribute):

| field | meaning |
|---|---|
| `name` | venue-based identity (Rule 7); usually matches `diary_key` |
| `class` | descriptive grouping — `conversational` / `scheduled` / `maintenance` / `infra` / `ephemeral` (not a schema discriminator) |
| `launch` | how it starts / how liveness is probed — `launchd-daemon` / `launchd-cron` / `in-process` / `on-demand` |
| `runtime` | `rust` / `node` (display) |
| `launchd_label` | liveness probe key for launchd jobs |
| `tmux_session` | present ⇒ drives Claude ⇒ has a run-log index |
| `schedule` | informational (truth is the plist) |
| `log_path` | launchd stdout/err file |
| `diary_key` | `memory/diaries/<diary_key>/` |
| `persona_venue` | conversational agents — `resolve_persona()` → display_name (ADR-009) |
| `capabilities` | in-agent behaviors — `rotates` now, `skill_review` future |
| `enabled` | future-reserved agents ship `false` |

The registry is the source of truth for `/agents` and any future surface.
Individual executions — per-chat-key sessions, per-contract skill-fires,
calendar fires — are **not** entries; they're discovered at runtime from the
session DBs and the run-log index.

### 2. Unified log / output capture

- **launchd** (daemon + cron): already writes `memory/<name>.log`; the
  registry's `log_path` points at it. Zero plist churn.
- **tmux + claude**: `nucleus_core::runlog` (+ a TS mirror for whatsapp).
  Each spawn appends an in-flight row to `memory/logs/<agent>/runs.jsonl`
  — `{run_id, agent, session_id, transcript_path, tmux_target, started_at}` —
  and `close()` finalizes `ended_at`/`ok`. The transcript is read **in place**
  from `~/.claude/projects`; we never copy it. A row whose transcript is gone
  is gc'd on the next close, capped to the last 50 per agent — no separate
  cleanup chore. `SpawnOptions`/`PoolConfig` carry an `agent_label` that is
  threaded through every spawn site.

`ok` means "closed cleanly". A run that dies without `close()` leaves
`ended_at` null = "ran, outcome unknown". Error-*tracking* is out of scope:
for scheduled agents the launchd exit code carries it; for fires the diary +
`⚠️` alert do.

### 3. Two layers: capabilities vs agents

- **Layer A — capabilities inside fixed agents.** Session rotation (the 4am
  summarize→diary→respawn loop) and the future on-the-fly skill review live
  *inside* the conversational agents as `capabilities`. Rotation **feeds** the
  diary; it is not a separate agent. (The 4am timer is kept; replacing it with
  Hermes-style on-demand compaction at context-pressure is future work.)
- **Layer B — maintenance agents.** The distiller (now) and the skill-gap
  learner (future) are standalone periodic registry entries that **read**
  diaries and emit durable artifacts.

### 4. Consolidations + cleanups shipped under this ADR

- **Distiller → one daily pass.** Hourly metabolism + weekly contemplation
  collapse into a single daily `distiller` run (extract → promote-to-T2 →
  archive-to-vault → prune). Two plists become one (`dev.nucleus.distiller`,
  04:00). `[distiller]` config simplified to `{cron, model}` with serde
  defaults so a pre-ADR-016 `nucleus.toml` still loads.
- **preference-learner sunset.** Crate, plist, and workspace member removed.
  Its remit is superseded by the future skill-gap learner, whose slot is
  reserved in the registry (`enabled = false`).
- **`/sessions` deleted**, fully superseded by `/agents` (the copy-attach
  affordance moved onto agent tiles).
- **ADR-009 follow-ups completed.** `nucleus-jarvis` → `nucleus-gmail`;
  `/remember` footer signs with the resolved persona `display_name`.
- **Dead cruft removed.** `dashboard.db` + its never-firing migration; the
  stale `obsidian-chat` diary directory.

### 5. `/agents` surface

The operator front door. Per registry agent it computes liveness from the
runtime its `launch` implies (daemon = PID; cron = last exit, 0 idle /
nonzero errored; in-process = hosted; on-demand = live tmux window), resolves
the persona `display_name`, and exposes the run-log. Tiles group by `class`;
expanding shows the run-log (transcript pointers) for tmux agents or the
launchd log tail for launchd agents. `/cron` keeps its launchctl-eye-view;
`/diary` keeps its own discovery; the `/` landing summarizes agents by status.

## Future work

- **skill-gap learner** — generic, all-facets successor to preference-learner
  (Hermes-style): a periodic diary/transcript-driven pass **and** an
  on-the-fly per-session arm (the `skill_review` capability). The run-log
  built here is its substrate. Inherits the deferred persona-evolution as
  reviewable suggestions, never silent writes to `personas/<slug>.md`.
- **Rotation → on-demand compaction** at context-pressure (+ parent-session
  chaining), replacing the blind 4am timer.
- Diary FTS search + skill usage counters / pinning (Hermes patterns) if needed.

## Out of scope

- Metrics / observability beyond log capture (latency, per-agent error rates).
- Process supervision changes — launchd + tmux remain the runners; the
  registry is a layer above them.
- A daemon maintaining the registry — operator hand-edits (config is files).
