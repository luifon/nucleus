---
name: bot-triage
description: >
  Diagnose and recover a Nucleus bot that went silent, answered weirdly, or
  whose fire failed — "the WhatsApp bot isn't responding", "reminder didn't
  arrive", "it ignored my message", "the session looks stuck". Walks the
  layers in order (tmux session state → delivery DBs → logs → recovery
  ladder) instead of guessing. Use for ANY unresponsive/odd agent behavior,
  even when the operator doesn't say "triage".
flavor: recipe
trigger: model
mcp_needed: []
last_used: null
last_failure: null
failure_count_30d: 0
notify_on_failure: []
---

# bot-triage — why is the bot silent?

Work the layers IN ORDER; each one exonerates or convicts before moving on.

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

`status='pending'` piling up → the bot process is down or disconnected.
`error` columns name the failure. A fire with `success=1` that the operator
never saw → look at the message body (preamble/marker issues), not delivery.

## 3. Process layer

```bash
launchctl list | grep dev.nucleus          # '-' PID = not running
tail -20 memory/<agent>.log                # WhatsApp: look for "connected"
```

- The string `Not logged in` / `/login` anywhere in a session or log =
  **Claude auth expiry** — every autonomous fire silently no-ops until the
  operator re-logs-in. Treat as an alert, never as content.

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

# Failure modes

- **Diagnosing from the wrong layer:** "bot ignored me" has four distinct
  causes (wedged TUI, dead process, delivery failure, missing context) with
  different fixes — skipping layers leads to restarting things that were
  innocent and losing session state for nothing.
- **Killing a window mid-turn** loses that turn's un-persisted work; prefer
  letting a dialog-answered turn finish first.
- **kickstart after env/plist edits** silently keeps old env — the classic
  "restarted but nothing changed" trap; use bootout + bootstrap.
- **Mistaking history ❯ rows for a stuck draft** (or vice versa) — only the
  LAST ❯ row is the live input.
- **Declaring victory from a capture-pane** — always re-verify through the
  channel the operator actually uses (their WhatsApp DM, the fire, etc.).
