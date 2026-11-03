---
name: bot-triage
description: >
  Diagnose and recover a Nucleus bot that went silent, answered weirdly, or
  whose fire failed — "the WhatsApp bot isn't responding", "reminder didn't
  arrive", "it ignored my message", "the session looks stuck" — and a WhatsApp
  connection that keeps dropping (repeated boots, launchd crash-loops, Baileys
  405 loops, stream conflicts, a connected bot that no longer replies). Walks
  the layers in order (tmux session state → delivery DBs → logs → recovery
  ladder → connection triage) instead of guessing. Use for ANY
  unresponsive/odd agent behavior, even when the operator doesn't say "triage".
flavor: recipe
trigger: model
mcp_needed: []
last_used: 2026-09-22
last_failure: null
failure_count_30d: 0
notify_on_failure: []
---

# bot-triage — why is the bot silent?

# When to invoke

- The operator says a bot is silent, answered strangely, or ignored a
  message; a reminder or other delivery did not arrive; a session looks
  stuck.
- The WhatsApp agent's diary (`memory/diaries/whatsapp/<date>.md`) shows
  many `boot` entries in one day. A healthy day has 1–2; 5 or more means
  churn → layer 5.
- The WhatsApp bot is reported offline, does not answer DMs, or reminders
  queued to `whatsapp-dm` do not arrive.
- After a fix elsewhere (env change, dependency bump, plist edit), to confirm
  that the WhatsApp connection stays up.

# Steps

Work the layers IN ORDER; each one exonerates or convicts before moving on.
Before any restart, capture the state that identifies the failure class
(`ps -o lstart,etime -p <pid>`, `lsof -a -p <pid> -iTCP`, `tmux ls`, the
session and queue tables). A restart destroys that evidence.

## 1. Session state (tmux)

```bash
tmux ls
tmux list-windows -t <session>            # live claude = any non-zsh window
tmux capture-pane -t <session>:<win> -p -S -100
```

Read the pane bottom-up and classify:

- **Interactive dialog / picker** (`❯ 1. …` options, "wants to use", install
  prompts): the session is WEDGED on a prompt no one will ever answer.
  READ the dialog before acting — answering blindly can auto-approve a
  permission. Send the safe option (`tmux send-keys -t <t> "2"` etc.).
- **Unsent draft in the live input row** — the LAST `❯` row on screen,
  inside the bottom `────` box, holding text: submits are failing (operator
  messages pile up invisibly). ❯ rows ABOVE that are history — submitted
  messages re-render with a ❯ prefix; do not confuse them.
- **Clean idle prompt** but operator says silent → the session is innocent;
  go to layer 2.

## 2. Delivery layer (the DBs tell the truth)

```bash
sqlite3 memory/whatsapp.db "SELECT id, status, sent_at IS NOT NULL, source,
  substr(body,1,60) FROM outbound_queue ORDER BY id DESC LIMIT 5;"
sqlite3 memory/reminders.db "SELECT reminder_id, fired_at, channel, success,
  COALESCE(error,'-') FROM reminder_fires ORDER BY id DESC LIMIT 5;"
sqlite3 memory/agent_messages.db "SELECT at, sender, target, delivered,
  COALESCE(error,'-') FROM agent_messages ORDER BY id DESC LIMIT 5;"
```

WhatsApp conversational turns (ADR-033) — where did the operator's message go?

```bash
sqlite3 memory/whatsapp.db "SELECT ref, status, received_at, typed_at, turn_id,
  COALESCE(error,'-') FROM chat_inbound ORDER BY received_at DESC LIMIT 5;"
sqlite3 memory/whatsapp.db "SELECT substr(id,1,8), kind, status, started_at, ended_at,
  ack_sent, progress_count, COALESCE(error,'-') FROM chat_turns
  ORDER BY started_at DESC LIMIT 5;"
./target/release/nucleus tasks list        # background tasks (ADR-033)
```

A message stuck at `received` was never typed (session spawn failing — see
the log); `typed` but no turn means the session has not read it yet (a long
turn is running, or the pane is on a prompt); a `running` turn hours old is
stuck on something on screen (layer 1). The engine sends one ack after 30s
and replies only when the turn ends — silence with a `running` turn is
normal while the work runs.

`status='pending'` piling up → the bot process is down or disconnected
(layer 5 for WhatsApp). `error` columns name the failure. A fire with
`success=1` that the operator never saw → look at the message body
(preamble/marker issues), not delivery.

## 3. Process layer

```bash
launchctl list | grep dev.nucleus          # '-' PID = not running
tail -20 memory/<agent>.log                # WhatsApp: look for "connected"
```

- The string `Not logged in` / `/login` anywhere in a session or log =
  **Claude auth expiry** — every autonomous fire silently no-ops until the
  operator re-logs-in. Treat as an alert, never as content.
- A nonzero last-exit status for the WhatsApp service, or many boots or
  reconnects in its diary → layer 5.

## 4. Recovery ladder (least → most destructive)

1. **Answer the dialog** (wedge class 1) and let the turn finish.
2. **Kill the window**: `tmux kill-window -t <session>:<win>` — the pool
   detects the dead window on next ask and respawns with `--resume`
   (context survives).
3. **Restart the service**: `launchctl kickstart -k gui/$(id -u)/dev.nucleus.<agent>`
   — code changes only. Plist/env changes need `bootout` + `bootstrap`
   instead (kickstart does NOT reread the plist).
4. Verify recovery in the LOG (e.g. "whatsapp: connected"), then confirm
   end-to-end in the failing path itself — an operator-visible symptom is
   only fixed when the operator's path works, not when local checks pass.

## 5. WhatsApp connection triage

Use this layer when the WhatsApp connection itself is unhealthy: the
process or the link drops, so nothing queued to `whatsapp-dm` moves. A
failed reminder fire with a healthy connection is not this layer.

1. **Establish the churn shape.** Count `boot` and `reconnect` entries per
   day in `memory/diaries/whatsapp/<date>.md`. Read the close reasons that
   the connection breaker (ADR-027) records:

   ```bash
   sqlite3 memory/whatsapp.db "SELECT ts, class, code, uptime_ms,
     substr(COALESCE(detail,'-'),1,120) FROM connection_events
     ORDER BY id DESC LIMIT 20;"
   tail -50 memory/whatsapp.log
   tail -50 memory/whatsapp-baileys.log
   ```

   Note whether the boots cluster (crash-loop) or are spread across the day
   (network drops, conflicts). The breaker reconnects inside the process for
   reconnectable classes; after 10 failed reconnects in 15 minutes it opens
   the circuit, keeps outbound rows `pending`, alerts once on `discord-home`
   and probes every 5 minutes.
2. **Classify before touching anything:**
   - **launchd crash-loop** — the process exits seconds after start and
     `KeepAlive` respawns it. The log shows a stack trace or an immediate
     exit; `launchctl list | grep whatsapp` shows a nonzero last-exit status.
   - **Baileys 405 loop** — the connection retries with 405s. It almost
     always means the connection setup no longer matches the known-good
     recipe (CLAUDE.md Rule 8): `fetchLatestWaWebVersion({})` (never the
     Baileys-shipped version) and `browser: Browsers.macOS('Chrome')`.
     Restore the recipe; do not tune retry timing.
   - **Stream conflict (440 / replaced)** — another client uses the same
     auth state (a helper script that did not close, or two bot instances).
     Find and end the other holder; helper scripts close with
     `sock.end(undefined)`.
   - **Auth expiry / logged out** — the close reason is `loggedOut`. Only the
     operator can fix this, by pairing again with a QR code. Report it; do not
     attempt automated recovery.
   - **Environment gap** — works when run from a shell, crash-loops under
     launchd → `PATH` or `NUCLEUS_CLAUDE_BIN` missing from the plist's
     `EnvironmentVariables` (launchd inherits no shell env, Rule 5).
   - **Handler wedged — connected but NOT processing** (seen 2026-08-14/15,
     about 18 h before anyone noticed). Every liveness signal reads healthy:
     PID alive, reconnects logged normally, outbound queue empty, launchd
     status 0 — yet inbound DMs produce nothing. Signature in
     `memory/whatsapp.db`: `chat_sessions.last_active` for the chat stays at
     the time of the last *successful* reply, and the diary has **no `dm`
     entries** for the period. A `dm` line is written only after a reply is
     produced, so no `dm` lines means no reply came back; it does NOT prove
     that no message arrived. Layer 2's `chat_inbound` and `chat_turns` show
     how far each message got. The risk grows with uptime; the 04:00
     `bot-restart` launchd job limits it. Fix: `bootout` + `bootstrap` the
     service, then ask the operator to send one real inbound message. Only he
     can send it, and it is the only conclusive test.
   - **Benign** — boots match intentional restarts (deploys, `bootout` /
     `bootstrap` cycles, laptop sleep and wake, the 04:00 `bot-restart`
     job). No action; say so.
3. **Apply the class-specific fix only.** Do not bundle: a 405 loop does not
   need a plist edit, a crash-loop does not need the Baileys recipe changed.
4. **Reload correctly** — layer 4 step 3: plist or env changes need
   `bootout` + `bootstrap`.
5. **Verify in the failing path.** Watch `memory/whatsapp.log` and
   `connection_events` for a stable connection over several minutes with no
   new boot entries, and confirm the outbound queue drains (a pending
   reminder delivery is a good probe). One successful boot does not show
   stability.

# Failure modes

- **Diagnosing from the wrong layer:** "bot ignored me" has four distinct
  causes (wedged TUI, dead process, delivery failure, missing context) with
  different fixes — skipping layers leads to restarting things that were
  innocent and losing session state for nothing.
- **Restarting before capturing state** — a restart destroys the evidence
  that identifies the failure class (process age, socket set, tmux windows,
  in-memory pool). Capture first, then restart. Restarting the service is
  cheap; recovering the diagnosis afterwards is not possible.
- **Inferring a root cause from one observation without a baseline** — "two
  sockets to Meta = stream conflict" was wrong: a freshly booted worker
  holds the same two. Before calling an observation abnormal, compare it
  with a known-good instance. Say "I don't know yet" when the evidence does
  not support a cause.
- **Reading uptime as health, or liveness as function** — "PID alive +
  queue empty + reconnects logged + launchd status 0" is exactly what the
  wedged-handler class looks like. Liveness checks prove that the process
  exists, not that it still does its job; verify the function (a real
  message, a real fire, a real HTTP route).
- **Killing a window mid-turn** loses that turn's un-persisted work; prefer
  letting a dialog-answered turn finish first.
- **kickstart after env/plist edits** silently keeps old env — the classic
  "restarted but nothing changed" trap, and the problem returns hours
  later; use bootout + bootstrap.
- **Mistaking history ❯ rows for a stuck draft** (or vice versa) — only the
  LAST ❯ row is the live input.
- **Declaring victory from a capture-pane** — always re-verify through the
  channel the operator actually uses (their WhatsApp DM, the fire, etc.).
- **Calling `sock.logout()` anywhere** — it unlinks the device and forces a
  new QR pairing, which only the operator can do. Close helper connections
  with `sock.end(undefined)`, always.
- **Attempting automated re-auth on `loggedOut`** — uses up retries and can
  make the auth state worse. Report to the operator at once with the
  disconnect reason.
- **Declaring the connection fixed after one clean boot** — churn is
  intermittent; verify over a period of time, in the launchd-spawned
  process, not in an interactive shell run.
- **Reading benign restarts as a crash-loop** — compare boot times with
  deploys, the 04:00 restart and laptop sleep before changing anything.
- **Restarting the WhatsApp bot while the outbound queue is sending** — can
  delay or duplicate a reminder delivery; when the restart is optional,
  check that the queue is idle first.
