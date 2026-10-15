# ADR-025 — Pre-rotation memory flush: persist before the 4 am recycle

Date: 2026-07-18
Status: accepted + built (2026-07-18); the rotation became a compaction in
place on 2026-10-15 (next section)

## As built since 2026-10-15: daily compaction in place

The 04:00 job no longer replaces the session. It compacts the long-lived
chat session in place: the same session id, no new session, nothing written
to the session map. The memory flush and the diary entry stay.

Why: the rotation asked the session for a summary, spawned a new session
primed with it and stored the new id. In practice it never ran. The
WhatsApp engine skipped a chat whose session was closed, and the 4 h idle
reaper had always closed it before 04:00; after a process restart no actor
existed at all. Each new message resumed the same session with its full
history, so one DM session grew from July until Claude Code compacted it
automatically at 638k tokens (2026-10-14).

Per pass, for every chat in the stored session map (WhatsApp
`chat_sessions`, Discord `channel_sessions`, the dashboard's
`obsidian_chats`), one chat at a time:

1. **Skip** a chat with no activity in the last 24 h (WhatsApp: the latest
   `chat_inbound` or `chat_turns` row; Discord and dashboard: the stored
   `last_active`), and a session with fewer than 10 turns since its last
   compaction. The count is the number of `turn_duration` records after the
   last `compact_boundary` record of the transcript (all of it when it was
   never compacted), computed by the turn tracker
   (`turnsSinceCompaction` / `turns_since_compaction`, shared vectors in
   `core/testdata/turn_tracker_vectors.json`). The old measure (the last 100
   text turns of the whole file) would select an already compacted session
   every day.
2. **Wait** until no turn runs and nothing is queued, at most 10 minutes. In
   WhatsApp the actor's `compacting` flag then stops typing and following;
   messages that arrive wait in the queue and are typed after the
   compaction. In Rust the chat's pool slot is held; an ask waits on it.
3. **Resume** the stored session id when the session is closed, the same
   way a message does. A failed resume is a failure: no fresh session is
   spawned.
4. **Memory flush**: the summary turn (`SUMMARY_PROMPT`, 5 minutes at most,
   `awaitTurnComplete`). The reply is split by `splitRotationReply` /
   `split_rotation_reply` as before. Diary entries: `daily_compact <chat>`
   with the summary, and `memory_flush <chat>` when the DURABLE section is
   not empty.
5. **Compact**: once the transcript is quiet for 3 s, `/compact` is typed
   into the same session (verified submit, no date preamble), and the job
   waits for the transcript's `compact_boundary` record, at most 10
   minutes. On timeout it sends Escape, which cancels the compaction.
6. **Follow from the end** (WhatsApp): the transcript is followed from its
   settled end, so neither the summary turn nor the compaction records
   become turns or replies.
7. **Close again** a session that was closed before the job (WhatsApp: unless
   a message is waiting in the queue; Rust: the slot is removed), so the
   idle reaper's behavior does not change. A session that was open stays
   open and keeps its `last_active`.

A failure (busy too long, resume failed, summary turn failed, no
compact_boundary) is logged and written to the diary as `daily_compact
<chat>: compaction failed: …`; the session stays as it was and the next day
tries again. The operator is not messaged. A pass has a budget of 25
minutes: each step's timeout is cut to what is left, and no step starts
after it. A step can still end a few minutes later (the summary turn is
retried once after an infrastructure error with the same timeout; settling
the transcript, typing and a resume take up to a minute each).

When a failure happens after something was typed (a summary turn past its
timeout, a `/compact` that did not finish), one Escape stops it before new
messages are typed, so a waiting message starts its own turn instead of
being read by a turn the engine no longer follows. Only one: a second Escape
in a row opens Claude Code's message-rewind list. In WhatsApp the job also
waits for the chat's running background commands (started less than the
turn ceiling ago): their completion notices would arrive in records the job
skips.

**Ordering with the 04:00 restart.** `tools/restart-bots.sh` (launchd,
04:00) restarts the resident bots in the same minute, and a restart kills
the sessions. Each bot holds `memory/compaction/<agent>.lock` (`pid=<pid>`,
`started=<unix seconds>`; agents `whatsapp`, `discord`, `chat`) while it
compacts. The lock is taken 60 s before 04:00, so it exists before the
restart job reads it; the restart job waits until no live lock is left. A
lock whose pid is dead or that is older than 45 minutes is stale and
ignored, and the restart waits at most 45 minutes; `--force` does not wait.
A lock instead of moving one of the two times: the order then does not
depend on how long a compaction takes or on the restart's schedule.
Implementation: `core/src/compaction_lock.rs`,
`messaging/whatsapp/src/compaction_lock.ts`.

Code: `ChatActor.compact` / `ChatEngine.compactAll` (WhatsApp),
`SessionPool::daily_compact` and `Session::compact` (Rust, used by the
Discord bot and the dashboard chat). The summarize-and-respawn path
(`rotate`, `daily_rotate`, the priming preamble) was removed.

The sections below record the design of 2026-07-18; where they say
"rotation", the job is now the compaction described above.

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
>
> Verification: splitter covered by the shared vector suite on both
> sides (21 Rust / 14 TS tests green); degradation path guarantees
> pre-flush behavior on format-ignoring replies. First live proof is
> the next 4 am rotation — look for `memory_flush <chat>` entries in
> the whatsapp/discord diaries of 2026-07-19.

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
