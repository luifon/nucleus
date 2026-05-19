# ADR-005b — WhatsApp DM mode (operator-only, conversational)

**Status:** Proposed (2026-05-19)
**Addendum to:** [[ADR-005]] — second brain / PARA vault
**Sibling of:** [[ADR-005a]] — brain-dump review-before-apply

## Context

The WhatsApp bot today is group-only. The Baileys handler filters inbound
messages against `WHATSAPP_ALLOWED_GROUP_NAMES`; anything that doesn't
arrive in a listed group is silently ignored. Two conversational
patterns are forced into groups as a result:

- **Brain-dump capture** — the brain-dump group exists for this. ADR-005
  and ADR-005a cover the flow end-to-end. Works well.
- **Casual conversation with the bot** — also runs in a group, because
  groups are the only listening surface. Mixing casual chat with
  brain-dump capture in the same group creates ambiguity; running it in
  a separate group requires either inviting yourself to a group of one
  (awkward in WhatsApp) or maintaining a "conversational group" alongside
  the brain-dump group (current state).

The operator runs the bot on their own WhatsApp number — same identity
they use for personal chat. A natural pattern emerges: **DM the bot's
number directly**. WhatsApp lets you message yourself; that's exactly
the surface the bot could listen on, and it's where casual conversation
"feels right" without group ceremony.

ADR-009 (persona configurability) introduced the env-var shape that
makes this clean: per-context persona overrides like
`NUCLEUS_PERSONA_WHATSAPP_DM` extend the venue default without
duplication. This addendum is the implementation of the DM listening
half — the persona side is already in place after ADR-009 lands.

## Decision

Allow the WhatsApp bot to listen to **DMs from explicitly-allowlisted
JIDs**, in addition to the existing group allowlist. For the
single-operator case the allowlist contains exactly one JID — the
operator's own. DMs from any other JID are silently ignored, same
posture as the existing group rejection behavior.

**Brain-dump capture remains group-scoped.** ADR-005a's review-before-
apply flow stays inside the brain-dump group. DM is for non-brain-dump
conversation only. Reasons:

- The brain-dump pipeline has a multi-step model (capture → plan →
  rundown → apply) that's tightly coupled to its group context
- Routing brain-dump through DM would duplicate the model and create
  ambiguity about which transcript triggered which capture
- "Same platform per flow" (ADR-005a) is preserved: a brain-dump
  capture's whole lifecycle stays in the brain-dump group

If brain-dump-via-DM is ever wanted, it's a separate decision —
explicitly out of scope here.

## Allowlist shape

A new env var, parallel in shape to `WHATSAPP_ALLOWED_GROUP_NAMES`:

```
WHATSAPP_ALLOWED_DM_JIDS=<jid-1>,<jid-2>,...
```

- Comma-separated list of WhatsApp JIDs in the `<number>@s.whatsapp.net`
  form (the canonical Baileys JID shape for individual users)
- Default in `.env.example` is **empty** — DM listening is opt-in. A
  fresh-clone operator gets group-only behavior, identical to today
- For the single-operator case, the value is just the operator's own JID

The operator finds their own JID by sending a message from their own
number to the bot's number (or via the Baileys session log on first
connect). Both approaches surface the JID cleanly.

## Routing

The WhatsApp inbound handler gains a chat-type discriminator before
allowlist lookup:

```typescript
function chatType(jid: string): "group" | "dm" {
  return jid.endsWith("@g.us") ? "group" : "dm";
}
```

Dispatch logic:

1. **Group inbound** (`@g.us` suffix):
   - Check group name against `WHATSAPP_ALLOWED_GROUP_NAMES`.
   - If the group matches `WHATSAPP_BRAINDUMP_GROUP_NAMES` (existing
     env var) → brain-dump handler (per ADR-005a).
   - Otherwise, conversational handler with the group-context persona.
   - Unknown group → silently ignored (today's behavior, unchanged).
2. **DM inbound** (`@s.whatsapp.net` suffix):
   - Check sender JID against `WHATSAPP_ALLOWED_DM_JIDS`.
   - If allowed → conversational handler with the DM-context persona.
   - Unknown sender → silently ignored.
   - **Brain-dump in DM is rejected explicitly**: if the operator
     sends a DM that looks like a brain-dump capture (voice memo, or
     text that includes a brain-dump trigger keyword), the bot replies
     with a one-line nudge: *"brain-dump goes to the brain-dump
     group."* No silent acceptance of misrouted captures.

The two conversational handlers (group-context and DM-context) share
the same underlying `SessionPool` machinery; they differ only in the
persona resolved at spawn time and in which allowlist gated their
entry.

## Persona per context (uses ADR-009's extension)

`resolve_persona()` from ADR-009 gains an optional context parameter:

```rust
pub fn resolve_persona(venue: &str, context: Option<&str>) -> Result<PersonaContent>;
```

Resolution order (first match wins):

1. `NUCLEUS_PERSONA_<VENUE>_<CONTEXT>` — e.g., `NUCLEUS_PERSONA_WHATSAPP_DM`
2. `NUCLEUS_PERSONA_<VENUE>` — e.g., `NUCLEUS_PERSONA_WHATSAPP`
3. Hard error (no silent fallback)

Three contexts for WhatsApp:

- `DM` — direct messages
- `GROUP` — conversational groups (non-brain-dump)
- `BRAINDUMP` — the brain-dump group(s)

`.env.example` ships only the venue default
(`NUCLEUS_PERSONA_WHATSAPP=assistant`) — the per-context overrides are
optional knobs for operators who want voice differentiation. Most
operators keep them all the same; the override is there for those who
explicitly want, e.g., a terser voice in brain-dump than in DM.

## Outbound — Rule 6 implications

CLAUDE.md Rule 6 lists the pre-authorized outbound destinations the
bot can send to without asking. DM-to-the-operator joins that set:

- A DM **from** the operator to the bot's own number → reply via DM is
  pre-authorized (same shape as "DM from the user" already covered by
  Rule 6 — this addendum makes that explicit for WhatsApp)
- The operator's own JID, when the bot proactively initiates (e.g., a
  scheduled reminder routed via `--channels alfred` — but see Related
  cleanups below) is pre-authorized
- Outbound DMs from the bot to **any other JID** still require explicit
  operator authorization

The Rule 6 update is small — adding "WhatsApp DM from the operator's
JID (per `WHATSAPP_ALLOWED_DM_JIDS`)" to the pre-authorized list.

## Reminder channel routing

The reminders system today supports a `--channels alfred` value that
posts to the WhatsApp conversational group. With DM mode landed, the
operator may prefer reminders in DM:

```bash
reminders add --at <iso> --body "..." --channels whatsapp-dm
```

This requires a new channel value (`whatsapp-dm`) in the reminders
channel registry. Implementation is a small addition to the channel
dispatch logic, mirroring how `alfred` (conversational group) and
`braindump` (brain-dump group) work today.

The new channel value: `whatsapp-dm`. Posts to the operator's JID
(first entry of `WHATSAPP_ALLOWED_DM_JIDS`). If the DM allowlist is
empty, the channel is unavailable and `reminders add --channels
whatsapp-dm` errors at parse time with a clear message.

## Brain-dump in DM — out of scope

The bot's DM handler rejects brain-dump-shaped inbound (voice memos
sent in DM; text containing brain-dump trigger keywords) with a
single-line nudge directing the operator to the brain-dump group.
This is enforcement, not just convention — accepting a brain-dump in
DM would silently produce vault writes from a channel the model wasn't
designed for, and reviewing in DM would split ADR-005a's review flow
across two surfaces.

Voice memos in DM that are clearly conversational (short, addressed to
the bot, not capture-shaped) are transcribed and treated as
conversation. The "is this a capture or a conversation?" disambiguation
uses the same heuristics today's brain-dump group uses to decide
whether something's a capture or a meta-comment.

## Migration

1. Add `WHATSAPP_ALLOWED_DM_JIDS` to `.env.example`, default empty
2. Update Baileys inbound handler in `messaging/whatsapp/src/index.ts` to:
   - Compute `chatType(jid)` early in the message-handler entry
   - Branch group / DM dispatch as described above
   - Reject brain-dump-shaped DMs with the nudge reply
3. Wire `resolve_persona("whatsapp", Some("dm" | "group" | "braindump"))`
   into the spawn points for each handler (group conversational,
   brain-dump, new DM handler)
4. Update CLAUDE.md Rule 6 to add the DM-from-operator pre-authorized
   case
5. Add `whatsapp-dm` channel value to the reminders channel registry
   (`chores/reminders/src/channels.rs` or wherever the registry lives)
6. Update CLAUDE.md Rule 10's `--channels` value table to include
   `whatsapp-dm`
7. Document the operator setup step (how to find own JID, populate
   `WHATSAPP_ALLOWED_DM_JIDS`) in the README's WhatsApp section

## Operator-visible setup

For the existing operator wanting to enable DM mode:

1. Send any message from their own number to the bot's number (creates
   the DM thread, surfaces the JID in the Baileys connection log)
2. Read the JID from `messaging/whatsapp/<log-file>` or via a one-liner
   helper script
3. Add it to `.env`:
   `WHATSAPP_ALLOWED_DM_JIDS=<their-jid>@s.whatsapp.net`
4. Restart the WhatsApp bot
5. Test by sending a DM — the bot should respond with the DM-context
   persona

The setup wizard (ADR-010) can automate steps 2-4 once it lands.

## Related cleanups noted but not in this ADR

- **The `--channels alfred` reminder channel value** is itself a Rule 7
  drift (persona name in a channel identifier). Renaming to `whatsapp`
  or `whatsapp-group` requires migrating existing rows in
  `memory/reminders.db` that reference the old value. Separate work,
  separate commit. The new `whatsapp-dm` channel added here uses the
  venue-named convention from the start, which is the direction
  `alfred` should eventually shift to.

## Out of scope

- Multi-operator DM allowlists — Nucleus is single-operator
- DMs from arbitrary contacts (intentional security boundary; the
  allowlist is the gate)
- Brain-dump capture via DM
- Group-DM hybrid flows (a conversation that starts in DM and migrates
  to a group, or vice versa) — each thread stays in its channel
- Custom DM commands / slash-syntax — the DM handler is conversational,
  not command-driven (existing brain-dump commands like `/sync` or
  pinned shortcuts stay group-scoped)

## References

- ADR-005 — second brain / brain-dump pipeline; the original
  WhatsApp listening surface
- ADR-005a — brain-dump review-before-apply; the in-band review flow
  that stays group-scoped
- ADR-009 — persona configurability; the per-context env-var shape
  this ADR uses
- ADR-010 — setup wizard; automates DM allowlist population once
  shipped
- CLAUDE.md Rule 6 — outbound authorization; updated to add the
  operator-DM pre-authorized case
- CLAUDE.md Rule 10 — reminders CLI / channel values; updated to
  include `whatsapp-dm`
