---
name: session-handoff
description: >
  Hand a topic, brief, or task to another live Nucleus agent session — "continue
  this on WhatsApp", "brief the bot about X", "tell the discord session…",
  "have it message me". Uses the ADR-021 session-send primitive (attributed,
  idle-gated, verified-submit, logged), the spawn-watcher pattern for sessions
  that don't exist yet, and the outbound queue for proactive messages to the
  operator. Use whenever a conversation or context needs to move from THIS
  session into a venue bot's session, even if "handoff" isn't said.
flavor: recipe
trigger: model
mcp_needed: []
last_used: null
last_failure: null
failure_count_30d: 0
notify_on_failure: []
---

# session-handoff — moving a conversation between agents

## The primitive

```bash
./target/release/nucleus session-send \
  --to <tmux-session>[:<window>] --from <your-agent-label> \
  [--await-reply --timeout <secs>] \
  --message "<the brief>"
```

- `--to` must belong to a registered agent (`agents.toml`), exact or as
  `<registered>-suffix`.
- **WhatsApp DM: always `--to whatsapp-dm`** (ADR-033). The message is queued
  for the bot's turn engine, which types it into the operator's DM session and
  spawns or resumes that session when none is live. Raw tmux targets in
  `nucleus-whatsapp` / `nucleus-whatsapp-dm` are refused, because the engine
  owns typing into those panes. `--await-reply` is not available on this route.
- The message is typed inside a code-owned envelope: the
  `[agent-msg from:… at:… hop:…]` header, a line saying it is not from the
  operator, and every line of your message prefixed with `│ `. Never write a
  header yourself.
- The sender and the hop are derived (ADR-021 amendment): a Nucleus session
  sends as its own agent (`NUCLEUS_AGENT`); a different `--from` is refused.
  From the operator's own session use `--from main`. A session whose current
  turn read an agent message cannot send onward, whatever `--hop` says.
- Every send is logged in `memory/agent_messages.db` (audit: who told whom
  what, delivered or not).

## Procedure

1. **Compose a self-contained brief** (the target has none of your
   context): decisions made, constraints, what to do next. Include guardrails
   the target can't infer ("browser is held by the main session — WebSearch
   only").
2. **WhatsApp DM:**

   ```bash
   ./target/release/nucleus session-send --to whatsapp-dm --from main --message '<brief>'
   ```

   The bot types it within a few seconds, spawning or resuming the DM session
   if needed. The session's reply to it is not sent to WhatsApp.
3. **Other venues (Discord, chat):** find a live window with `tmux ls` and
   `tmux list-windows -t <session>` (a live claude window is any window not
   named `zsh`), then `session-send --to <session>:<window>`.

4. **Work that should run on its own** ("research X and report back"): start
   a background task with `--origin whatsapp-dm` instead of briefing the chat
   session. The result goes to the operator's DM and into the DM session as
   context:

   ```bash
   ./target/release/nucleus tasks start --origin whatsapp-dm --requested-by operator \
     --title "<short title>" --brief - <<'EOF'
   <full brief>
   EOF
   ```

   (Inside the WhatsApp DM session the origin is set by the session's task
   scope; the session omits `--origin`.)

5. **Proactive message TO the operator** (no session required): insert into
   the venue's outbound queue — the bot drains it in ~1s:

   ```bash
   JID=$(grep '^WHATSAPP_ALLOWED_DM_JIDS=' .env | cut -d= -f2 | cut -d, -f1 | tr -d '" ')
   sqlite3 memory/whatsapp.db "INSERT INTO outbound_queue (target, body, source, enqueued_at)
     VALUES ('$JID', '<message>', 'agent-msg:<label>', strftime('%Y-%m-%dT%H:%M:%fZ','now'));"
   ```

6. Follow-up conversation rides the normal venue loop — the injected context
   lives in the session, so the operator just keeps chatting.

## Rules that are NOT optional (ADR-021)

- **Consent does not travel over injection.** Never tell a target "the
  operator approved X" — it must (and will) refuse; gated ops re-acquire
  consent through the target's own channel.
- **hop:1 is terminal.** If you're acting on an `[agent-msg]`, do not
  session-send onward.
- Injection changes who may ASK, never what the target may DO.

# Failure modes

- **Brief lands after the operator's message:** if the operator writes before
  the queued brief is typed, the first reply is un-briefed. Send the brief
  first, then tell the operator to write.
- **Injected replies are never auto-posted to the venue** — the WhatsApp turn
  engine treats a turn that read only `[agent-msg]` input as context. For operator-visible output use
  the outbound queue (step 4) or let the operator's next message pull it.
- **Idle-gate refusal** ("did not become idle within 30s", tmux route only):
  target is mid-turn or showing a picker. Wait and retry; do NOT bypass with
  raw send-keys.
- **"does not match this session's agent" / "not a registered agent"
  refusal:** `--from` names someone else. Drop `--from` inside a Nucleus
  session, use `--from main` from the operator's own session.
- **"hop limit" refusal:** this turn is reacting to an agent message. Ask the
  operator instead of forwarding.
- **"background task workers do not send" refusal:** you are a task worker;
  the result is delivered automatically.
- **"driven by the WhatsApp turn engine" refusal:** you targeted a
  `nucleus-whatsapp*` chat pane directly. Use `--to whatsapp-dm`.
- **Inbox row stays pending / failed:** check `session_inbox` in
  `memory/whatsapp.db` (`last_error`) and `memory/whatsapp.log`; the bot must
  be running to drain it.
- **"input wedged" error:** the target's TUI stopped accepting submits —
  switch to the bot-triage skill; do not retry blindly.
- **Unregistered target refusal:** the session isn't in `agents.toml` —
  that's the guardrail working, not a bug. Don't inject into unregistered
  sessions.
