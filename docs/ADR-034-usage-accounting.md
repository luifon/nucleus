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

A machine used daily with both tools holds thousands of such files and several gigabytes of logs; Codex logs are several times larger than Claude transcripts because they are never deleted.

## Decision

### 1. Store and ownership

`memory/usage.db`, owned by core (`nucleus_core::usage`). The only writer is `usage::refresh`, reached from two places:

- `nucleus usage refresh` — the dashboard's refresh button spawns this command as a child process (the dashboard itself never writes the DB);
- the distiller's daily pass, after the session-index maintenance.

An exclusive advisory lock (`flock`) on `memory/usage-refresh.lock`, held through an open file descriptor for the whole refresh, serializes the two. The kernel releases it when the descriptor closes, also when the process dies, so there is no staleness timer that could expire under a healthy refresh and no lock file for a second process to delete. A second refresh while one runs fails with "already running". The dashboard adds an in-process single-flight gate: a POST takes the gate, answers 409 when its own child is still running or the lock is held, and only then spawns, so simultaneous requests start one child. The dashboard opens the DB read-only and before the first refresh reports "no data".

The store keeps aggregates permanently: rows per API response (no content), per cost-state run, per limit event, per session. Transcript deletion does not remove anything from it. The first refresh backfills every file that still exists.

### 2. Incremental and idempotent

`source_files` records, per file, its identity (device and inode), size, mtime, the byte offset read so far, the parser's carry state, and a fingerprint of the part already read: a hash of the first 64 KiB and of the 64 KiB that end at the offset. Per file a refresh:

- **skips** it when identity, size and mtime are unchanged;
- **appends** from the stored offset with the stored carry when the identity is the same, the file is not shorter than the offset, and the fingerprint still matches;
- otherwise **re-reads** it from byte 0 with an empty carry (new file, `--full`, a different inode at the path, truncation below the offset, or content rewritten in place), deleting every observation of that path first.

A half-written last line is left for the next refresh. Each file's deletions, records and new state commit in one transaction, so a crash leaves all or none of it. An in-place edit that keeps the size and touches neither fingerprint window is not detected; `--full` recovers it.

The store has two layers. Per-file **observations** (`usage_obs`, `cost_runs`, `limit_events`, `rate_snapshots`, keyed by path) record what each file says. **Counted rows** (`usage_rows`) are derived from the observations of every file that contains a response: the observation with the largest output (the final line of a streamed response), then the file whose first line is oldest (the original session rather than a fork that copied it), then the path. Only the keys a refresh touched are re-derived; a key no file observes any more is removed. Every key is derived from content, never from a byte position, and the derivation depends only on the stored observations, so `nucleus usage refresh --full` produces exactly the rows of the incremental history. A test drives one fixture through appends, a same-size rewrite, truncate-and-regrow, a fork, a resume, Codex counter resets and a replace-by-rename, and compares every counted value against a fresh `--full` read.

Memory is bounded: the reader holds one line of at most 32 MiB (a longer line is skipped and counted), and records reach the database in batches of 2,000 through a bounded channel inside the file's transaction. Files that cannot be read, relevant lines that are not valid JSON, and oversized lines are counted per refresh in `refresh_runs` (with the first warnings) and per file in `source_files`; the page shows a partial refresh as `[PARTIAL: …]`, and lines skipped in stored files stay reported until the file changes. Deleted-between-discovery-and-read files are not failures. Discovery never follows a symbolic link below the configured roots, so a link cycle or a link out of the root is never walked.

### 3. Claude Code parsing

- **Responses.** Usage is on `type:"assistant"` lines (`message.usage`). One API response is written as several lines — one per content block, each repeating the usage — and a streamed response also writes partial lines (`output_tokens` 1, `stop_reason` null). The dedupe key is `message.id` (fallback `requestId`, then the line `uuid`); the line with the largest `output_tokens` wins, within a batch and across refreshes (the upsert only replaces a row when the new output is not smaller).
- **Token categories** stored per row: uncached `input`, `cache_write_5m`, `cache_write_1h` (from `usage.cache_creation`; a missing split counts as 5-minute), `cache_read`, `output`, and `reasoning` (`output_tokens_details.thinking_tokens`, a subset of output, display only).
- **Subagents.** `<session>/subagents/agent-<id>.jsonl` is not part of the main file. Its rows carry the parent session id and the subagent id; the agent type comes from `agent-<id>.meta.json`.
- **Shared responses.** A resumed or forked session file repeats responses that another session file already recorded. Each file's observation is kept; the counted row is derived once (§2), and the reconciliation uses the observations to know every session whose files contain the key (§4).
- **Errors and limits.** Synthetic assistant lines (`isApiErrorMessage`, model `<synthetic>`) become limit events: usage limits (`error:"rate_limit"`, 429, `quotaLimits.rateLimitType`, `resetsAt`), overload (529 and other 5xx), `invalid_request` ("prompt is too long"), authentication failures.
- **Titles and working directory** come from `ai-title` / `custom-title` lines and the first line carrying `cwd`.

### 4. Claude Code cost-state and reconciliation

Claude Code writes `type:"cost-state"` lines: per model, the running token totals of one process run (`startTime`) and its own dollar estimate `costUSD`. Two findings from the real data decide how it is used:

1. It is not a session total. A resumed session starts a new run and the counter does not always carry the earlier run's totals, so long resumed sessions show cost-state totals far below their transcript.
2. It counts calls that no assistant line records — background calls that reuse the session's context (cache reads with little output), title generation on a small model. They are a small share of Claude tokens.
3. Runs overlap. A fork or resume repeats responses of an earlier run inside its own window, and one process that hosts several sessions can carry its counter from one session into the next, keeping the original start time.

Rule, per model, over each run's window `[startTime, last timestamp before the snapshot]`, with `O` = counted responses whose key appears in the session's files and `C` = cost-state totals:

1. **One owner per response.** Runs are ordered by (start, snapshot, session). A response in several runs' windows belongs to the first; later runs see it as carried.
2. **New counter part.** A run subtracts `D`, what its counter provably already holds from earlier runs. The proof is identity, not a comparison of totals. The counter **continued** an earlier run when that run has the same start time (the same counter), at least one response key appears in both windows, and no category of the earlier counter exceeds this run's; the latest such run is the one continued. Then `D` = that run's totals, background calls included (they are the same records of the same counter), plus any carried responses outside that run's window, and `D`'s dollars = that run's `costUSD` plus the extra responses' price share of the rest. Otherwise the counter **restarted**: `D` = the carried responses' tokens, with dollars in proportion to their table price (their token share for a model without a price); an earlier run's background tokens are never subtracted, because nothing shows they are in this counter. `C' = C − D`, `costUSD' = costUSD − D$`.
3. **Tokens counted = the owned responses plus a derived residual row of `max(0, C' − O_own)`** per category.
4. **Dollars = `costUSD'` for everything the run's new part covers, and the price table for owned responses beyond it.** Implemented as table prices on every response and residual row plus a derived **adjustment** row of `costUSD' − table(C')`. The sum is `costUSD' + table(max(0, O_own − C'))`.

Nothing is counted twice: each response has one owner, each counter's dollars are split by subtraction, residual tokens are only the excess of `C'` over `O_own`, and the adjustment replaces the table's price of `C'` instead of adding to it. For a model without a price, the dollars come from the adjustments alone, once per counter. Cost-state records cache writes without the 5-minute/1-hour split; the split follows the window's responses (1-hour when the window has none). Residual and adjustment rows are deleted and recomputed on every refresh. The total adjustment is the price table's drift from Claude Code's own prices; the models tab shows it next to the sum of `costUSD'`. A counter that continues an earlier run under a new start time is treated as restarted: only the shared responses are subtracted, so the earlier run's background calls can be counted twice. The data this was built on shows continued counters keeping their start time.

### 5. Codex parsing

- The first `session_meta` names the thread, its working directory and, for a subagent thread, the parent (`source.subagent.thread_spawn.parent_thread_id`); a subagent's usage is attributed to the parent session. `turn_context.model` sets the model for the following turns.
- `event_msg` / `token_count` carries `info.total_token_usage` (cumulative for the thread) and `info.last_token_usage` (the response that produced the event). Codex re-emits an unchanged event after some turns, and a thread can restart its counter. Rule: an event whose cumulative totals equal the previous event's in every field (input, cached, cache writes, output, reasoning, total) is a repeat and is skipped; every other event contributes its `last_token_usage`. Comparing every field keeps a reset whose new total equals the old one from being taken for a repeat. This is also correct for a forked thread, whose first total includes the parent's history. The row key is the thread id, the event timestamp and the cumulative totals, so a re-read produces the same keys.
- OpenAI counts cached tokens inside `input_tokens` and reasoning inside `output_tokens`; the row stores `input − cached` as input so categories do not overlap.
- `rate_limits.primary/secondary` readings are stored (deduplicated per reset window and percentage; a reading without a reset time uses a fixed placeholder key, so it deduplicates too); a non-null `rate_limit_reached_type` is a limit event, one per limit type and reset window.

### 6. Price table and the dollar estimate

Every dollar figure is an estimate at API prices, not billing. The built-in table (`core/src/usage/pricing.rs`) lists every model found in the Claude Code and Codex logs, in USD per million tokens (input, output, cache read, cache write 5-minute and 1-hour), and records per model its **basis**, source URL and retrieval date (`PRICES_AS_OF`):

- **API list price** — read from the vendor's pricing page (Anthropic's and OpenAI's).
- **Third-party estimate** — the vendor publishes no price. `codex-auto-review` (Codex's automatic approval-review model) is not on OpenAI's pricing page; its input and output rates come from a third-party price listing, whose URL and retrieval date the table stores. Every total carries `third_party_usd`, and the page marks each dollar figure that includes it ("incl. $X at a third-party estimate", with the source in the tooltip); the CLI report prints the source.
- **Inferred** rates — a rate the source does not list is derived and marked: `codex-auto-review`'s cached-input and cache-write rates follow OpenAI's ratios for its listed models, and `gpt-5.3-codex`'s cache writes are billed like uncached input because OpenAI lists no cache-write price for it.

OpenAI bills a request whose input (uncached + cached + cache writes) is more than 272,000 tokens at long-context rates for the whole request: 2 x input, cached input and cache writes, 1.5 x output, on the models the pricing page lists them for. The table carries those rates, and each response is priced on its own before any sum. Anthropic bills the full context of Claude 4.6 and later at the standard rates.

`[usage.prices]` in `nucleus.toml` overrides or extends the table (basis "nucleus.toml", optional `long_context`). A model matches by exact id, then by the longest table key that is a prefix at a `-` boundary; a `[1m]` suffix is ignored. Anthropic prices agree with Claude Code's own cost-state estimates to within the drift the models tab reports (§4).

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

Claude data before (first refresh − 30 days) is incomplete, because those transcripts were already deleted; the status endpoint returns that day and the page says so on any range that reaches before it, and always on "all time" while Claude is in scope. Codex history is complete.

Day bounds of every range (`range_from` in the summary, `from`/`to` of the limit events) are computed by the server in `NUCLEUS_TZ`; the page never derives a day from the browser's timezone, and it shows event times in `NUCLEUS_TZ` too. The Nucleus tab's 30-day figures (recurring jobs, agents, reminders) carry their own unpriced-token counts and third-party dollars, and every Nucleus dollar figure shows the same notes as the other tabs.

API: `GET /usage/api/{status,summary,projects,nucleus,limits,sessions}`, `POST /usage/api/refresh`; wire types are ts-rs generated.

## Rejected alternatives

- **Aggregating only daily rows.** Per-response rows are needed for exact dedupe across refreshes, the heatmap, largest sessions and reconciliation windows. They are small (no content): tens of thousands of rows for months of daily use.
- **Byte offsets in keys.** A key built from a line's position changes when a file is rewritten, and the re-read then counts the same usage under a second key.
- **A lock file with a staleness timer.** A long phase outlives any fixed timeout, and a second refresh then deletes the live lock. The kernel's advisory lock has no timeout to guess.
- **Using cost-state as the Claude total.** It undercounts resumed sessions (§4, finding 1).
- **Ignoring cost-state.** It would lose the background calls it alone records (§4, finding 2).
- **Writing from the dashboard process.** Violates ADR-020's single-writer rule; the dashboard spawns the CLI instead.
- **A live Claude quota.** Claude transcripts record limits only when one is hit; there is no reading to show between hits.

## Consequences

- Usage history survives Claude Code's transcript cleanup, provided a refresh runs at least every 30 days (the distiller runs one daily).
- A new model appears as "without a price" until it is added to the table; the page flags it.
- A change of `NUCLEUS_TZ` recomputes the stored local-day fields on the next refresh.
