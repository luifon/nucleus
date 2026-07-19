# ADR-027 — Channel adapter circuit breaker (WhatsApp first)

Date: 2026-07-18
Status: accepted + built (2026-07-19)

> **As-built** (`messaging/whatsapp/src/breaker.ts` + `connection_events`
> in whatsapp.db + `[whatsapp.breaker]` toml knobs over defaults):
>
> - Pure clock-injected state machine, 9 unit tests. Ladder
>   1s→2s→5s→15s→60s→300s per consecutive quick failure; a ≥2 min
>   stable connection resets it; 10 failures in 15 min open the circuit
>   (single Discord alert, 5 min half-open probes); recovery clears the
>   failure window but keeps the ladder rung (a probe that dies in
>   seconds backs off long instead of re-alerting), and only a stable
>   stretch resets the rung — a subtlety the first test draft caught.
> - One deviation from the sketch below: `loggedOut` HOLDS instead of
>   exiting — exiting would make launchd crash-loop a state only the
>   operator can fix (re-pair). The bot stays alive, queue durable,
>   single 🚨 alert.
> - `connect()` failures that never reach a connection event are fed
>   back into the breaker so silent spawn failures still count.
> - Every close records (ts, class, code, uptime) — the churn-diagnosis
>   dataset, passive from day one. Deployed 2026-07-19; a monitor is
>   watching for the first natural churn event (baseline 6–9/day) to
>   confirm classification in production.

## Context

The WhatsApp bot has boot-cycled 6–9×/day since June (W24) — the known
churn baseline. The current posture is launchd's blunt one: process exits,
launchd respawns, repeat forever, at whatever rhythm the failure dictates.
Consequences: diary noise that buries real signals, reconnect storms that
can worsen upstream throttling (Baileys 405 loops), and zero structured
evidence about WHY each cycle happened — which is exactly why the churn
diagnosis has stayed pending for a month. Hermes wires a circuit breaker
into every platform adapter (auto-pause failing adapters); that is the
right shape, and building it produces the diagnosis data as a side effect.

## Decision

An in-process connection supervisor in `messaging/whatsapp` (pattern
generalizes to any future adapter; Discord's serenity already has sane
internal retry and is out of scope):

1. **Close-reason taxonomy first.** Every disconnect is classified
   (Baileys `DisconnectReason`, HTTP status, stream error code, socket
   errno) and recorded as a structured row in the bot's DB
   (`connection_events`: ts, class, code, uptime-before-close). This is
   the missing churn-diagnosis dataset, populated passively from day one.
2. **Backoff ladder in-process** instead of exit-and-respawn for
   reconnectable classes: exponential with jitter (1 s → 2 → 5 → 15 → 60,
   cap 5 min). Process exit remains for non-reconnectable classes only
   (`loggedOut` — device unlinked, operator action required) and for
   crashes (launchd stays the outer supervisor; the breaker is the inner
   one — supervision layers compose, they don't replace each other).
3. **Open circuit** after N failed reconnects in a window (default 10 in
   15 min): stop hammering, hold the outbound queue (rows simply stay
   `pending` — the drain loop already tolerates this), alert ONCE via
   `discord-home` (the independent channel; alerting through the broken
   one is a design error), and probe half-open every 5 min.
4. **Closed-circuit recovery** posts a single "back up after Xm, N queued
   messages flushed" observation to the diary — not to the operator,
   unless the outage exceeded 30 min.
5. **Never** touch auth state from the breaker: no re-pair, no
   `sock.logout()` (Rule 8), no auth-dir writes. The breaker manages the
   socket lifecycle only.

Config in `nucleus.toml` (`[whatsapp.breaker]`: thresholds, windows,
caps) — behavior toggles, identical for every clone, per the env-vs-toml
policy.

## Consequences

- Churn stops being invisible: `connection_events` turns "it reboots a
  lot" into a queryable distribution (which classes, what times, what
  uptimes) — the standing diagnosis action finally gets its data.
- Diary boot-noise drops to genuine process starts; the W2x churn-note
  series in the vault gets its closing entry.
- The daily 4 am restart and deploy restarts are unaffected (clean exits
  don't count against the breaker).
- Risk: a bug in the in-process ladder could keep a zombie process
  "connected" to nothing. Mitigation: the breaker feeds the existing
  healthcheck (ADR-020) — open circuit > 30 min flips the health probe
  red, and launchd's outer supervision still catches process death.
- Verification per the standing rule happens in the failing path: chaos
  test by dropping the network (pf rule / Wi-Fi off) and watching the
  ladder, the open-circuit alert, and the queue flush on recovery.
