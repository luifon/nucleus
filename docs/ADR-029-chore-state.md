# ADR-029 — Chore state: daily-session continuity and watermarks

**Status:** Accepted (2026-09-07) — Implemented (2026-09-07)

**Builds on:**
- [[ADR-016]] — the maintenance agents (distiller, skill-gap-learner) and
  their diaries.
- [[ADR-020]] — `SessionProfile` as the one place spawn posture lives; the
  DB-ownership rule; versioned migrations.
- [[ADR-026]] — the */30 heartbeat, whose transcript volume prompted the
  first daily-session implementation (reminders migration v7, 2026-09-06).

## Context

Two problems surfaced together.

**Transcript volume.** Every maintenance invocation spawned a fresh claude
session. The heartbeat alone produced ~29 transcripts a day; the distiller
two (metabolism, contemplation); the skill-gap-learner one per on-the-fly
review plus two for `learn`. `claude --resume` and the ADR-023 session index
filled with machine-generated sessions and buried real conversations. The
2026-09-06 fix scoped one-session-per-day to reminders only, with its own
table in `reminders.db`, and the other chores would each have needed a copy.

**Silent gaps.** The distiller's daily pass reads "yesterday and today". A
pass that fails (2026-08-31 through 2026-09-07 every night, from a paste
defect in the session layer) or a machine that is off at 04:00 leaves those
days unprocessed forever: the next pass reads the next window and the diary
prune later deletes what was skipped. The skill-gap-learner's `learn` has the
same shape with a 7-day window. Neither job knew where it had last got to.

Both are "state a chore carries from one run to the next", and neither
belongs in the job's own code or in a DB another binary owns.

## Decision

A core module, `nucleus_core::chore_state`, owning `memory/chore_state.db`
(ADR-020 DB-ownership: core is the sole writer; every chore goes through the
module). Two tables.

### `daily_sessions` — one claude session per local day per key

`SessionProfile::daily_session(key)` opts a profile in. `spawn()` and
`run_one_shot()` then:

1. look up the session recorded for `key` today;
2. resume it if there is one, otherwise spawn fresh;
3. if resuming fails to boot, forget the id and spawn fresh (a dead session
   must not poison the rest of the day);
4. on the first successful `ask`, record the session for `key` today. A
   session that never produced a reply is not what the next spawn resumes.

The date boundary is the local calendar day (`NUCLEUS_TZ`). One row per key,
overwritten when the date rolls.

Keys in use: `reminder-<id>` (reminders with the `daily_session` flag, today
the heartbeat), `distiller` (metabolism and contemplation share the day's
session), `skill-gap-learner` (every review fire and both `learn` passes).

`SessionProfile::resume()` and the reminders-local table are gone
(reminders migration v8 drops `reminder_daily_session`). `into_parts()` is
test-only now; production call sites that manage their own lifecycle use
`SessionProfile::spawn()`, so the continuity wiring cannot be bypassed.

### `watermarks` — where a job last got to

`chore_state::watermark / set_watermark(key, value)` store an opaque string.
`resume_date_after(key, default_days_back)` is the date-keyed helper: the
day after the watermark, or `default_days_back` days ago when there is none,
never later than today.

- **Distiller metabolism** — one watermark per agent
  (`distiller.metabolism.<agent>`): the last local date whose diary is fully
  processed. Each run walks from the day after it through today in 2-day
  windows (the yesterday+today shape the pass always pasted), one ask per
  window, and advances the mark after each window, stopping at yesterday
  because today's diary is still being written. A normal day is one window;
  a week of failed nights is a few more asks, each the usual size.
- **Skill-gap-learner `learn`** — `skill-gap-learner.learn`: the date of the
  last completed run. The gap pass reads at least 7 days of diary, stretched
  back to the day after the watermark when runs were missed. Set after the
  curator completes.

A missing or unparsable watermark falls back to the pre-ADR window, so a
fresh install behaves as before.

## Consequences

- One transcript per chore per day. The heartbeat, distiller and
  skill-gap-learner stop dominating `claude --resume`.
- A failed or skipped night is processed by the next successful run, not
  lost. The diary prune still runs from contemplation, after metabolism has
  consumed the days it deletes.
- Two processes resuming the same daily session at the same moment (an
  on-the-fly review firing while `learn` is running) is not serialized. Each
  chore already kills its tmux session at start, which was the same hazard
  before this ADR; a per-key lock is the next step if it bites.
- Context accumulates within a day's session. A long day ends in
  auto-compaction, which the resume path already handles ("Resume from
  summary" picker).
- Loss of `chore_state.db` costs one extra transcript per key and one wider
  catch-up window. It is never backed up.
- The day this lands, each daily-session reminder starts one extra session:
  its row lived in the dropped reminders table and is not carried over.

## Rejected

- **Per-chore tables in per-chore DBs.** Three copies of the same
  bookkeeping; the reminders one already existed and would have been the
  template for two more.
- **One shared daily session across all chores.** Mixing the heartbeat's
  sweep context into the distiller's judgments (and vice versa) trades
  transcript count for cross-contamination.
- **Reading the chore's own diary to find its last success.** The diary is
  prose for the distiller to read, not a ledger; parsing it back is fragile
  and the prune would eventually delete the evidence.
- **A `--since` flag for manual catch-up.** Puts the operator in the loop
  for something the job can know itself.
