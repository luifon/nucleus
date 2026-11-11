# ADR-044 — Checked session output

Date: 2026-11-11
Status: proposed

**Relates to:**
- [[ADR-033]] — the turn engine and the task ledger. Before a chat turn's
  reply or a task result is sent, code reads the turn's transcript for tool
  failures the reply does not mention and adds a code-written note (§1).
  Rule 8 of the turn engine (infrastructure banners are never delivered as
  content) stays; this ADR adds a second check on replies that are
  delivered.
- [[ADR-008]] — skill-fires. The reply of a fire session is still the post;
  code may add the note of §1 to it. The session never writes that note
  itself.
- [[ADR-025]] — the memory flush before the daily compaction. The summary
  turn gets a code-built list of the actions Nucleus actually performed for
  that chat (§2), so the summary does not have to reconstruct them.
- [[ADR-006]], [[ADR-036]] — the reminders store and the work store are two
  of the stores the list of §2 is built from.

## Context

Two failures of session output have happened in Nucleus:

- A compaction summary called a reply that had been sent "unsent".
- Skill-fires posted usage-limit banners as content (since fixed in
  `classify_infra_reply` in Rust and `classifyInfraReply` in TypeScript).

Two surveyed open-source assistant stacks address the same class of
failure. One reports a tool failure that no later step answered, instead
of ending the turn as a success. The other marks uncertain claims in its
compaction summaries as `UNVERIFIED`.

Claude Code writes its own compaction summaries, so Nucleus cannot label
claims inside them. What Nucleus controls is the reply text before it is
sent, the transcript of the turn, its own stores of actions, and the text
it types into a session.

The operator rated this a minor item and left the implementation details
to the implementer.

## Decision

### 1. Tool failures the reply does not mention

Before Nucleus sends a session's reply, code reads the transcript of the
turn that produced it. This applies to:

- chat turns (the WhatsApp turn engine, ADR-033 §1; the Rust `Session` and
  `SessionPool` asks of the Discord bot and the dashboard chat);
- skill-fires (ADR-008), after the worker has cut the reply to the post;
- task results (ADR-033 §3), for the task worker's turn.

A tool failure is a `tool_result` record with `is_error: true` in the
turn. When the turn contains a tool failure that the reply does not
mention, code adds a note to the reply: "a step failed: <tool>", with the
tool's name. The note is a code-owned text (English, in the venue's texts:
`[whatsapp.texts]`, `[tasks.texts]` and the other venues' equivalents,
CLAUDE.md Rule 13). The reply is not changed otherwise and is still sent.
The note and the reply go through the same delivery path as the reply
alone (`outbound_queue`, the target policy and the secret filter on
WhatsApp).

### 2. Nucleus's own actions come from the stores

A statement about an action of Nucleus ("sent", "scheduled", "pushed") is
checked against the store that records that action, and is not trusted
from a summary.

Action kinds with a durable record today:

| Action | Store |
|---|---|
| a WhatsApp message sent or queued by the WhatsApp bot or a Nucleus sender | `outbound_queue` (`memory/whatsapp.db`) |
| a reminder scheduled, fired, cancelled | `memory/reminders.db` (ADR-006) |
| a task started, finished, delivered | `memory/tasks.db` (ADR-033) |
| a push, a draft pull request, an issue comment of the work pipeline | `memory/work.db` (ADR-036) |

Action kinds without a durable record today:

- a Discord message: the Discord bot sends directly through
  `channel.send_message` (`messaging/discord/src/lib.rs`) and records no
  row;
- a `git push` outside the work pipeline (ADR-040 §1): `memory/work.db`
  records only the pipeline's own pushes.

A statement about an action kind without a durable record is marked
**not checkable**. It is never marked "not done": the absence of a row
for such a kind says nothing about whether the action happened. A kind
moves to the first table when a store starts to record it.

Code builds a list of the actions actually performed from the stores of
the first table.
The list is given to a session at the two points where a session
otherwise works from a summary:

1. the summary turn of ADR-025's memory flush before the daily compaction;
2. the text Nucleus gives the WhatsApp chat session when it starts (a new
   session or a resumed one).

The list is code-built, so a model cannot add or drop an entry. It holds
facts from the stores (what, when, the identifier in the store), not
message bodies.

### 3. Action statements in replies are checked by code

Giving a session the list of §2 does not check what it then says. Before
Nucleus sends a reply (the same paths as §1), code finds the statements in
the reply about Nucleus's own actions ("sent", "scheduled", "pushed",
"cancelled") and compares each one with the stores of §2:

- a statement that a store confirms is left as it is;
- a statement about a kind with a durable record that no store row
  confirms gets a code-written note: "not found in the records: <action>";
- a statement about a kind without a durable record gets the note "not
  checkable: <action>".

This check is required. How code finds the statements and matches them to
store rows is decided when this is built (footnote 3). The notes are
code-owned texts, as in §1, and go through the same delivery path.

## Consequences

- A reply that reports success after a failed tool call reaches the
  operator with a note naming the tool. A reply that already says the step
  failed gets no note.
- A session that resumes after a compaction or a restart reads what was
  sent, scheduled and pushed from the stores, so it does not repeat a send
  that a summary called "unsent".
- A reply that claims an action no store confirms reaches the operator
  with a note. Claims about Discord sends and pushes outside the pipeline
  are marked "not checkable" until those actions have a durable record.
- Each checked reply costs one read of the turn's part of the transcript.
  Each list costs a read of four stores. No model call is added.
- Claims inside Claude Code's own compaction summaries stay unlabelled.

## Rejected alternatives

- **Marking uncertain claims in compaction summaries** (one surveyed
  stack's approach). Claude Code writes the compaction summary itself;
  Nucleus has no point at which it could change it.
- **Asking the session whether its own steps succeeded.** The transcript
  and the stores already record it; a model's answer is the kind of text
  this ADR does not trust.

## Open questions / footnotes

1. The rule that decides whether a reply "mentions" a failure is chosen
   when this is built. Candidates: the tool's name appears in the reply;
   any statement of failure appears in the reply.
2. Whether a failed tool call followed by a successful call of the same
   tool in the same turn (a recovery, which ADR-038 counts as
   `min_error_recoveries`) still adds a note is chosen when this is built.
   A refused tool call (permission classifier or operator) is also an
   `is_error` result; whether it counts is decided at the same time.
3. How code finds action statements in a reply (word lists, patterns per
   language, a parser) and how it matches each one to a store row (by
   action kind, target and time) is chosen when this is built. The check
   itself is required (§3).
4. How far back the list of §2 goes (since the last compaction, or a
   number of hours) and its size limit are not decided; it counts against
   the 64 KiB typed-prompt limit (ADR-033 §2).
5. Durable records for the kinds that have none today (Discord sends,
   pushes outside the work pipeline) are not designed; until they exist,
   statements about them are "not checkable".
6. Whether the start text of §2.2 is typed as the first message or passed
   as an appended system prompt is decided when it is built.

## Verification

- **Tool failures.** Transcript fixtures for each path (WhatsApp turn,
  Rust `Session` ask, skill-fire, task worker): a turn with an `is_error`
  tool result and a reply that does not mention it produces the reply plus
  the note with the tool's name; a turn whose reply mentions the failure
  produces no note; a turn without failures produces the reply unchanged.
  Shared vectors cover the Rust and TypeScript readers, as for the turn
  tracker.
- **Delivery.** On WhatsApp the note and the reply arrive through
  `outbound_queue` and pass the secret filter; a skill-fire note is added
  after the post is cut, so no text above the post marker reaches the
  channel.
- **Action list.** Fixture stores with a sent message, a scheduled
  reminder, a finished task and a push produce a list with exactly those
  entries; a message only present in a summary does not appear in it. The
  ADR-025 summary turn and a new and a resumed WhatsApp session receive the
  list (checked in the typed text).
- **Statement check.** A reply that says a WhatsApp message was sent with
  no matching `outbound_queue` row gets the "not found in the records"
  note; a reply whose statement a store row confirms gets no note; a reply
  that says a Discord message was sent, or that a push outside the
  pipeline was made, gets the "not checkable" note and never a "not done"
  or "not found" note.
- **Live check.** A WhatsApp turn in which a tool call fails while the
  reply reports success arrives with the note; after the daily compaction,
  the session answers "was X sent?" from the list.
