# ADR-027 — Channel adapter circuit breaker (WhatsApp first)

Date: 2026-07-18
Status: accepted + built (2026-07-19); amended 2026-09-24 (link-gated
drain, retry support, version cache, key material in logs — see the
amendment at the end)

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

## Amendment — link-gated drain, retry support, version cache (2026-09)

### Findings

`connection_events` and the logs from 2026-07-20 to 2026-09-24 show that
most closes are network closes (428) and server closes (503), and that the
breaker reconnects in under 10 s. The damage the operator saw came from the
bot's own behavior during and after those closes:

- The outbound drain kept running while the link was down. Each tick sent
  the oldest row, the send failed with "Connection Closed", and the row used
  one of its 5 attempts. A row failed in about 5 s, and 5 consecutive
  failures exited the process. Three reminders were lost this way.
- The socket had no `getMessage` callback. When a recipient device could not
  decrypt a message and sent a retry receipt, Baileys could not encrypt the
  message again, and the recipient showed "waiting for this message". The
  retry counter cache was per socket, so it restarted at zero after every
  reconnect.
- When the version request to web.whatsapp.com failed,
  `fetchLatestWaWebVersion` returned the version bundled with Baileys. Every
  405 login failure happened on that bundled version.
- libsignal printed each Signal session it closed or opened
  (`console.info("Closing session:", session)`), including the ratchet
  private key, root key and chain keys, and stdout is `memory/whatsapp.log`.
- The Baileys logger was silent, so retry receipts and stream errors were not
  recorded, and `connection_events` stored only the status code.

### Decision

1. **Close detail.** `connection_events.detail` (added by
   `addColumnsIfMissing`) stores JSON `{message, data}` of
   `lastDisconnect.error` (`describeDisconnect` in `breaker.ts`): the Boom
   message and the stream-error node or other cause, byte values summarized
   by length, at most 2000 characters.
2. **Baileys log.** The Baileys logger writes to
   `memory/whatsapp-baileys.log` at level `NUCLEUS_BAILEYS_LOG` (default
   `info`: retry receipts, stream errors, pre-key uploads). Every record
   passes through `redactKeyMaterial` (`key_redaction.ts`). The newsyslog
   policy for `memory/*.log` rotates it.
3. **Link-gated drain** (`outbound_drain.ts`). The drain sends only while
   the link is open: `linkUp()` when the connection opens and the allowlist
   is resolved, `linkDown()` on close. On close the bot also clears the drain
   timer and `liveSock`; the timer starts again on the next open. A send that
   fails with a link error (`isLinkError`: connection closed or lost, status
   408/428/503/515), or that was pending when the link closed, returns the
   row to `pending` with the same message id and does not count an attempt
   (`markLinkLost`). A timeout during which the link closed does not count an
   attempt either. The ADR-033 idempotency is unchanged: a row keeps its
   `msg_id` for every attempt, an `in_flight` row is not sent again while its
   send can still succeed, and a server acknowledgement marks it sent.
   Connection rot (the process exit for a socket that reports open and does
   not work) now needs 5 consecutive link failures or timeouts while the
   link reports open, over at least 30 s; a close resets the count. Errors
   that are not link errors still count attempts and fail the row after 5.
4. **Live socket only.** Presence updates and media re-upload requests use
   the socket of the current connection (`livePresence`, `liveReupload`),
   not the socket a handler was created with. Every message goes through the
   outbound queue (ADR-033), so no message send uses an old socket.
5. **Retry support.** Every message the bot sends is stored by message id in
   `sent_messages(id, jid, proto, sent_at)` in whatsapp.db
   (`sent_store.ts`; `send.ts` stores its message too), kept 7 days and
   pruned at boot and daily. The socket gets
   `getMessage: key => sent.get(key.id)` (undefined when not stored) and one
   process-level `msgRetryCounterCache`, so retry counts survive reconnects.
6. **Version cache** (`wa_version.ts`). A successful version fetch is kept in
   memory and in `memory/whatsapp-wa-version.json`. A failed fetch uses the
   version in memory, then the one on disk; the bundled version is used only
   when no version was ever fetched on the machine. `send.ts`, `check.ts`
   and `list-groups.ts` use the same function. Rule 8 still holds: the
   version comes from `fetchLatestWaWebVersion({})`, with
   `Browsers.macOS("Chrome")`.
7. **No key material in logs.** `installConsoleKeyFilter()` runs at process
   start in the bot and in the one-shot scripts. A libsignal session line
   keeps its text and loses the session object ("Closing session: [signal
   session state redacted]"); in any other logged object the values of
   key-named fields are replaced. `scripts/redact-signal-logs.mjs` removes
   the key values from log files written before the filter (in place, with a
   `--dry-run` mode that reports counts).
8. **Baileys 7.0.0-rc14** (pinned), from rc11. rc12 fixes GHSA-qvv5-jq5g-4cgg
   (message and app-state spoofing through protocol messages), rc13 fixes a
   regression of that fix for the account's own protocol messages, rc14 adds
   an Android browser option, a profile-picture token fix and a newer bundled
   WA Web version. No API used here changed.

### Verification

Unit tests: drain gating with a fake socket (no send before `linkUp`, no
attempt used by an outage, same message id after the link returns, many
outages without an exit, a pending send whose link closed, a timeout during
a close, rot exit on a link that reports open, message errors still
counted); `sent_store` (round trip through `getMessage`, miss, prune);
version cache (fetched, memory, disk, bundled, corrupt file); key redaction
(a real libsignal `closeSession`, console arguments, the Baileys log file);
`describeDisconnect`; the `detail` migration; the log redaction script on
synthetic logs (dry run, in-place rewrite with the same inode, idempotence).

After deploy, with `$T` the deploy time:

```sql
-- memory/whatsapp.db
select code, detail, count(*) from connection_events where ts > $T group by 1, 2;
-- no 405 closes
select count(*) from connection_events where ts > $T and code = 405;
-- no queue row failed by a closed connection
select count(*) from outbound_queue
 where status = 'failed' and enqueued_at > $T and last_error like '%Connection Closed%';
```

In the logs, no "WhatsApp bot exiting: … consecutive failed/hung
sendMessage calls" line falls between the closes of a 408 chain, and
`memory/whatsapp-baileys.log` shows "sent retry receipt" and "stream errored
out" records. After the redaction script has run,
`grep -cE "(privKey|rootKey): <Buffer" memory/whatsapp.log*` prints 0 for
every file, and stays 0.
