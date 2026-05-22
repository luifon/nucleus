# ADR-006 — Reminders as the universal time-triggered notification primitive

**Status:** Proposed (2026-05-15)

## Context

Today's reminder system is two things glued together:

1. **Ad-hoc reminders** — the `reminders` binary's `add` / `list` / `cancel` / `due` subcommands, backed by a single `reminders` table in `memory/reminders.db`. Schema: `id, due_at, body, channel, status, fired_at, fired_msg_id, cancelled_at, created_at`. The `due` subcommand polls every minute via the `reminders-tick` launchd plist (`StartInterval=60`) and fires whatever's due. Works fine.

2. **The daily timesheet reminder** — a `timesheet` subcommand on the same binary with the body hardcoded into the source (`"⏰ End of day — time to log your hours."`), the channel hardcoded (`discord-home`), and the schedule expressed by a *separate* `dev.nucleus.timesheet-reminder` launchd plist that fires `reminders timesheet` at `Hour=18, Minute=30` via `StartCalendarInterval`.

The second thing is wrong. A timesheet reminder is just a reminder — same shape (body, channel, time) — but it's been special-cased into code constants + a parallel launchd plist. That has two concrete costs:

- **Configuration requires a rebuild.** Want to move timesheet from 18:30 to 19:00? Edit the plist *and* rebuild after the user logs out (because launchd caches the plist's TZ at bootstrap time — see [`launchd_tz_pitfall`](../memory…) memory). Want to change the body? Edit Rust source and rebuild.
- **The StartCalendarInterval path is fragile on macOS.** Today, 2026-05-15, the timesheet missed its 18:30 fire entirely because `OS_REASON_CODESIGNING` killed the launch — macOS caches the binary's codesign identity at plist-load time and SIGKILLs the next launch if the binary on disk changed (today's `cargo build` rebuilt `reminders` cascaded from a `nucleus_core` touch). The same plist class also lost the morning fire weeks ago when the launchd user-bootstrap captured the wrong TZ at first login.

The ad-hoc reminder path doesn't have either of those problems (it runs from `StartInterval=60` + program-side wallclock decisions), and it already has all the moving parts — DB-backed schedule, per-row body, per-row channel, idempotent firing. The timesheet should be a *row in that table*, not a separate code path.

Beyond that, the current model has limitations that show up as soon as anything more complex than "remind me once" lands:

- **One channel per reminder.** No way to send a reminder to both Discord and Alfred. The `channel` column is a single string.
- **No recurrence.** Daily / weekly / weekdays-only / monthly all require re-inserting the reminder after each fire.
- **No history.** `last_fired_at` overwrites; no audit of "what fired when over the last 30 days".
- **No pause.** To temporarily disable a recurring nudge (vacation, off week) you'd have to delete it and reinsert it later — losing all its state.

This ADR redesigns the reminders subsystem to subsume all of those — one model, one ticker, one CLI.

## Decision

**A reminder is the universal primitive for time-triggered notifications.** It carries:

- **what** — a body string, the message that gets delivered
- **when** — a cron expression, evaluated in the operator's local timezone (`NUCLEUS_TZ`); for one-shot use a cron pattern that matches a single calendar date plus a `one_shot` flag that prevents re-firing
- **who** — one or more channels (Discord home, the WhatsApp conversational group, the WhatsApp brain-dump group, WhatsApp DM, Calendar)

The `reminders due` polling worker is the single execution engine. The standalone `timesheet` subcommand + `dev.nucleus.timesheet-reminder` plist are removed. Default reminders (timesheet, anything else "this is just how Nucleus operates") get inserted by a seeder on binary startup, idempotently — the same pattern news-fetcher uses for default sources.

## Data model

Three tables, all in `memory/reminders.db`.

### `reminders`

The canonical record of a reminder.

```sql
CREATE TABLE reminders (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    body            TEXT    NOT NULL,
    cron            TEXT    NOT NULL,        -- 5-field standard cron, evaluated in NUCLEUS_TZ
    one_shot        INTEGER NOT NULL DEFAULT 0,
    status          TEXT    NOT NULL DEFAULT 'active',
                                              -- 'active'    — recurring, firing on schedule
                                              -- 'pending'   — one-shot, not yet fired
                                              -- 'fired'     — one-shot, delivered
                                              -- 'paused'    — temporarily disabled; see paused_until
                                              -- 'cancelled' — terminal
    next_fire_at    TEXT,                     -- RFC3339 UTC; denormalized from cron for query efficiency. NULL when status is fired/cancelled.
    last_fired_at   TEXT,
    paused_until    TEXT,                     -- RFC3339 UTC; when non-NULL and now ≥ paused_until, the ticker auto-flips status back to active
    created_at      TEXT    NOT NULL,
    created_by      TEXT    NOT NULL DEFAULT 'user'  -- 'user' | 'system' (seeded); informational only — no special protection
);

CREATE INDEX idx_reminders_next_fire ON reminders(next_fire_at)
    WHERE next_fire_at IS NOT NULL;
```

`status` carries the lifecycle for both kinds. A recurring reminder lives in `active` forever (or until cancelled / paused). A one-shot reminder lives in `pending` until it fires, then transitions to `fired`. The ticker queries `WHERE status IN ('active', 'pending') AND next_fire_at <= now()`.

Why `cron` for everything, including one-shot? Because the alternative was a discriminated `due_at` OR `cron` schema (two mutually-exclusive columns, validation logic in code, two code paths). Standard cron can express any specific minute on any specific date — `30 18 18 5 *` matches "18:30 on May 18 every year". The `one_shot` flag says "fire the next match, then never again". The ticker uses `cron` to compute `next_fire_at`; for one-shot reminders, after firing it sets `next_fire_at = NULL` and `status = 'fired'` instead of recomputing. One field, one execution path.

### `reminder_channels`

Multi-channel delivery with per-channel state.

```sql
CREATE TABLE reminder_channels (
    reminder_id     INTEGER NOT NULL REFERENCES reminders(id) ON DELETE CASCADE,
    channel         TEXT    NOT NULL,        -- 'discord-home' | 'alfred' | 'braindump' | …
    status          TEXT    NOT NULL DEFAULT 'pending',
                                              -- per-FIRE state, reset each time the parent reminder fires
                                              -- 'pending' — not yet attempted this fire
                                              -- 'sent'    — delivered successfully
                                              -- 'failed'  — attempts maxed out
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT,
    last_attempt_at TEXT,
    PRIMARY KEY (reminder_id, channel)
);
```

When a reminder fires, the ticker iterates its `reminder_channels` rows, delivers to each in turn, and records per-channel success/failure. **Channels that fail are retried on the next tick** without re-delivering to channels that already succeeded — this is the "per-channel state, no duplicate delivery" behavior we agreed on. Once all channels reach a terminal state (`sent` or `failed`), the next-fire computation runs (recurring) or the reminder is marked fired (one-shot).

For the NEXT fire, all channel rows reset to `pending`. (Atomic transition: see Lifecycle below.)

### `reminder_fires`

Append-only audit log.

```sql
CREATE TABLE reminder_fires (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    reminder_id     INTEGER NOT NULL REFERENCES reminders(id) ON DELETE CASCADE,
    fired_at        TEXT    NOT NULL,         -- RFC3339 UTC, time the ticker started this fire
    channel         TEXT    NOT NULL,
    success         INTEGER NOT NULL,         -- 0/1
    msg_id          TEXT,                     -- Discord message id / outbound_queue id / …
    error           TEXT                      -- when success = 0
);

CREATE INDEX idx_reminder_fires_at ON reminder_fires(fired_at DESC);
CREATE INDEX idx_reminder_fires_reminder ON reminder_fires(reminder_id, fired_at DESC);
```

One row per (fire, channel) attempt. Powers `reminders history` and any future dashboard widget that wants to surface "did last night's timesheet land?" without joining against Discord. Retained indefinitely — it's tiny.

## Cron crate

After web-searching the current Rust landscape ([croner](https://crates.io/crates/croner), [zslayton/cron](https://github.com/zslayton/cron), [cron-parser](https://crates.io/crates/cron-parser)), **we use `croner`**. Rationale:

- **TZ-aware out of the box** — chrono + chrono-tz integration. Given the TZ saga that motivated this whole redesign, *not* rolling our own TZ handling is the load-bearing requirement.
- **DST-safe.** Brazil doesn't observe DST anymore but operators elsewhere might. Croner handles DST transitions correctly per the OCPS spec.
- **Extended pattern syntax** when we need it — `L` (last day of month), `#` (nth weekday of month), `W` (closest weekday). Useful for "remind me on the last weekday of every month" without writing our own crons.

`zslayton/cron` (the more commonly-cited crate, last updated March 2026) is simpler and slightly more current but defaults to UTC — that's a non-starter for us.

## Timezone semantics

Cron expressions are evaluated in `NUCLEUS_TZ` (default `America/Sao_Paulo`, configurable in `.env`). That's the same TZ env var the rest of the stack already reads. `next_fire_at` and `last_fired_at` are stored as UTC RFC3339 to keep them sortable + comparable to `Utc::now()`, but the cron string itself is interpreted as local time.

Practical: `"30 18 * * 1-5"` means "18:30 BRT, Monday through Friday", which equals 21:30 UTC. The ticker computes `next_fire_at = 21:30 UTC` and stores that.

## Missed-fire policy

**Fire late, always.** If a reminder's `next_fire_at` is in the past — whether by one minute or two days — fire it on the next tick. Apply to both one-shot and recurring. We discussed "recurring skips missed and waits for next occurrence" as an alternative; rejected because:

- It's surprising behavior. The user explicitly set this reminder; missing it silently feels wrong.
- The recurring `last_fired_at` already exposes "yes this is late" downstream if anything wants to act on it (e.g., the body could note "this is from yesterday").

So: one tick, one fire, regardless of how stale `next_fire_at` is. After firing, recompute `next_fire_at` from the cron to the next future match (so we don't backfire every missed occurrence — we deliver one and move on).

## Pause + auto-resume

Pause sets `status = 'paused'` and optionally writes `paused_until = <RFC3339 UTC>`. On each tick, before the fire loop, the ticker runs:

```sql
UPDATE reminders
   SET status = 'active', paused_until = NULL
 WHERE status = 'paused' AND paused_until IS NOT NULL AND paused_until <= ?1;
```

So an indefinite pause stays paused until you unpause manually; a timed pause (`reminders pause #3 --until 2026-05-22T00:00:00-03:00`) flips itself back automatically. Pause/unpause CLI verbs ship in v1.

## Multi-channel delivery flow

Per fire, atomically:

1. Read the reminder's `reminder_channels` rows where `status = 'pending'`.
2. For each channel, deliver via the existing `deliver` function (Discord SDK / WhatsApp outbound_queue). On success, set `status = 'sent'`, `msg_id`, `last_attempt_at`. On failure, increment `attempts`, set `last_error`; if `attempts >= MAX_ATTEMPTS` (3), set `status = 'failed'`. Append a `reminder_fires` row either way.
3. After iterating, check if any rows are still `status = 'pending'`. If yes: leave the reminder's `next_fire_at` unchanged — the next tick retries the still-pending channels.
4. If no rows are pending (all `sent` or `failed`):
   - If `one_shot`: set `status = 'fired'`, `next_fire_at = NULL`, `last_fired_at = now()`.
   - Else: recompute `next_fire_at` from `cron`, set `last_fired_at = now()`, reset all `reminder_channels.status` to `'pending'` and `attempts = 0`.

Wrap each fire in a single SQLite transaction to keep the channel-state reset atomic with the `next_fire_at` advance.

## CLI surface

Unified `add` (handles both one-shot and recurring via mutually-exclusive flags). Single binary, same crate.

```
reminders add  --at <iso>                   --body <…>  --channels <c1,c2,…>   # one-shot
reminders add  --cron "<5-field expr>"      --body <…>  --channels <c1,c2,…>   # recurring
reminders list   [--include-fired] [--include-cancelled]
reminders show   <id>                                                          # full detail incl. channels + recent fires
reminders cancel <id>
reminders pause  <id>  [--until <iso>]
reminders resume <id>
reminders history [--days N] [--channel <c>] [--reminder <id>]                 # queries reminder_fires
reminders due                                                                  # the polling tick (run from launchd)
```

`--channels` accepts a comma-separated list. Validated against the known channel set (`discord-home`, `alfred`, `braindump`) — unknown channels fail fast.

`--at` and `--cron` are mutually exclusive. `--at` implies `one_shot = 1` and produces a cron expression that matches that single date+time (used purely for the ticker's uniform code path).

`list` shows the reminder's next fire time (computed) so it's easy to verify a cron is doing what you meant before it fires.

## Seeding

`seed_default_reminders()` runs at binary startup, idempotently. Initial set (v1):

| body | cron | one_shot | channels | created_by |
|------|------|----------|----------|------------|
| `"⏰ End of day — time to log your hours."` | `"30 18 * * 1-5"` | 0 | `[discord-home]` | `system` |

The seeder checks `WHERE created_by = 'system' AND body = ?` before inserting — so if the row was cancelled, the seeder won't re-create it on next startup (cancelled stays cancelled until you explicitly re-add). If the row was deleted (you wiped the DB), the seeder re-inserts.

Future "system" reminders (weekly review, monthly bookkeeping, whatever) plug into the same seeder.

## Launchd footprint

After this lands, the launchd inventory shrinks by one:

- `dev.nucleus.reminders-tick` (`StartInterval=60`) — **kept.** This is the polling engine.
- `dev.nucleus.timesheet-reminder` (`StartCalendarInterval`) — **deleted** (both `.example` template and the installed plist). Its functionality is a row in `reminders` now.

No `StartCalendarInterval` left in the Nucleus stack. All scheduling decisions happen in code, against `chrono::Local::now()`, with the TZ env var the plist passes in. The launchd-bootstrap-TZ bug and the StartCalendarInterval-codesign-cache bug both become impossible by construction.

## Migration plan

The implementation session should land this in one commit (or two — schema + code, then CLI), in this order:

1. **Schema migrations in `chores/reminders/src/store.rs`:**
   - `ALTER TABLE reminders ADD COLUMN cron TEXT` (nullable for backfill, then validated NOT NULL in app code)
   - `ALTER TABLE reminders ADD COLUMN one_shot INTEGER NOT NULL DEFAULT 0`
   - `ALTER TABLE reminders ADD COLUMN next_fire_at TEXT`
   - `ALTER TABLE reminders ADD COLUMN last_fired_at TEXT`
   - `ALTER TABLE reminders ADD COLUMN paused_until TEXT`
   - `ALTER TABLE reminders ADD COLUMN created_by TEXT NOT NULL DEFAULT 'user'`
   - Drop the obsolete `due_at`, `fired_at`, `fired_msg_id`, `cancelled_at` columns? SQLite can drop columns in modern versions. Or leave them as dead columns and stop writing them — simpler. Recommend leaving them, marked deprecated in a comment.
   - Create `reminder_channels` and `reminder_fires` tables (idempotent `CREATE TABLE IF NOT EXISTS`).
2. **Backfill existing rows:**
   - For each row in `reminders` with old-shape data: derive a cron string from `due_at` (`MM HH DD MO *`), set `one_shot = 1`, copy `due_at` → `next_fire_at`, set `status` based on the old enum, insert a `reminder_channels` row with the existing `channel` and `status = 'pending'` (or `'sent'` if the row was already fired).
3. **Code:**
   - Drop the `Cmd::Timesheet` variant from the CLI.
   - Drop the `timesheet()` function.
   - Wire `croner` into `due()` for cron evaluation and `next_fire_at` computation.
   - Implement the new lifecycle helpers in `store.rs`: `pending_due_with_channels`, `record_channel_fire`, `advance_after_fire`, `auto_resume_paused`.
   - Implement the new CLI verbs.
   - Add `seed_default_reminders()` and call it from `main` like the existing schema bootstrap.
4. **Launchd:**
   - `launchctl bootout gui/$UID/dev.nucleus.timesheet-reminder`
   - `rm ~/Library/LaunchAgents/dev.nucleus.timesheet-reminder.plist`
   - `rm tools/launchd/timesheet-reminder.plist.example`
   - That's it. `reminders-tick.plist` stays untouched.
5. **Build + reload:**
   - `cargo build --release --bin reminders`
   - `launchctl bootout gui/$UID/dev.nucleus.reminders-tick && launchctl bootstrap gui/$UID ~/Library/LaunchAgents/dev.nucleus.reminders-tick.plist` (refreshes the codesign cache for the new binary)
6. **Smoke test:**
   - `reminders list` shows the seeded timesheet with `next_fire_at` set to next weekday 18:30 BRT.
   - `reminders add --at <iso 2 min from now> --body "test"` + observe firing within 2 min.
   - `reminders pause <timesheet_id> --until <iso 1 min from now>` + observe auto-resume after the timestamp.

## What's out of v1 (deferred)

Two things, explicitly:

- **Reminder editing.** Today there's no `reminders edit` — to change a reminder you cancel + re-add. That's acceptable for v1; CLI editing can land later as `reminders edit <id> [--cron …] [--body …] [--channels …]`. Implementation is straightforward but adds CLI surface area we don't need for the migration.
- **Body templates / per-channel body overrides.** Currently the body string is one piece of text and the delivery layer adapts it per channel (e.g., Discord gets the @-mention prefix, Alfred doesn't — that's a delivery-side concern, not a data-model one). If we ever want truly different content per channel, the `reminder_channels` table grows a `body_override TEXT NULLABLE` column. Trivial extension.

Everything else discussed during design — multi-channel, per-channel retry, history log, pause-until, croner, fire-late — ships in v1.

## Consequences

**Positive:**
- One model, one ticker, one CLI for all time-triggered notifications.
- Changing the timesheet time/body/channel becomes `reminders edit` (v2) or a quick SQL update — no rebuild required.
- No more `StartCalendarInterval`-based plists in the stack → immune to the launchd-TZ-bootstrap and codesign-cache failure modes that cost a day this week.
- Multi-channel delivery just works; no per-feature plumbing required to send the same reminder to both Discord and Alfred.
- History log unlocks dashboard surfaces ("did anything fail to deliver last night?") cheaply.
- Pattern is reusable: anything new that wants to fire on a schedule (weekly review reminder, monthly subscription nudge, etc.) is one seeded row, no new code path.

**Negative:**
- Migration is non-trivial — existing reminders rows need backfill, the schema changes are non-additive, and there's exactly one production DB (the operator's own `memory/reminders.db`). The migration session must be careful with the existing data; back up `reminders.db` before running.
- The data model is more complex than today's flat table — three tables instead of one, two enum-like status columns, denormalized `next_fire_at`. Worth it for what it buys but worth acknowledging.
- `croner` adds one dependency to the `reminders` crate. Small, no transitive bloat, but it's a dep.
- We lose the very specific guarantee "the timesheet plist's schedule lives in launchd's calendar interval logic." Some operators might find that more familiar. Counter: that "familiar" path is what burned today.

## Sources

- [croner — crates.io](https://crates.io/crates/croner)
- [zslayton/cron — github](https://github.com/zslayton/cron)
- [Hexagon/croner-rust — github](https://github.com/Hexagon/croner-rust)
- [Building a Task Scheduler with Cron Expressions in Rust — OneUptime, Jan 2026](https://oneuptime.com/blog/post/2026-01-25-task-scheduler-cron-expressions-rust/view)
