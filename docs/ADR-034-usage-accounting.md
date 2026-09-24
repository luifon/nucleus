# ADR-034 — Usage accounting: tokens and estimated cost per project, model, agent and reminder

**Status:** Accepted (2026-09-24) — Implemented (2026-09-24)

**Builds on / touches:**
- [[ADR-015]] — the dashboard gains a `/usage` surface in the observability group.
- [[ADR-016]] — agent labels come from the run-log (`memory/logs/<agent>/runs.jsonl`); the registry's `launchd-cron` agents are the scheduled jobs on the recurring-cost table.
- [[ADR-020]] — `memory/usage.db` follows the rules: versioned migrations, one writer, read-only readers, ts-rs wire types.
- [[ADR-023]] — `session_index.db` is a label source; its long retention recovers labels that rotated out of the run-log.
- [[ADR-006]] / [[ADR-008]] — reminder skill-fires are attributed to their reminder through `reminder_fires.msg_id`.

## Context

The operator runs Claude Code and Codex on subscriptions, in many repositories, and Nucleus spawns its own Claude sessions for bots, chores and reminder fires. Before this ADR nothing answered "how much does each project, agent, reminder and model consume, and when?" The data exists on disk:

- **Claude Code** writes one JSONL transcript per session under `~/.claude/projects/<encoded-cwd>/`, one per subagent under `<session>/subagents/`, and deletes transcripts after about 30 days (`cleanupPeriodDays`).
- **Codex** writes one JSONL log per thread under `~/.codex/sessions/YYYY/MM/DD/` and keeps them.

Measured on the operator's machine at implementation: 2,862 files, 9.2 GB (Claude 2.2 GB in 1,133 main and 545 subagent files; Codex 6.5 GB in 1,195 files).

## Decision

### 1. Store and ownership

`memory/usage.db`, owned by core (`nucleus_core::usage`). The only writer is `usage::refresh`, reached from two places:

- `nucleus usage refresh` — the dashboard's refresh button spawns this command as a child process (the dashboard itself never writes the DB);
- the distiller's daily pass, after the session-index maintenance.

A lock file (`memory/usage-refresh.lock`, touched after every file, stale after 5 minutes) serializes the two. A second refresh while one runs fails with "already running" (the dashboard answers 409). The dashboard opens the DB read-only and before the first refresh reports "no data".

The store keeps aggregates permanently: rows per API response (no content), per cost-state run, per limit event, per session. Transcript deletion does not remove anything from it. The first refresh backfills every file that still exists.

### 2. Incremental and idempotent

`source_files` records, per file, the byte offset read so far, the file size and mtime, and the parser's carry state. A refresh skips unchanged files and reads only appended bytes; a half-written last line is left for the next refresh. Each file's records and its new offset commit in one transaction, so a crash leaves both or neither. Every row has a stable key and every write is an upsert, so `nucleus usage refresh --full` (re-read everything) converges on the same rows. The reader holds one line at a time: peak memory on the full 9.2 GB backfill was 76 MB. Timings on the operator's machine: first backfill 10–17 s, incremental refresh 0.3–4 s.

### 3. Claude Code parsing

- **Responses.** Usage is on `type:"assistant"` lines (`message.usage`). One API response is written as several lines — one per content block, each repeating the usage — and a streamed response also writes partial lines (`output_tokens` 1, `stop_reason` null). The dedupe key is `message.id` (fallback `requestId`, then the line `uuid`); the line with the largest `output_tokens` wins, within a batch and across refreshes (the upsert only replaces a row when the new output is not smaller).
- **Token categories** stored per row: uncached `input`, `cache_write_5m`, `cache_write_1h` (from `usage.cache_creation`; a missing split counts as 5-minute), `cache_read`, `output`, and `reasoning` (`output_tokens_details.thinking_tokens`, a subset of output, display only).
- **Subagents.** `<session>/subagents/agent-<id>.jsonl` is not part of the main file. Its rows carry the parent session id and the subagent id; the agent type comes from `agent-<id>.meta.json`.
- **Shared responses.** A resumed or forked session file repeats responses that another session file already recorded. The row is stored once (the first file processed owns it); `usage_keys` records every session whose files contain the key, which the reconciliation uses (§4).
- **Errors and limits.** Synthetic assistant lines (`isApiErrorMessage`, model `<synthetic>`) become limit events: usage limits (`error:"rate_limit"`, 429, `quotaLimits.rateLimitType`, `resetsAt`), overload (529 and other 5xx), `invalid_request` ("prompt is too long"), authentication failures.
- **Titles and working directory** come from `ai-title` / `custom-title` lines and the first line carrying `cwd`.

### 4. Claude Code cost-state and reconciliation

Claude Code writes `type:"cost-state"` lines: per model, the running token totals of one process run (`startTime`) and its own dollar estimate `costUSD`. Two findings from the real data decide how it is used:

1. It is not a session total. A resumed session starts a new run and the counter does not always carry the earlier run's totals, so long resumed sessions show cost-state totals far below their transcript.
2. It counts calls that no assistant line records — background calls that reuse the session's context (cache reads with little output), title generation on a small model. On the operator's data this is about 3% of Claude tokens.

Rule, per (session, run, model) over the run's window `[startTime, last timestamp before the snapshot]` and per token category, with `O` = observed responses whose key appears in the session's files and `C` = cost-state totals:

- **Tokens counted = max(O, C).** The responses, plus a derived **residual** row of `max(0, C − O)`.
- **Dollars = costUSD for everything cost-state covers, and the price table for observed tokens beyond it.** Implemented as table prices on every response and residual row plus a derived **adjustment** row of `costUSD − table(C)`. The sum is `costUSD + table(max(0, O − C))`.

Nothing is counted twice: residual tokens are only the excess of `C` over `O`, and the adjustment replaces the table's price of `C` instead of adding to it. Cost-state records cache writes without the 5-minute/1-hour split; `C`'s split follows the window's responses (1-hour when the window has none). Residual and adjustment rows are deleted and recomputed on every refresh. The total adjustment is the price table's drift from Claude Code's own prices; on the operator's data it is −$0.37 over $3,654 of cost-state estimates (1,738 runs).

### 5. Codex parsing

- The first `session_meta` names the thread, its working directory and, for a subagent thread, the parent (`source.subagent.thread_spawn.parent_thread_id`); a subagent's usage is attributed to the parent session. `turn_context.model` sets the model for the following turns.
- `event_msg` / `token_count` carries `info.total_token_usage` (cumulative for the thread) and `info.last_token_usage` (the response that produced the event). Codex re-emits an unchanged event after some turns, and a thread can restart its counter. Rule: an event whose total equals the previous event's total is a repeat and is skipped; every other event contributes its `last_token_usage`. This is also correct for a forked thread, whose first total includes the parent's history.
- OpenAI counts cached tokens inside `input_tokens` and reasoning inside `output_tokens`; the row stores `input − cached` as input so categories do not overlap.
- `rate_limits.primary/secondary` readings are stored (deduplicated per reset window and percentage); a non-null `rate_limit_reached_type` is a limit event, one per limit type and reset window.

### 6. Price table and the dollar estimate

Every dollar figure is labelled "estimate at API list price". The built-in table (`core/src/usage/pricing.rs`) lists every model found in the Claude Code and Codex logs, in USD per million tokens (input, output, cache read, cache write 5-minute and 1-hour), with the source and the date it was read (`PRICES_AS_OF`). `[usage.prices]` in `nucleus.toml` overrides or extends it. A model matches by exact id, then by the longest table key that is a prefix at a `-` boundary; a `[1m]` suffix is ignored. Anthropic prices were checked against Claude Code's cost-state (§4). `codex-auto-review` (Codex's automatic approval-review model) is not on OpenAI's pricing page; its price comes from a third-party price aggregator and is marked as such in the table.

A model without a price is counted in tokens and costs 0 in the sums; every total carries `unpriced_tokens`, and the page shows "+ N tokens without a price" next to any dollar figure that includes such tokens, so a missing price never reads as a lower cost. The price table and each model's match are listed on the page.

### 7. Projects

A project is a repository root, named by the root directory's name at runtime (no names are configured). Resolution of a working directory: Claude scratch directories (`/tmp/claude-<uid>/<encoded-cwd>/…`) map to the session directory they encode; a configured worktree marker (`/.claude/worktrees/`, `/.worktrees/`) cuts the path; otherwise the nearest ancestor with `.git` is the root, and a `.git` file (linked worktree) points to the main repository. For a deleted directory (the top-most missing path component) and the existing directory that held it, in order: a sibling that earlier resolved through a `.git` file gives the repository; a holding directory named like a known repository is the `<manager>/<repo>/<workspace>` layout of worktree managers; a deleted `<repo>-<suffix>` next to a known `<repo>` is the `git worktree add ../<repo>-<branch>` layout; otherwise the deleted directory is its own project. "Known repository" means a root already resolved through git or a marker. Mappings are stored; once a directory is gone its stored mapping is kept, except the last fallback, which is retried on every refresh.

### 8. Nucleus attribution

Labels, lowest priority first: venue DBs (`discord.db`, `whatsapp.db`, `chat.db`, `jobs.db`), `session_index.db` (ADR-023), `runs.jsonl` (ADR-016), and reminder fires (`reminder_fires.msg_id = skill-fire:<session>[|…]` → reminder id, agent `reminders-fire`). Labels are copied into `usage.db` and kept after the source forgets them; reminder title, cron and status are copied too. Sessions of the Nucleus workspace without a label are grouped as `unlabeled`: the operator's interactive sessions plus bot sessions whose label had rotated out of every source before the first refresh.

"Recurring jobs" are cron reminders that are still active, pending or paused, plus the registry's `launchd-cron` agents. Their monthly cost is the actual cost of the last 30 days (not a projection from the schedule), so skipped condition ticks and pauses are reflected.

### 9. Surface

`/usage` (observability group). Filters in one row, kept in the URL (`?tool=&days=&metric=&tab=`): tool (all / Claude / Codex), range (7 / 30 / 90 days / all), measure (dollars / tokens). Tabs:

- **overview** — today vs yesterday, this week vs the same weekdays last week, range vs the previous range, cache-read share, per-tool totals, Codex's last recorded weekly percentage with its timestamp, daily and weekly stacked columns, hour-of-day × weekday heatmap;
- **projects**, **models** (with the price table and the reconciliation figures), **nucleus** (recurring jobs, agents, reminders), **limits** (event timeline, Codex weekly percentage per day, event table), **sessions** (largest sessions with resume command and transcript path).

**Tool filter.** Every data endpoint takes `vendor=all|claude|codex` and applies it in SQL, so totals, comparisons, rankings, heatmap, sessions and limit events are computed over the selected tool only. Views that exist for one tool show a "not applicable" state under the other filter: Codex quota under Claude, Nucleus agents and reminders under Codex. Every headline figure states the tools it covers (`[Claude + Codex]`, `[Claude]`, `[Codex]`).

Charts are hand-rolled SVG with the locked palette: Claude = the amber accent, Codex = the neutral faint gray, heatmap = one amber ramp. The two-series pair fails the dataviz validator's lightness-band and chroma checks (the locked palette has one accent and neutrals) and passes the colorblind and normal-vision separation checks (ΔE 25.8 / 28.3); identity never relies on color alone — each two-series chart has a legend and every view has a table.

Claude data before (first refresh − 30 days) is incomplete, because those transcripts were already deleted; the status endpoint returns that day and the page says so on any comparison that reaches before it. Codex history is complete.

API: `GET /usage/api/{status,summary,projects,nucleus,limits,sessions}`, `POST /usage/api/refresh`; wire types are ts-rs generated.

## Rejected alternatives

- **Aggregating only daily rows.** Per-response rows are needed for exact dedupe across refreshes, the heatmap, largest sessions and reconciliation windows. They are small (no content): about 60,000 Claude responses and 38,000 Codex events for the whole corpus.
- **Using cost-state as the Claude total.** It undercounts resumed sessions (§4, finding 1).
- **Ignoring cost-state.** It would lose the background calls it alone records (§4, finding 2).
- **Writing from the dashboard process.** Violates ADR-020's single-writer rule; the dashboard spawns the CLI instead.
- **A live Claude quota.** Claude transcripts record limits only when one is hit; there is no reading to show between hits.

## Consequences

- Usage history survives Claude Code's transcript cleanup, provided a refresh runs at least every 30 days (the distiller runs one daily).
- A new model appears as "without a price" until it is added to the table; the page flags it.
- A change of `NUCLEUS_TZ` recomputes the stored local-day fields on the next refresh.
