# ADR-024 — Condition watchers: gate reminder fires on cheap local checks

Date: 2026-07-18
Status: accepted + built (2026-07-18)

> **As-built deviations from the text below:**
>
> 1. Gated ticks are NOT recorded as `reminder_fires` rows — a `*/5`
>    watcher would write 288 audit rows/day of pure noise. Instead the
>    last evaluation is stored in place on the reminder
>    (`condition_state` + `condition_checked_at`, shown by
>    `reminders show`); fires rows appear only for actual fires and for
>    broken watches (`channel='condition'`, success=0).
> 2. Gated CRON ticks advance to the next cron match (the missed
>    occurrence is skipped, not fire-late'd); gated ONE-SHOTS stay due
>    and re-evaluate every tick — "fire as soon as X", which subsumes
>    the on-exit use case.
> 3. Broken-watch policy (spawn failure / 5s timeout): recorded as a
>    failure; cron advances to its next match, a one-shot is PAUSED so
>    it can't re-fail every minute forever — the operator fixes the
>    script and resumes.
>
> Verified live through the launchd tick (2026-07-18): a flag-file
> watcher evaluated false (state recorded, zero fires, one-shot kept
> watching), fired exactly one tick after the flag appeared, and the
> delivered message carried the watcher's `{"context": …}` evidence.
> Decision + eval logic unit-tested (modes × states × timeout).

## Context

A `--system-prompt` reminder fire costs a tmux + Claude session spawn every
time, so recurring skill-fires are scheduled coarsely (daily, weekday
slots) even when the underlying question is "did anything change?". The
things worth watching — a queue depth, a file's mtime, a service's health,
an inbox count — are checkable in milliseconds by a script; only the
*reaction* needs a model. OpenClaw ships this split as cron "condition
watchers" (headless script gates that only fire the payload on
`fire: true`) plus an `on-exit` schedule kind, and it is the single
cheapest idea in their scheduler.

## Decision

Extend the reminders primitive (ADR-006/008) with an optional gate:

```
reminders add --cron "*/5 * * * *" \
  --condition "tools/checks/outbound-stuck.sh" \
  --system-prompt "Outbound queue is stuck; diagnose per the triage runbook." \
  --channels discord-home
```

Semantics:

- The condition runs at each matching tick, under a hard timeout (5 s) and
  the tick's file lock. It is a plain executable: **exit 0 = fire**,
  non-zero = skip silently (recorded in `reminder_fires` as a `gated` row,
  success-neutral — gating is not failure and must not consume retry
  budget or trigger ⚠ alerts).
- Optional stdout JSON `{"context": "..."}` on fire: appended to the
  body/system-prompt so the session starts with the watcher's evidence
  ("queue depth 14, oldest 22 min") instead of re-deriving it.
- **Fire-on-change mode** (`--condition-mode change`, default `while-true`):
  the tick stores the last exit state per reminder and fires only on a
  false→true transition — a persistently-true condition alerts once, not
  every 5 minutes.
- `--condition` composes with both `--body` and `--system-prompt`
  reminders; cron and `--at` alike.
- Condition scripts live wherever the operator likes; repo-shipped checks
  go under `tools/checks/` (committed, identifier-free per Rule 1).

Explicitly not adopted: OpenClaw's separate `on-exit` schedule kind. A
watcher script that tests "is PID/marker gone?" on a 1-minute cron covers
the use case without a new schedule type in the model.

## Consequences

- High-frequency vigilance becomes affordable: a `*/1` or `*/5` cron with
  a condition costs a subprocess per tick, a session only on change. The
  reminders binary stays the single scheduler (no parallel watcher daemon
  — the cleanup-over-parallel rule).
- `reminder_fires` gains the `gated` outcome; `reminders history` and the
  dashboard DTOs learn to render it (ts-rs regen per Rule 12).
- New failure mode: a hung condition script must not stall the tick — the
  5 s timeout kills it and records `condition-timeout` as an error (that
  one DOES count as a failure, since the watch is broken).
- The 60 s tick grain bounds condition frequency; that is accepted — this
  is a scheduler refinement, not an event bus (no inotify, no webhooks;
  those stay out per ADR-021's non-goals posture).
