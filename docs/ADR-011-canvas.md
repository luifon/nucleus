# ADR-011 — Canvas: agent-rendered interactive components in the dashboard chat

**Status:** Proposed (2026-05-16)

## Context

The dashboard chat (`chat/` crate, served at the URL in `$NUCLEUS_CHAT_PUBLIC_URL`) is text-in / text-out. Agent output is rendered as markdown via `marked`. Anything that would be faster as an *interaction* — "pick which PARA bucket for this capture", "uncheck the ops you don't want", "fill in date + duration" — gets serialized to "give me one of: A, B, C" and the operator types back. Slower and noisier than a click.

OpenClaw's "Live Canvas" (A2UI) demonstrates the alternative: the agent emits a structured block, the frontend renders it as a widget, the user interacts, the response posts back as a structured message that the agent's next turn can read. Constrained to *one* surface (a frontend capable of rich rendering); irrelevant for transports that are inherently text (Discord, WhatsApp, Gmail).

A second motivation surfaces alongside the rendering one: the current dashboard chat is a *generic* surface with no distinct identity. Every other venue has a persona (Jerry, the WhatsApp persona, JARVIS). The chat surface — where the operator does deeper, longer-form work — deserves a venue of its own. This ADR introduces both: canvas rendering, and a chat-specific persona that lives where canvas naturally lives.

## Decision

The dashboard chat gains:

1. A **canvas** rendering layer — the agent may emit canvas blocks; the frontend parses them out before markdown rendering and substitutes interactive components. Submissions post back as structured messages the agent recognizes on its next turn.
2. A **dedicated persona** for chat-hosted sessions — **Q**. Owns the chat venue. Receives canvas spec + behavior in its system prompt. Discord, WhatsApp, and Gmail sessions are unchanged.
3. A **parallel rollout** — canvas ships as a new binary served at a new URL (`$NUCLEUS_CHAT_V2_PUBLIC_URL`), alongside the existing chat URL (`$NUCLEUS_CHAT_PUBLIC_URL`), until the canvas surface is proven safe in production. Then it replaces the old chat.

## Prerequisites

This ADR does not ship without two pieces of upstream work:

- **ADR-010 (proposed) — perimeter / private deployment.** The dashboard and chat surfaces must be moved behind Tailscale before canvas ships. News stays publicly accessible (read-only RSS-shape); dashboard and chat are operator-only. Canvas adds attack surface to a chat surface that is currently public; the perimeter move is therefore a hard precondition, not a parallel concern. ADR-010 is drafted at implementation kickoff, not now.
- **A persona file for the chat venue** at `chat/persona.md`. Q's voice, scope, behavior. Substituted into spawned sessions via `--append-system-prompt`, same mechanism every other venue uses (Rule 7).

## Where canvas works

- **Dashboard chat (`chat/`)** — yes. The only surface.
- **Discord, WhatsApp, Gmail** — no. Text transports. The agent must detect host and fall back to text-mode prompts ("reply with 1, 2, or 3"). The persona system prompt for each venue tells it which surface it's running on; only Q's prompt knows about canvas.
- **News-fetcher, distillers, reminders one-shot sessions** — no. Non-interactive, no user present.

## The Q persona

Q is the chat venue's persona. Lives at `chat/persona.md`. Substituted at session spawn.

Q's scope:
- Owns the dashboard chat surface
- Knows the canvas spec (it lives in Q's system prompt or in `chat/canvas-syntax.md` loaded into Q's sessions only)
- Has access to the PARA-Obsidian vault via `--add-dir` (existing chat behavior)
- Speaks the same operational language as the other personas (Rule 6 outbound rules apply, Rule 9 vault-writing rules apply)
- Does **not** emit canvas blocks when the user request is conversational, when explanation is the goal, or when the choice space is too large (>~7 options); falls back to prose with numbered lists

Q's identity is **the operator of the deeper-work surface** — not a gatekeeper or assistant-for-someone-else (per the persona archetypes convention). Q runs the dashboard, the way JARVIS runs the inbox.

## Component types

All five ship in the first cut. No phasing.

| Type | What | Submitted value |
|---|---|---|
| `decision` | Single-select from N options. Click an option → submitted. | One `<value>` |
| `multi-select` | Multi-select from N options. Click checkboxes + submit. | One or more `<value>`s |
| `confirm` | Two options, YES / NO, with body text framing. Click → submitted. | One `<value>`: `yes` or `no` |
| `form` | Structured fields (text, date, number). Fill + submit → object. | One `<value>` per field, keyed by field name |
| `review` | List of items each with a per-item checkbox (e.g. multi-op plan). Submit → list of accepted items. | List of accepted item ids |

`confirm` could be folded into `decision` (two hardcoded options), but a separate type lets the frontend render it with stronger visual affordance (a real yes/no pair, not a generic two-button select).

## Emission format

XML-like inline blocks in the agent's message text:

```xml
<canvas type="decision" id="bucket-pick-2026-05-16-abc123">
  <prompt>Which PARA bucket should this go in?</prompt>
  <option value="1-Projects/X">1-Projects/X — active engagement</option>
  <option value="2-Areas/Y">2-Areas/Y — ongoing</option>
  <option value="0-Inbox">0-Inbox — unsure</option>
</canvas>
```

```xml
<canvas type="form" id="event-create-2026-05-16-def456">
  <prompt>Creating a calendar event. Fill in:</prompt>
  <field name="title" type="text" required="true" />
  <field name="start" type="datetime" required="true" />
  <field name="duration_min" type="number" default="30" />
  <field name="attendees" type="text" placeholder="comma-separated emails" />
</canvas>
```

```xml
<canvas type="review" id="multi-op-2026-05-16-ghi789">
  <prompt>The brain-dump plan would do these ops. Uncheck any you don't want.</prompt>
  <item id="op-1" default="checked">create 1-Projects/X/idea.md</item>
  <item id="op-2" default="checked">append to 3-Resources/Y.md</item>
  <item id="op-3" default="unchecked">move 0-Inbox/old.md → 4-Archives/</item>
</canvas>
```

- `type` — `decision` | `multi-select` | `confirm` | `form` | `review`
- `id` — unique per session, scoped to a session's history. Agent generates with a timestamp + short random suffix.
- Inner elements depend on type; all canvas types have a `<prompt>` for the human-readable framing.

XML-like over markdown code fence (` ```canvas\n... \n``` `) because LLMs reliably emit balanced tags, regex parsing is sufficient for MVP, and the structure visually distinguishes from prose. The escaping cost (literal `<>` inside option text) is minor.

## Response format

When the user submits, the frontend posts a new user message back to the chat API. The message body contains a structured marker:

```xml
<canvas-response id="bucket-pick-2026-05-16-abc123">
  <value>1-Projects/X</value>
</canvas-response>
```

```xml
<canvas-response id="event-create-2026-05-16-def456">
  <value name="title">Q3 review</value>
  <value name="start">2026-08-15T14:00:00-03:00</value>
  <value name="duration_min">60</value>
  <value name="attendees">a@b.com</value>
</canvas-response>
```

```xml
<canvas-response id="multi-op-2026-05-16-ghi789">
  <value>op-1</value>
  <value>op-2</value>
</canvas-response>
```

Q's system-prompt addendum instructs it to expect this shape and parse `<value>` elements (one for `decision` / `confirm`, multiple keyed by `name` for `form`, multiple unkeyed for `multi-select` and `review`).

The original `<canvas>` block stays in the message history as-is. Re-rendering shows it in submitted state (see below).

## Submitted state — disabled-but-visible

Once the user submits, the rendered widget transitions to **disabled-but-visible**:

- Decision / multi-select / confirm: chosen options are highlighted, others greyed; no click handlers.
- Form: fields display the submitted values, read-only.
- Review: items show the checked/unchecked state at submit time; checkboxes disabled.

Rationale: the canvas block is a record of what was rendered + chosen. Future agent turns may reference it. The operator may scroll back and remember context ("oh right, I picked X here"). Collapsing to a one-line summary loses information; disabled-but-visible preserves it without the risk of accidental re-submission.

## Frontend rendering

In `chat_index.html`, the assistant-message pipeline becomes:

```js
function renderAssistantContent(text, messageId) {
  const { canvasBlocks, rest } = extractCanvasBlocks(text);
  let html = marked.parse(rest);
  for (const block of canvasBlocks) {
    const submitted = lookupSubmittedState(block.id);  // from history or localStorage
    html = appendCanvasWidget(html, block, submitted);
  }
  return html;
}
```

Each widget is a small inline component (no shadow DOM, no frameworks — kept inline for the same lightweight pattern the rest of the chat uses). Submit handler POSTs the response marker to the chat API as a new user-role message.

State persistence:
- **Authoritative:** the `<canvas-response>` message in chat history. Re-render scans the message history for the matching `id`.
- **Cache:** `localStorage` mirrors the latest submitted state per `id`, so a post-submit refresh shows the chosen state without waiting for a history fetch.

## Backend changes

`chat/src/api.rs` doesn't need to know canvas exists at the model layer. Agent output flows through unchanged. Response messages (`<canvas-response>...`) post via the existing user-message endpoint.

One optional change: tag stored messages with a derived `has_canvas` flag for cheap re-render filtering. Implementable later.

## Destructive actions

**Canvas is a choice surface, not an execution surface.** When a canvas-driven choice would lead to a destructive operation, the agent must perform a typed-text confirmation in the same chat before executing. The frontend never has a clickable "delete" / "send-to-other" / "purge" button without that follow-up.

"Destructive" categories canvas can present *and the agent must follow up before executing*:

- **Vault operations** — any `delete` or `move` op against the Obsidian vault (ADR-005)
- **Outbound to non-self audiences** — any message to a channel or contact other than the operator themselves or pre-authorized scheduled outputs (CLAUDE.md Rule 6)

Categories explicitly **not** in this list because the surface doesn't apply:

- Reminders cancel / pause / resume — done via WhatsApp or CLI, not chat
- Skills archive — done via direct Claude Code session, not chat
- Calendar `delete_event` — operator-acceptable risk; the calendar is the trash account's, and recovery is trivial

Enforcement model: a *policy*, not a runtime gate. The Q persona prompt carries the rule. A canvas block whose selection implies a destructive op must be followed by the agent emitting a plain-text confirmation ("Type 'yes delete' to confirm"). The frontend does not prevent the agent from misbehaving — that's a persona/prompt concern. If incidents recur, escalate to a runtime gate.

## Out of scope

- Cross-session canvas — a canvas opened in chat A can't be answered from chat B.
- Real-time multi-user — Nucleus is single-operator.
- Drawing / sketching — UI selection, not generative art.
- Free-form HTML rendering — security risk, not needed.
- Discord / WhatsApp / Gmail canvas — text transports; the agent falls back to prose prompts in those venues.
- **Brain-dump review on canvas.** Brain-dump captures arrive via WhatsApp; review and approval stay on the same platform per the single-platform-per-flow principle. ADR-005 may grow a "rundown-before-apply" step on WhatsApp; that is not a canvas concern.

## Security

- Frontend escapes all canvas-block text content. `marked` is invoked only on non-canvas message text.
- No `<script>`, `<iframe>`, no arbitrary HTML inside canvas option / item / prompt text.
- No JS evaluation of agent-provided strings.
- No file upload in MVP (a future `form` field type may add it, with strict MIME + size constraints documented separately).
- Tailscale-only access (ADR-010 prerequisite) means the surface is not exposed to the public internet.

## Migration / rollout

1. **Prerequisite — ADR-010:** Tailscale-gate dashboard + chat. News stays public. Verify no leaks.
2. **Stand up the new binary at the URL in `$NUCLEUS_CHAT_V2_PUBLIC_URL`:**
   - Either a forked `chat-v2/` crate or a feature-flagged `chat/` build, served on a separate port + tunnel.
   - The existing chat URL (`$NUCLEUS_CHAT_PUBLIC_URL`) continues serving the unchanged old binary.
3. **Add Q persona:** `chat/persona.md` (or `chat-v2/persona.md` during parallel period). Wired into session spawn via `--append-system-prompt`.
4. **Add canvas spec to Q's system prompt:** either inline (full spec ~1-2k tokens) or via a reference doc loaded with `--add-dir` (e.g., `chat/canvas-syntax.md`). The spec does NOT live in T2 (T2 is shared across all venues; Discord/WhatsApp sessions should not see canvas instructions).
5. **Frontend canvas parsing + rendering** in `chat_index.html`.
6. **Burn-in period:** dogfood the v2 URL for some weeks. Watch for: canvas blocks that should have been prose, prose that should have been canvas, destructive-action policy violations, frontend rendering edge cases (nested canvas, malformed XML, very long option lists).
7. **Replace:** when stable, retire the old chat binary, point `$NUCLEUS_CHAT_PUBLIC_URL` at the v2 surface, decommission the temporary v2 URL.

## Future work

- **Inline mermaid / chart canvas types** — diagrams and charts as first-class canvas, not markdown extensions.
- **File / vault preview canvas** — a block that previews a markdown file from the vault with an "open in Obsidian" action.
- **ADR-010 (perimeter)** — drafted at implementation kickoff. Tailscale ACLs, cloudflared adjustments, verifying that news stays public while dashboard + chat are private.
- **Brain-dump rundown on WhatsApp** — addendum to ADR-005. Separate from canvas; documented here only to record the decision that it does *not* belong on canvas.

## References

- ADR-002 — tiered memory; explains why canvas spec lives in the chat venue, not T2
- ADR-005 — brain-dump pipeline; canvas review explicitly out of scope (single-platform principle)
- ADR-006 — reminders; not a canvas surface, listed for cross-reference
- ADR-007 — JARVIS / Gmail venue; pattern for "venue-specific persona with venue-specific system prompt"
- ADR-008 — skills; not a canvas surface
- ADR-010 (proposed) — perimeter / private deployment; hard prerequisite
- CLAUDE.md Rule 6 — outbound messaging authorization (applies to canvas-driven outbound)
- CLAUDE.md Rule 7 — code identity is the venue, not the persona (chat = venue, Q = persona)
- CLAUDE.md Rule 9 — vault-writing rules (apply when canvas-driven choices result in vault ops)
- OpenClaw Live Canvas (A2UI) — prior art
