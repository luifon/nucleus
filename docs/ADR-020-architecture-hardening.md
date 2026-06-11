# ADR-020 — Architecture hardening pass

**Status:** Accepted (2026-06-11) — Implemented (2026-06-11)

**Builds on / touches:**
- [[ADR-006]] — reminders: the tick lock gains a heartbeat; the fire paths
  move onto session profiles.
- [[ADR-008]] — skill fires: the narration-leak guard becomes part of the
  shared `run_one_shot` contract (`OneShotOutcome.ended_clean`).
- [[ADR-016]] — run-log rows gain `claude_version`; the orphaned pre-016
  launchd services are finally removed mechanically.
- [[ADR-017]] — fixes the skill-gap-learner's session posture (two
  long-standing config drops).

## Context

A six-perspective architecture review of the whole stack (core crate,
WhatsApp TS bot, Rust binaries, dashboard, ops/config layer, cross-cutting
seams) produced a tiered findings list: real operational bugs, durable
architecture gaps, and hygiene debt. This ADR records what shipped, the
policies decided along the way, and — as importantly — the review
recommendations we explicitly **rejected**, so they don't get re-litigated
every six months.

## What shipped

### Session substrate

1. **`SessionProfile` (core/src/session_profile.rs) is the only way
   binaries configure sessions.** Before: 5 distinct hand-rolled
   `SpawnOptions`/`AskOptions` shapes across 10 call sites, and twice the
   safety-critical knobs were silently dropped — skill-gap-learner ran
   with `await_turn_complete: false` (mid-task cutoff while writing skill
   files, the same class as the DSU skill-fire bug) and without the
   Settings `disallowed_tools`. Profiles make those errors
   unrepresentable: one-shot constructors hard-code
   `await_turn_complete: true` with no override; the security posture
   comes from `ProfileContext` (borrowing Settings) and only has an
   add-only `extend_disallowed_tools`; MCP-gated sessions take their
   `allowed_tools` as a constructor argument. `Default` for the raw
   option structs is **deleted** — a new call site that bypasses the
   profile layer is a compile error, not a latent bug.
2. **`run_one_shot` → `OneShotOutcome.ended_clean`** standardizes the
   narration-leak guard (a session cut off mid-action returns its last
   internal monologue line as the "reply"; forwarding that posts under
   the operator's identity). Reminders' skill-fire path now consumes it;
   any future unattended-output path must too.
3. **`SessionPool` two-phase slots.** The map write lock was held across
   cold `Session::spawn` (5–60s), serializing every chat behind one cold
   boot; the dead-window check had a TOCTOU; reap/shutdown's
   `Arc::try_unwrap` silently leaked sessions held by in-flight asks.
   Now: brief map lock claims an `Arc<Mutex<Slot>>` (`session: Option`),
   the slot mutex is the per-key serializer, spawns happen under it only.
   Lock ordering: slot-then-map; the map lock is never held while
   acquiring a slot.
4. **Session fields are private.** tmux targets and transcript paths are
   implementation details of this process model; callers use read-only
   accessors. Blocking transcript reads on async paths go through
   `spawn_blocking` wrappers.

### Data layer

5. **DB ownership rule.** Each `memory/*.db` has exactly one writer
   process. The one sanctioned cross-process write is a **queue table**
   owned by the reader (whatsapp.db's `outbound_queue`, drained by the
   bot, enqueued by reminders) — that's the pattern for any future
   cross-venue delivery, not ad-hoc reaching into another service's
   tables. `db::open` sets `busy_timeout(5s)` so reader/writer overlap
   retries instead of failing fast.
6. **Versioned migrations (core/src/migrate.rs).** The per-binary
   "ensure_schema on every boot" pattern could never express *run
   exactly once*, so backfills and value sweeps re-executed forever and
   reminders accumulated 8 string-matched tolerated ALTERs. Now: a
   `schema_migrations` ledger; each DB's **v1 is its historical
   ensure_schema body verbatim** (idempotent — existing DBs heal once
   and get the baseline row); v2+ are plain run-once steps.
   `Step::Sql` is atomic (statements + version row inside one
   `BEGIN IMMEDIATE`); `Step::Rust` runs on the pool and must be
   idempotent. whatsapp.db is untouched — the TS process owns it.
   Behavior change: reminders' channel-rename sweep now runs once.

### Claude-binary coupling

7. **Version is logged, not pinned.** Decision: keep running the latest
   `claude` binary always — the operator wants current features and the
   stack tolerates churn well enough that freezing versions costs more
   than it saves. The trade is observability: every run-log row now
   records `claude_version` (captured once per process via
   `claude --version` on the same `claude_bin()` resolution the spawn
   uses), so when something breaks after an upgrade the forensic trail
   is one `runs.jsonl` grep away. The fail-fast/pinning alternative was
   considered and rejected (see below).

### Ops layer

8. **Hard-cut orphan pruning.** `tools/launchd/install.sh` removes any
   installed `${PREFIX}.*.plist` with no matching template on every full
   run. A service deleted from the repo dies on the next install run —
   no more loaded-but-forgotten daemons (distiller-hourly/-weekly and
   preference-learner ran redundantly for weeks after ADR-016/017).
   launchctl usage modernized to `bootout`/`bootstrap` throughout.
9. **healthcheck derives from agents.toml.** The hardcoded service lists
   had drifted (still checking the orphans) and leaked operator-specific
   labels into a committed file (Rule 1 violation). Now the lists come
   from the registry (`launchd-daemon` → persistent, `launchd-cron` →
   periodic), bonsai via its env gate, and operator extras via
   `HEALTHCHECK_EXTRA_PERSISTENT`/`_PERIODIC` in `.env`.
10. **Log rotation via newsyslog** (`tools/newsyslog/nucleus.conf.example`
    → `/etc/newsyslog.d/nucleus.conf`): `memory/*.log` rotates at 1MB,
    keep 5. **gzip deliberately omitted**: KeepAlive daemons hold their
    `StandardOutPath` fd across rotation and keep appending to the
    renamed `.0` until restart — compress+unlink would silently destroy
    those writes; uncompressed archives preserve them at ~6MB/service.

### Cross-language and web

11. **Rust DTOs are the single source of wire truth** (Rule 12 in
    CLAUDE.md). ts-rs derives on the dashboard DTOs + shared core types
    export to `web/src/lib/api/generated/`; hand-written api modules
    keep fetch functions and documented UI-layer narrowings only. The
    migration itself caught two stale hand-written types. Drift gate:
    `npm run check:api`. Deliberately not wired into the web build —
    it must not require a Rust toolchain.
12. **WhatsApp drain hang protection.** The re-entrancy flag was correct
    but fail-closed: a hung (never-settling) `sendMessage` wedged the
    queue silently forever. Per-send 20s timeout (retry + rot-counter +
    tick abort, with a late-completion continuation that suppresses the
    duplicate when the send lands after all) plus a 460s watchdog that
    exits for a launchd respawn. Retry-over-drop because this queue
    carries operator notifications. Baileys socket typed as `WASocket`
    everywhere; `formatReply` deduplicated into `src/format.ts`.
13. **Dashboard resilience**: route-level ErrorBoundary (keyed by
    pathname; sidebar survives a page crash) and real fetch
    cancellation (AbortController threaded through the client helpers
    and `useFetch`).

## Rejected alternatives — and why

- **A supervisor daemon owning all DBs behind IPC.** Over-engineering at
  single-operator scale. The federation of independent launchd binaries
  is the right isolation model; the ownership rule + queue-table
  convention + busy_timeout deliver the consistency guarantees the
  supervisor was supposed to provide, without a new always-on process
  and a bespoke IPC protocol.
- **Migrating T2 memory to SQLite.** T2 *is* Claude Code's file-based
  auto-memory (`MEMORY.md` index + one file per fact) — that layout is
  the harness contract, not an implementation choice. A DB would break
  recall integration outright.
- **Porting the WhatsApp bot to Rust / compiling core to WASM for TS.**
  No mature Baileys equivalent exists in Rust; the mirror-API
  maintenance cost is real but bounded, and both bridge options add a
  build/runtime layer worse than the disease. The TS mirror stays,
  kept honest by code review and the shared incident comments.
- **Redux-class state management for the dashboard.** Scale doesn't
  justify it; the shared hooks + per-domain modules pattern holds.
- **Pinning the Claude Code version / fail-fast on mismatch.** Rejected
  in favor of №7 above: the operator explicitly prefers running latest;
  pinning would trade a rare debuggable failure for a constant upgrade
  chore. If transcript-format or TUI churn ever breaks the substrate in
  practice, revisit with the `claude_version` evidence in hand.

## Behavior changes worth knowing

- gmail metabolism now runs with `await_turn_complete: true` (was a
  flagged candidate; profile default).
- Distiller, news scoring, and chat-title sessions now carry the
  Settings `disallowed_tools` they previously silently lacked.
- Reminders' legacy channel-name sweep runs once (migration v1) instead
  of on every boot.
- `AskResult.was_cold_spawn` is now exact (set when this call spawned)
  rather than an elapsed-time heuristic.

## Verification (as shipped)

Workspace build/test/clippy green (95 tests); migration runner exercised
against copies of the live reminders.db and news.db (idempotent, data
intact); 3 orphan plists pruned and 9 services rebootstrapped live;
healthcheck 21 pass / 0 warn / 0 fail with registry-derived lists; a live
reminder delivered once through the new WhatsApp drain path; dashboard
serving with generated types; `npm run check:api` clean.
