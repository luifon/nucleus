# ADR-037 — ntfy push notifications as a delivery channel

**Status:** Accepted + built (2026-09-30)

**Builds on:**
- [[ADR-006]] — reminders, multi-channel delivery with per-channel retry.
- [[ADR-033]] — credential filtering of text Rust sends out.

## Context

Reminders reach the operator through Discord, the WhatsApp DM or a calendar
event. Discord and WhatsApp are chat apps: a reminder arrives as one message
among conversations, with the notification settings of that app, and the
WhatsApp path depends on the WhatsApp bot being connected. There was no
channel whose only job is to make the phone show a notification.

ntfy is a small push notification server. A client publishes a message to a
topic with one HTTP request; the ntfy phone app shows it as a notification,
with a priority that decides whether it sounds. The operator runs one, reached
privately, with access denied by default.

## Decision

1. **`nucleus_core::ntfy`** sends one notification: title, message,
   priority, tags and an optional click URL, published to the server root as
   JSON with a bearer token. It is shared code, so other senders (task results,
   the heartbeat) can use it later without a second implementation.
2. **Configuration is `.env` only:** `NUCLEUS_NTFY_URL`, `NUCLEUS_NTFY_TOPIC`
   and `NUCLEUS_NTFY_TOKEN`, read into `Settings::ntfy`. The channel is off
   unless all three are set. The server address identifies an operator's
   setup, so it is never in `nucleus.toml` or a committed file (Rule 1). The
   URL must be the server root: JSON publishing only happens at `/`, and at a
   topic path ntfy would publish the JSON text as the message and still
   report success, so a URL with a path, query or fragment turns the channel
   off with a logged warning. The topic name is an `.env` value too, so
   `tools/check-secrets.sh` rejects committed text that contains it, which
   means a topic name that occurs in the repository blocks commits. The
   scanner skips values shorter than 6 characters and a list of common words,
   so it does not protect a short or common topic name; use one of at least
   6 characters that occurs nowhere in the repository.
3. **A token, not a password.** The token belongs to an ntfy user that may only
   publish to the one topic. A leaked token can send notifications to that
   topic and nothing else, and it is revoked without touching the operator's
   own account.
4. **Credential filter.** The title and the message pass
   `secret_filter::CredentialRules` before they leave, as the Discord task
   results do. The rules always include the configured token itself, which
   may come from the process environment rather than `.env`. Error text from
   the server is filtered the same way, has the token removed and is capped at
   300 bytes before it reaches a log or the reminder history. The message is
   capped at 4 000 bytes, under ntfy's default 4 096-byte limit, above which
   ntfy turns a message into an attachment.
5. **The `ntfy` reminder channel.** `deliver()` publishes the reminder with
   its title (or `Reminder #<id>`), priority high and the `bell` tag. Because
   every reminder path goes through `deliver()`, plain reminders, skill-fire
   replies and the ⚠️ failure alerts all reach ntfy the same way. The fire's
   `msg_id` is `ntfy#<message id>`.

## Consequences

- `--channels ntfy` works next to the others, with the same per-channel retry
  (up to 3 attempts per fire).
- ntfy has no idempotency key: a publish that succeeded on the server but
  whose response was lost is retried and shows twice. Discord has the same
  property.
- The phone receives only what the server receives. With iOS instant delivery
  through ntfy.sh as upstream, ntfy.sh sees only message IDs, never the text.
- Not done here: task results and heartbeat alerts still use their current
  venues; moving them to ntfy is a separate decision.
