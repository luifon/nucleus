# ADR-023 — Session search: transcript retrieval between T1 and T2

Date: 2026-07-18
Status: accepted + built (2026-07-19)

> **As-built** (`core/src/session_index.rs` + `session-search` bin;
> maintenance folded into the distiller daily pass; `[session_search]`
> toml knobs, `prune_apply=false` shipped default):
>
> 1. The junk premise was overstated: first real index = 380 transcripts,
>    **377 eligible, 3 junk**. Most one-shot spawns (fires) carry a real
>    user payload + assistant deliverable and are legitimately
>    searchable; true junk is wedged/no-op sessions only. The gate
>    (≥1 real user + ≥1 assistant turn post-filtering, synthetic-agent
>    prefix exclusion) identified exactly those 3 — including the two
>    wedged heartbeat attempts of 2026-07-18.
> 2. Turn timestamps are session-level (file mtime), not per-turn — the
>    `--days` filter needs no finer grain.
> 3. Agent attribution joins runs.jsonl (ADR-016), which rotates (~50
>    rows/agent), so older sessions show agent `?` — searchable, just
>    unlabeled. Accepted; labels are advisory.
> 4. No cross-language fixture file for the gate — it has no TS mirror;
>    inline table-driven tests cover it (+ a tempdir FTS round-trip test
>    exercising index → porter-stemmed search → dry-run → real prune
>    with the path-safety rails).
>
> Verified on the live corpus: "consórcio adm" and "wedged submit
> verify" both recalled the right sessions with usable snippets;
> incremental re-index skips unchanged files; dry-run prune reports 1
> candidate and deletes nothing.

## Context

Everything that happens in a session and does not get distilled is
unrecoverable to the agents. Diaries (T1.5) capture what a bot chose to
observe; T2 holds promoted facts; the vault (T3) holds deliberate writes.
But "what did we decide about X two weeks ago?" — when X never made it into
any of those — requires the operator to grep raw transcripts by hand. Both
OpenClaw (`memory_search`, hybrid BM25+vector) and Hermes (`session_search`,
SQLite FTS5) shipped exactly this and treat it as a flagship memory feature:
the agent can search its own past sessions on demand, paying tokens only at
recall time.

The corpus today: ~350 transcripts, 140 MB, in the Claude project dir for
this workspace. Two properties matter:

1. **A large fraction is junk.** Every chat-title spawn, reminder fire,
   sstest experiment, and wedged-and-killed session leaves a transcript.
   Median file is ~43 KB but even a no-op spawn carries ~20 KB of
   boilerplate (context injection, file-history snapshots) — so emptiness
   is a CONTENT property (no substantive user↔assistant exchange), never a
   file-size one.
2. **The corpus is append-mostly and single-machine.** No sync, no
   multi-writer problem; a daily incremental index plus a catch-up scan is
   enough.

## Decision

A `session-search` capability owned by core, in three parts:

### 1. Index

- SQLite + FTS5 at `memory/session_index.db` (ADR-020 DB-ownership: core
  owns it; migrations via the standard seam). No embeddings, no external
  providers — FTS5 with porter stemming covers the recall need at zero
  standing cost; hybrid/vector can be revisited if FTS proves insufficient.
- Indexed unit: **turn** (role, text, session id, timestamp, venue/agent
  label derived from the transcript's session metadata), so hits return a
  locatable excerpt, not a whole file.
- Turn extraction reuses the `lastNTurns`-family filtering already built
  for priming (skip tool results, system injections, date preambles) — one
  definition of "a real turn" across the codebase.

### 2. Eligibility gate (the junk problem)

A transcript enters the index only if it shows a **substantive exchange**:

- ≥ 1 non-injected user turn AND ≥ 1 assistant text turn beyond the
  priming/preamble machinery, and
- not name/label-matched as synthetic (chat-title spawns, `sstest*`,
  test-skill fires, healthcheck probes).

Everything else is invisible to search. The gate is a pure function with
shared test vectors (same pattern as `submit_verify_vectors.json`).

### 3. Physical pruning (kill junk in full)

Index-side exclusion alone still leaves dead files accumulating forever.
A prune pass (folded into the distiller's daily 4 am run, per ADR-016's
consolidation stance — no new launchd job) deletes ineligible transcripts
older than 14 days, **in full**: transcript file + any sidecar. Eligible
transcripts are never auto-deleted. Deletion also forfeits `--resume` for
those sessions — acceptable by definition, since the gate says nothing
happened in them. The pass logs counts (`pruned N junk transcripts`) to the
distiller diary; ADR-020's no-silent-caps rule applies.

### Query surface

- A small CLI (`session-search "<query>" [--venue x] [--days n]`) callable
  from any session via Bash — no MCP server, no new tool plumbing; skills
  and personas can name it directly (same pattern as the `reminders` CLI).
- Dashboard: a search box on the sessions/agents surface later, not in v1.

## Consequences

- Bots gain "search your own past" without any standing context cost;
  the tier model gets its missing retrieval layer between T1 and T2.
- Junk sessions stop accumulating: excluded from recall immediately,
  deleted after 14 days.
- The index is derived data — corruption/loss is repaired by a full
  rescan; no backup obligations.
- New failure surface: the eligibility heuristic misclassifying a real
  session as junk would delete it at day 14. Mitigation: the prune pass
  runs `--dry-run` for its first week in production, logging what it
  *would* delete, before the deletion flag flips on.
