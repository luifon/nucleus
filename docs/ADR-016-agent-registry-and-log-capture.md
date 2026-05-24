# ADR-016 — Agent registry + unified log capture

**Status:** Proposed (2026-05-23)

**Drives change in:**
- [[ADR-001]] — the per-crate topology stays, but each agent gains a
  declarative registry entry it doesn't have today.
- [[ADR-004]] — diaries continue as the post-hoc summary surface;
  this ADR adds the raw-output complement.
- [[ADR-015]] — the dashboard's `/sessions` surface gets de-scoped to
  honest minimum; an `/agents` surface (or a re-scoped `/sessions`)
  becomes the front door once the registry exists.

## Context

Surfacing "what is each agent doing, and where is its output?" in
the nucleus-dashboard exposed a real architectural gap: **agents run
in three inconsistent ways and there is no central definition of
what an agent is**.

The three runtime shapes today:

| Kind | Examples | Live state | Raw output | Summary |
|---|---|---|---|---|
| **launchd-daemon** (long-running binary) | `whatsapp`, `discord`, `dashboard` | `launchctl list` → PID present | launchd captures stdout/stderr to `~/Library/Logs/dev.nucleus.<label>/*.log` | per-agent diary (ADR-004) |
| **launchd-cron** (one-shot binary, scheduled) | `news-fetcher`, `distiller`, `gmail-metabolism`, `preference-learner`, `reminders-tick` | `launchctl list` → exit code from last run | same launchd log file | diary |
| **tmux + claude** (skill-fires via reminders-fire; per-chat-key SessionPool windows used by chat / discord) | `nucleus-reminders-fire`, `nucleus-jarvis`, etc. | `tmux list-windows -t <session>` → live windows; the default `zsh` window means "idle" | **none persisted** — `claude_session.rs:149` kills the window on exit, so scrollback is lost when the fire completes | diary (after the fact) |

Three concrete consequences:

1. **No single source of truth** for "the set of agents." The
   dashboard's `/cron`, `/sessions`, and `/diary` each see a
   different overlapping subset. Diary lists `whatsapp` and
   `gmail-metabolism`; sessions doesn't (they run as launchd
   binaries, not in tmux); cron lists `nucleus-news-fetcher`
   launchd plist but the operator can't tell from there that the
   actual claude work happens in a tmux window that gets killed
   immediately after.

2. **Raw output for tmux-hosted work is unrecoverable.** When a
   skill-fire's window is killed on close, its full pane history
   goes with it. The diary captures a summary, not the raw
   transcript. If the fire silently produced a wrong result, the
   operator can't reconstruct what claude actually saw or did.

3. **`/sessions` surface is structurally broken.** The pane preview
   it builds is always reading the default `zsh` window (because
   all the interesting windows have been killed by the time the
   operator looks), so the preview is empty 99% of the time. The
   short-term mitigation is to strip the preview (see "Short-term
   mitigation" below); the proper fix needs this ADR.

## Decision

Two coupled mechanisms:

### 1. Agent registry

A declarative list of every operator-facing agent in a single place.
Probably `nucleus.toml` under `[[agents]]` (consistent with the rest
of the config-as-files discipline; alternatively a separate
`agents.toml` if the file grows). Schema:

```toml
[[agents]]
name = "whatsapp"
kind = "launchd-daemon"                  # daemon | cron | skill-fire | chat-pool
launchd_label = "dev.nucleus.whatsapp"
log_path = "memory/logs/whatsapp/current.log"
diary_key = "whatsapp"

[[agents]]
name = "news-fetcher"
kind = "launchd-cron"
launchd_label = "dev.nucleus.news-fetcher"
schedule = "0 9 * * *"
log_path = "memory/logs/news-fetcher/{run_id}.log"
diary_key = "news-fetcher"

[[agents]]
name = "dsu-prep-${SHORTCODE}"
kind = "skill-fire"
tmux_session = "nucleus-reminders-fire"
skill = "dsu-prep"
skill_args = ["${SHORTCODE}"]
log_path = "memory/logs/skill-fires/dsu-prep-${SHORTCODE}/{run_id}.log"
diary_key = "reminders"

[[agents]]
name = "chat-q"
kind = "chat-pool"
tmux_session = "nucleus-chat"
log_path = "memory/logs/chat/{chat_key}/{session_id}.log"
diary_key = "chat"
```

The registry is the **single source of truth** for `/cron`,
`/sessions` (or its replacement), `/diary`, and any future surface.
Adding an agent means adding a registry entry; removing one means
deleting it. No more implicit lists derived from filesystem walks
or `launchctl list | grep`.

### 2. Unified log capture

Every agent execution writes its raw stdout/stderr to a known path
(`log_path` in the registry). Three implementation paths matching
the three runtime kinds:

- **launchd** (daemon + cron): set `StandardOutPath` /
  `StandardErrorPath` in each plist to `log_path` (templated
  through `tools/launchd/install.sh`'s existing substitution
  pipeline). Adds nothing the system isn't already doing —
  launchd already writes to `~/Library/Logs/dev.nucleus.*`; this
  just moves the file to a path Nucleus controls and the
  dashboard can read.

- **tmux + claude** (skill-fire + chat-pool): wrap the inner
  `claude` invocation with `tee` to `log_path` so the raw stdin/
  stdout/stderr of the claude process is captured to a file
  regardless of whether the window survives. The same content
  that's in the pane *while* the window is alive remains on disk
  after the window is killed.

Logs rotate on size (per file ~ 10 MB; keep N most recent runs
per agent). Old logs gc'd by a small daily chore (could fold into
`distiller` if that survives — see [[distillers_killable]]).

### 3. Dashboard surfaces re-scoped on top of the registry

- **`/agents`** (new, or `/sessions` renamed) — primary front
  door. Tile per registry entry. Per tile: name, kind badge,
  liveness (PID / tmux window / "idle"), last run timestamp,
  inline "view live" / "view last log" / "view diary" actions.
- **`/cron`** — keeps the launchd-eye-view (`launchctl list`)
  for ops-style "what does the system think is loaded right now",
  but the operator-facing path is `/agents`.
- **`/diary`** — unchanged. Diaries remain the **summary** layer;
  logs are the **raw** layer.
- **`/sessions`** (current state) — collapses into `/agents` or
  becomes a thin wrapper showing only tmux-specific affordances
  (attach commands for currently-live tmux windows).

## Short-term mitigation (this commit, not future)

While ADR-016 is being designed and implemented:

- `/sessions` in nucleus-dashboard is **stripped to honest
  minimum** — tile per `nucleus-*` tmux session with: liveness,
  uptime, idle, window count, copy-attach button, link to the
  matching diary. **No pane preview** (it was always empty
  outside the brief mid-fire window). The `subtitle` on the page
  explains the gap and points at this ADR.
- The pane-preview backend endpoint (`/sessions/api/capture`)
  and frontend code are removed in the same change.

## Open questions

1. **Registry file location** — `nucleus.toml` keeps everything in
   one place but `[[agents]]` could grow large (1 entry per skill
   fire). A separate `agents.toml` (or `agents/<name>.toml` per
   agent) might scale better. Resolve at implementation kickoff.

2. **Per-chat-key agents** — chat-pool agents have one tmux window
   per `chat_key`; modelling them in the registry as a single
   agent or as one-per-chat-key is a real choice. Probably one
   registry entry per pool (the pool *is* the agent) and the
   dashboard surfaces individual chats by inspecting tmux + the
   chat.db for that pool.

3. **Log retention** — file-based rotation is simple; do we ever
   need queryable log structure (e.g. `loki`-style)? Probably
   not at single-operator scale — `grep` is fine. Revisit if
   the answer changes.

4. **Migration path** — registry can roll out incrementally: define
   the schema, populate one entry, verify it powers `/agents`
   correctly, then add the rest. No big-bang.

## Out of scope (ADR-016 explicitly does not solve)

- Metrics / observability beyond log capture (latency, throughput,
  per-agent error rates) — separate ADR if/when needed.
- Process supervision changes — keep launchd + tmux as the
  underlying runners; the registry is a layer *above* them, not a
  replacement.
- A daemon process that maintains the registry — operator hand-edits
  for now (same discipline as ADR-015 — config is files, not UI).
