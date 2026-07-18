# ADR-026 — Heartbeat: a standing "does anything need attention?" turn

Date: 2026-07-18
Status: accepted + built (2026-07-18)

> As-built: `is_silent_reply` gate in the reminders worker (unit-tested;
> suppression records `<msg_id>|silent` fires), heartbeat seeded as the
> system reminder titled `heartbeat` (title-matched seeding so prompt
> wording can evolve; cancellation stays sticky), checklist at
> `4-Areas/Nucleus/HEARTBEAT.md` in the vault.
>
> **Live verification (2026-07-18 evening):** first two fires wedged on a
> transient tool-execution stall (coincided with a Claude Code
> plugin-update window; every component — Bash, vault Read, parallel
> batches, the full sweep — passed isolated probes minutes later). The
> retry machinery absorbed it exactly as designed: full error chains
> persisted, silent retries, zero operator noise. Third attempt ran the
> full sweep (2 file reads, 9 read-only commands, ~2 min) and correctly
> DELIVERED a one-time report of the day's genuinely-failed fires — the
> report path is verified end-to-end. The `HEARTBEAT_OK` suppression
> path is unit-tested; first live observation expected on the next
> quiet fire.

## Context

Every Nucleus automation is task-shaped: a cron that does one thing. There
is no standing primitive for open-ended vigilance — the operator-curated
"keep an eye on these" list that a human assistant would sweep
periodically. OpenClaw ships this as a first-class **Heartbeat** distinct
from cron: a periodic agent turn reads a `HEARTBEAT.md` checklist, replies
`HEARTBEAT_OK` when nothing needs attention (suppressed, no message), and
only surfaces when something does. The value is the *suppression contract*
plus the *operator-editable checklist* — not the scheduling, which cron
already covers.

Nucleus can build this almost entirely from existing parts (ADR-006/008
reminders); what's missing is reply-gated delivery.

## Decision

1. **Reply-gated delivery in the reminders worker.** A skill-fire whose
   session reply is exactly `HEARTBEAT_OK` (modulo whitespace) records a
   successful fire and delivers nothing. General mechanism, not
   heartbeat-specific — any skill-fire gains "stay silent when there is
   nothing to say" (several existing fires fake this today with awkward
   prompt contortions).
2. **A checklist note owned by the operator** at
   `4-Areas/Nucleus/HEARTBEAT.md` in the vault (T3 — it is curated prose,
   not config). Free-form markdown: items, per-item cadence hints,
   whatever the operator wants the sweep to read.
3. **A system-seeded heartbeat reminder** (same seeding mechanism as the
   18:30 timesheet, `created_by='system'`, cancellation sticky):
   cron `*/30 9-23 * * *` in `NUCLEUS_TZ` — active hours only, no
   overnight fires — with a system prompt of: read `HEARTBEAT.md`, check
   each item cheaply (Bash/reads only; no outbound actions without the
   Rule 6 gates, exactly as any fire), report only what crossed a
   threshold; otherwise `HEARTBEAT_OK`. Channel: `whatsapp-dm`.
4. **Condition-watcher synergy (ADR-024):** once watchers land, the
   heartbeat's cadence can drop — mechanical checks migrate to cheap
   gated reminders, and heartbeat keeps only the judgment-shaped items.
   The two are complementary, not redundant: watchers are per-signal and
   scripted; heartbeat is holistic and read-from-prose.

Deliberately NOT adopted from OpenClaw: `skipWhenBusy` (Nucleus fires run
in their own one-shot sessions — there is no shared main session to be
busy) and a separate heartbeat scheduler (the reminders tick is the one
scheduler; the cleanup-over-parallel rule).

## Consequences

- Cost: up to ~30 session spawns/day at the seeded cadence. That is real;
  the cadence is a config knob and the expectation is it *drops* as
  ADR-024 absorbs the mechanical items. Start at `*/30`, tune down.
- `HEARTBEAT_OK` suppression is a new reply contract — documented in the
  fire persona, tested in the worker (marker-strip-style unit tests).
- The checklist lives in the vault, so editing what the heartbeat watches
  is a note edit, no deploy — and the note's history is the audit of what
  the operator ever cared about.
- Risk of alert fatigue inverts: a badly-written checklist item that
  always "needs attention" spams every 30 min. Mitigation: the seeded
  prompt instructs once-per-day max per item unless state changed
  (the fire session reads its own diary of prior heartbeat reports).
