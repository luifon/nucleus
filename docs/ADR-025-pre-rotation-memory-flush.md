# ADR-025 — Pre-rotation memory flush: persist before the 4 am recycle

Date: 2026-07-18
Status: accepted + built (2026-07-18)

> **As-built deviation.** Implementation revealed the rotation already
> asks the dying session for a continuity summary at exactly the right
> moment — so the flush is FOLDED INTO that ask instead of being a
> separate `FLUSH_OK` turn. The rotation prompt now requests two labeled
> sections: `SUMMARY:` (priming + diary, as before) and `DURABLE:`
> (observations not yet recorded anywhere; `none` when empty). The
> runtime splits the reply (`split_rotation_reply` /
> `splitRotationReply`, shared vectors in
> `core/testdata/rotation_reply_vectors.json`); a DURABLE section becomes
> a distinct `memory_flush <chat>` diary entry for the distiller. A reply
> that ignores the format degrades to summary-only — exactly the
> pre-flush behavior, so the failure mode is "no worse than before" by
> construction. The separate-turn design below is retained for context;
> its skip-when-idle and cannot-block-rotation properties hold trivially
> in the folded form (the summary ask already has both).

## Context

Long-lived venue sessions (WhatsApp per-chat, Discord pool) are recycled
daily at 4 am and re-primed from a summary + replayed tail. Anything the
session knew that made it into neither its diary nor the priming summary
dies with the rotation. The diary contract is best-effort during the day;
in practice a session's most useful observations often exist only in its
transcript at rotation time. OpenClaw ships the countermeasure as a
"memory flush": immediately before compaction, a silent agent turn prompts
persistence of unsaved durable facts to the memory files.

This composes with ADR-023: transcripts stay searchable, but search is
recall-on-demand — the diary → distiller → T2/T3 pipeline is what makes a
fact *ambient*. Flush feeds the pipeline; search covers what flush missed.

## Decision

Before a scheduled rotation (TS `sleepUntilNext4am` path in
`messaging/whatsapp`, and the Rust `SessionPool` rotation used by Discord),
the runtime sends one final turn to the session:

> Rotation imminent. Append to your diary (via the normal mechanism) any
> durable observations from this session not yet recorded: decisions,
> corrections, recurring user preferences, unresolved threads. Reply
> `FLUSH_OK` when done, or `FLUSH_OK` alone if nothing qualifies.

Rules:

- **Skip when idle:** no flush if the session has had no substantive turn
  since the last rotation (reuses ADR-023's substantive-exchange
  predicate). An idle chat produces zero flush cost.
- **Bounded:** `await_turn_complete` with a hard 90 s cap; on timeout or a
  non-`FLUSH_OK` reply, rotate anyway and log — flush is best-effort and
  must never wedge the 4 am cycle (the wedged-input lessons of ADR-021/22
  apply: rotation proceeds unconditionally).
- **Silent:** the flush turn and its reply never reach the venue; it is
  runtime↔session traffic only, like the priming preamble.
- The flush reply is not parsed for content — the diary write is the
  side effect; `FLUSH_OK` is just the completion marker (same
  ready-to-send contract style as the fire persona).

Mirrored TS/Rust like the submit-verify machinery, with shared fixture
vectors where logic is pure (the skip-when-idle predicate).

## Consequences

- Distiller input quality rises where it matters most: the observations a
  session accumulated but never wrote down. The 4 am cycle lengthens by
  up to ~90 s per live session — irrelevant at that hour.
- One more model turn per active session per day — marginal cost, bounded
  by the skip-when-idle gate.
- Failure modes are contained by construction: flush cannot block
  rotation, cannot post to the venue, and its absence (timeout) degrades
  to exactly today's behavior.
