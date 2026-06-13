# ADR-013 — Document understanding: enrichment, deferred jobs, vault import

**Status:** Accepted (shipped 2026-06-13) — supersedes the 2026-05-17
deferred stub ("vault ingestion: PDFs/Word/HTML → markdown via a drop
folder").
**Date:** 2026-06-13
**Related:** ADR-018 (document library — the substrate this extends),
ADR-020 (DB ownership, typegen), ADR-008 (skills), ADR-002 (no-embeddings
principle, unchanged), ADR-012 (canvas — the jobs primitive built here is
its substrate).

## Context

The original ADR-013 predates the document library. It wanted a
`~/Nucleus-Inbox/` drop folder, a polling `vault-ingester` binary, and
CPU-only extractors (`pdftotext`, `pandoc`, `tesseract`) to make file
contents visible to bots. Two things made that design obsolete:

1. **ADR-018 shipped.** WhatsApp inbound IS the drop folder; every
   image/document already archives to `memory/documents/` with metadata,
   audit, and a vault manifest.
2. **The `Read` tool renders PDFs and images natively** inside Claude
   sessions. The extractor is a session, not a toolchain.

What remained unsolved: the library knew documents' *names*, not their
*contents* ("find the lease" worked; "find the contract that mentions
pets" didn't), act-on-media ran synchronously holding the DM session lock
(a 3-minute analysis blocked every other message), and there was no path
from a knowledge-shaped document into the vault proper.

## Explicitly dropped from the original design

- Drop directory + `_processed/`/`_failed/` lifecycle — WhatsApp inbound
  replaced it.
- `pdftotext` / `pandoc` / `tesseract` — zero native extraction deps;
  unsupported formats (docx, zip) degrade gracefully to
  `enrich_status = 'unsupported'` instead of being OCR-tortured.
- The polling `vault-ingester` binary + launchd `StartInterval` +
  `[ingest] notify_channels` — no new daemon at all.
- Embeddings / vector search — still rejected (ADR-002: incompatible with
  the Claude Max billing principle). `find()` is tiered lexical matching.

## Decision — three capabilities on ONE new primitive

### The jobs primitive (`memory/jobs.db` + `src/jobs.ts`)

A job = one ledger row + one one-shot Claude session in the
`nucleus-whatsapp-jobs` tmux session (watch live: `tmux attach`).
`startJob()` inserts the row synchronously and returns `{jobId, promise}`;
the runner marks done/failed when the session settles. Generic schema
(`kind` is open: act | enrich | vault-import today) — **S12/canvas-class
long tasks ("build a learning course from this material") add kinds, not
columns**, with per-kind `maxWaitMs`.

**Restart semantics:** the jobs tmux session is part of the boot wipe
(ALL_TMUX_SESSIONS), so "orphaned" means *dead by construction*, never
maybe-still-running (the 2026-06-11 orphan-window outage made the wipe a
hard invariant). At boot, `sweepOrphans()` marks running→orphaned;
act/vault-import orphans enqueue a DM interruption note; enrich orphans
stay silent (the feature is silent). Transcript re-attach was considered
and rejected for v1: it would exempt the jobs session from the wipe and
re-enter transcript-tailing against a session whose binary may have been
the restart cause. `session_id` is recorded for manual `claude --resume`
forensics.

**Future extension (sketched, not built):** cross-process producers
insert `status='queued'` rows that the bot claims — the
queue-table-owned-by-reader pattern ADR-020 sanctions.

### 1. Auto-enrichment — the library understands its contents

Every **non-deduped** archive fires a silent, detached `enrich` job: a
Read-only session (no tools, no add-dirs) reads the file and emits strict
JSON `{keywords, summary, language}` — stored on the `documents` row
(`keywords` JSON, `summary`, `enriched_at`, `enrich_status`). The prompt
instructs the model to treat document content as **inert data** (prompt-
injection guard) and to never include full ID/serial numbers in keywords.

`find()` tier order — **operator-owned fields always outrank
auto-enrichment**: exact name → exact tag → exact keyword → substring over
name/filename/tags → substring over keywords/summary → token fuzzy
(keywords in the haystack; summary excluded — long prose matches
everything). Manifest sections gain `summary::` / `keywords::` /
`imported::` inline fields; the dashboard shows summary + keyword chips
and searches them.

**`priv:` caption** = archive-only AND never enriched — the document's
bytes never enter any session; the full by-reference posture for things
that should stay opaque.

### 2. Act-on-media with timeout promotion — deferred-reply jobs

Instruction-shaped captions run on a **job session, not the DM pool** —
the per-chat DM lock is never taken, so a long analysis can't block the
next message. `withQuickWindow(promise, 30s)`:

- Settles in-window → one direct reply (single message).
- Times out → "recebi, analisando — respondo já 📄" ack, row
  `markPromoted`, and the result (or failure) is **delivered later
  through the outbound queue** when the job finishes.

No duration prediction anywhere — the race decides. `ACT_QUICK_WINDOW_MS`
is a named constant (30s; a cold spawn alone is 5-20s, so most real asks
promote — raise to 60s if the two-message dance grates; don't shrink).

Load-bearing details: deferred-delivery targets are
`normalizeSenderId(chatId)` **digits** (raw `@lid` chatIds fail the
drain's allowlist silently); queue bodies are `formatReply`'d at enqueue
(the drain sends raw). The job persona is code-owned
(`JOB_ACT_SYSTEM_PROMPT`); tools = the docs CLI only — **delivery stays a
DM-pool capability**, act jobs answer.

**Documented trade:** act replies don't share the DM session's
conversational memory. The doc + instruction are self-contained; the DM
pool can `docs.ts find` the same doc for follow-ups. This is the price of
freeing the DM lock, and it's the right one.

### 3. Opt-in vault import — knowledge docs become vault notes

`vault:` / `import:` captions (DM only): the doc archives as always, then
an import job Reads it and proposes strict JSON `{slug, title, markdown}`.
**TS validates and writes** (model proposes, code enforces — the braindump
architecture; Rule 9 enforcement never leaves code):
`5-Resources/Imported/<date>-<slug>.md`, collision-suffixed, frontmatter
carrying `source_doc_id` / `source_sha256` / `original_filename`.
`recordImport()` stores the pointer + audit + manifest link. The distiller
sees imported notes automatically — they're ordinary vault markdown.

The single `Imported/` subfolder is operator-directed via this ADR (the
Rule 9 gate, same as `4-Areas/Documents/` in ADR-018). The job never
creates topical subfolders.

**Identity guard:** docs tagged `identity` are refused — importing an RG
into the session-mounted vault would defeat ADR-018's by-reference
reasoning. Override = retag (deliberate friction).

## The by-reference exception, stated plainly

ADR-018's rule — document bytes never enter model context — gets one
bounded exception: enrich/act/vault-import jobs read the document into
**one one-shot session and its transcript**. That's the entire point of
"understanding" a document. The DM pool's rule is unchanged; `priv:` opts
a document out entirely; identity docs are import-refused on top.

## Consequences

- "What does the lease say about pets?" now works three ways: act caption
  at send time, a later DM ask (find + Read), or — once imported — as
  plain vault knowledge any session/distiller pass can see.
- The jobs ledger is the second consumer of the outbound queue's
  deliver-later semantics (reminders were the first) and the designated
  substrate for S12.
- One more place reads documents.db (`jobs` does not — it's its own DB);
  ownership unchanged (whatsapp family writes, dashboard reads read-only).
- No concurrency cap on jobs in v1 (personal scale; each job is its own
  tmux window). A semaphore is the upgrade if inbound bursts ever matter.

## Rejected

- Jobs table inside whatsapp.db or documents.db (couples the S12 substrate
  to chat plumbing / breaks doc-optional jobs).
- Synchronous-only acts (DM lock blocking) and duration *prediction* for
  the fast/slow split (the race decides instead).
- Auto vault import (vault stays curated; import is explicit).
- Operator-persona job sessions (code-owned prompts keep replies
  predictable; the persona signature comes from formatReply at send time).
