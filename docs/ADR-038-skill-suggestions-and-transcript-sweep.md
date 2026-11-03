# ADR-038 — Skill suggestions and the transcript sweep

**Status:** Accepted + built (2026-11-03). Supersedes arm 1 (the on-the-fly
review) of ADR-017.

**Builds on:**
- [[ADR-017]] — the skill-gap learner, its review prompt, the validation
  gate, the curator.
- [[ADR-023]] — the session index (`memory/session_index.db`), the only copy
  of the text of sessions whose transcript Claude Code deleted.
- [[ADR-029]] — watermarks and the daily-session rule.
- [[ADR-020]] — DB ownership, versioned migrations, `SessionProfile`.
- [[ADR-033]] — the caller check (`core/src/caller.rs`).
- [[ADR-036]] — the data fence (`work::briefs::Fence`).

## Context

ADR-017's on-the-fly review ran only in the Discord bot, the WhatsApp chat
engine and the dashboard chat, after `[skill_learner].nudge_interval` (12)
operator turns in one chat. Three problems:

1. Interactive Claude Code sessions in this workspace were never reviewed.
   Most of the operator's work happens there.
2. The turn counter reset at the 04:00 restart and almost never reached 12.
   The run-log of the last 25 days held no on-the-fly review.
3. The curator's gap detection reads agent diaries, and terminal work never
   reaches a diary.

The reviewer also wrote skills directly. The operator wants to decide what
enters the library before it is written.

## Decision

### Suggestions

A suggestion is a proposed change to the skill library that waits for the
operator. `nucleus_core::skill_learner` owns `memory/skill_learner.db`
(ADR-020: one program writes it, every state change is one `BEGIN
IMMEDIATE` transaction that re-checks the status it changes). A suggestion
has a kind (`create`, or `patch` = a full replacement SKILL.md of an
existing personal skill), the skill name, the full draft, a rationale,
evidence rows (session id, source `transcript` / `index` / `manual`, agent
label, short quotes), a status (`pending`, `accepted`, `rejected`,
`superseded`), the operator's reject reason and timestamps.

- A patch stores the SHA-256 of the live SKILL.md it was written against.
  Accept refuses it when the live file has a different hash.
- Every draft passes `skills::validate` (the ADR-017 contract with
  `# Failure modes`), a strict YAML parse of the frontmatter, a kebab-case
  class-level name check, a credential scan (`secret_filter`) and a 24 KiB
  limit, when it is filed and again at accept. Code sets the ownership
  fields: a created skill gets `flavor: learned` and `created_by: agent`; a
  patch keeps the `created_by`, `flavor` and `pinned` values of the skill it
  replaces, so a patch never makes a hand-written skill archivable by the
  curator.
- One pending suggestion per skill name. A suggestion for a name that was
  rejected before is refused unless it names the rejected suggestion
  (`overrides_rejected`) and says what is new (`why_new`).
- A live SKILL.md is read without following a symlink (`O_NOFOLLOW`):
  a symlinked SKILL.md is refused when a patch is filed, shown or
  accepted, so a link cannot make Nucleus show, hash or overwrite another
  file.
- Accept writes `<workspace>/.nucleus/.claude/skills/<name>/SKILL.md`
  (through an exclusively created temporary file and a rename; refused when
  the personal tree, its `.nucleus` parents or the skill directory is a
  symlink), marks the suggestion accepted
  and supersedes every other pending suggestion for the same name. Only
  personal skills can be patched; the committed `.claude/skills/` tree and
  the machine-wide tree are never written. A `create` is refused when any
  active tier has a skill with that name.
- Writes to the personal tree are serialized across programs by an
  advisory lock (`memory/skill-library.lock`): accept holds it from the
  base-hash check through the rename, the learner's writing passes (gap,
  curator, write-mode review) and the dashboard's skill moves hold it too.
  So the curator cannot change a skill between an accept's hash check and
  its write.
- A revision replaces a pending suggestion with a new one that carries its
  evidence; the old one becomes `superseded`. A suggestion the operator
  edited is never revised by the model; new sessions only add evidence.
- An edit whose draft has a different `name:` renames a create suggestion.
  The new name is checked like a new suggestion's name (kebab-case, no skill
  and no other pending suggestion with that name). A patch suggestion keeps
  the name of the skill it changes; an edit that renames it is refused.

The DTOs derive `ts_rs::TS` (Rule 12) so the dashboard can show and decide
suggestions through the same functions.

### Two readers

- **Transcripts.** The JSONL files of this workspace's Claude Code project
  directory. `claude_session::transcript_events` reads text, tool calls and
  tool results with their line numbers; `last_n_turns` is now the text part
  of the same reader, so there is one parser.
- **Session index.** For sessions whose transcript Claude Code has deleted
  (its cleanup period), the index still holds their user and assistant
  text. The learner opens it read-only and reads only those sessions. The
  index no longer counts as rebuildable: `session_index.rs` says so, and a
  test proves that `update_index` and `prune_junk` keep the turns of a
  deleted transcript.

Agent labels come from every attribution source, read-only: `usage.db`
(sticky), the venue DBs, the session index and the run-logs
(`usage::attribution::collect`). Sessions of automated agents are skipped
(`[skill_learner] skip_agents`, prefix match, plus the ADR-023 synthetic
labels). Bot chats and unlabelled terminal sessions are reviewed.

### Ledger

`reviewed_sessions` records, per session, how far it was read (transcript:
complete lines; index: next turn number), the file size at that point, the
hash of the transcript's first line, the number of text turns before the
position, and the outcome (`reviewed`, `filtered`, `skipped-agent`, `own`, `failed`,
`gave-up`). A session that continues later is read again only from that
position; a transcript whose size did not change is not opened; a
transcript whose first line changed or that shrank was rewritten and is
read from the start. When Claude Code deletes a transcript that was read
only part-way, the backfill continues in the session index after the text
turns already read (the index numbers the same text turns). A part that
did not pass the pre-filter is recorded too. The learner's own review
sessions are recorded as `own`. `review_runs` keeps one row per review
session: the part, the outcome and what it filed.

### Parts and the pre-filter

The new text of a session is rendered for a reviewer (operator turns cut at
2 500 characters, assistant turns at 1 500, a tool call as one line, a
failed tool result's first 300 characters, successful results left out)
and split at line boundaries into parts of at most `review_window_bytes`
(default and maximum 24 KiB). One transcript record renders to at
most 4 KiB (the rest is left out with a note; its signals still count).
The prompt is checked against the 64 KiB typed-prompt limit before it is
sent; a part whose prompt would exceed it counts as a failed review. Each part is reviewed on its own; a part after
the first carries the session's opening request.

Code decides which parts get a review session (`[skill_learner]`
thresholds). A part needs `min_user_turns` operator turns (2; 1 for a
continuation) and one of: a skill loaded in the session followed by a
correction; `min_corrections` correcting turns (1; a refused tool call and
an interruption count); `min_error_recoveries` failed tool calls followed by
a success of the same tool (3); `min_tool_calls` tool calls (40). An
index-only part has no tool data: corrections, or `index_min_user_turns`
operator turns (8). The correction patterns (English and Portuguese) were
measured against this workspace's transcripts: phrases such as
"instead of", a bare "wrong" or a swear word alone matched neutral
sentences more often than corrections and are not used.

### Review

Suggest mode uses a fresh `SessionProfile::one_shot_no_tools` session per
review (no tools, no MCP servers, no skills, `dontAsk`), 300 s ceiling. The
prompt is ADR-017's ported review prompt, changed so the model writes no
files and answers with one JSON object: `create`, `patch`, `add_evidence`
or `revise` items, and an optional `need_skills` list. The prompt carries
the library summary, the pending and rejected suggestions, the full text of
personal skills loaded in the part, and the part itself between nonce
markers (`work::briefs::Fence`) as data, as are the pending and rejected
suggestions, the skill texts and the session's opening request. Code parses the answer strictly
(unknown fields are errors), checks every item, and asks once more when
the model needs a skill's text to patch it or an item failed a check. Items
that still fail are dropped and recorded. Quotes are kept only when they
appear in the part. A create or patch for a skill with a pending
suggestion becomes evidence for that suggestion.

Fresh sessions instead of ADR-029's daily session: each review prompt is
self-contained and up to ~60 KB, so one resumed session would carry every
earlier review into the next and reach auto-compaction within a night.
Each review session is labelled `skill-gap-learner` (skipped by the sweep)
and recorded in the ledger as `own`.

Write mode (`[skill_learner] live_mode = "write"`) runs ADR-017's write
path for the live sweep: an agentic session in the day's session writes the
skill files, and the validation gate quarantines a malformed write. The
backfill always suggests. The default is `suggest`.

Reviews run one at a time, so each review sees what the previous one filed.
A review that cannot reach its reviewer (spawn failure, a usage-limit
banner) stops the pass without moving the ledger. An answer that is not the
required JSON, twice in one review, counts a failure against the part;
after two failed reviews the part is given up.

### Passes and schedule

- **Live sweep.** Transcripts modified at or after the live start (an
  ADR-029 watermark, `skill-gap-learner.sweep.live-start`, set to "one day
  ago" on the first run). Transcripts modified in the last `quiet_minutes`
  (30) are still in use and wait. At most `live_max_reviews` (20) reviews.
- **Backfill.** Older transcripts and index-only sessions, oldest first. At
  most `backfill_per_night` (10) reviews; the pass stops for the night once
  more than `backfill_max_new_per_night` (5) backfill suggestions were filed
  since local midnight. Progress is the ledger; the backfill ends when no
  old session has unread text.

Both run inside the existing nightly `skill-gap-learner learn` job (04:30),
before the diary gap pass and the curator, with no new launchd job: the
sessions are quiet at that time (the bots compact at 04:00), suggestions
wait for the operator anyway, so a shorter interval would only spread the
review cost over the day without letting anything happen sooner. A pass
that fails does not stop the gap pass and the curator; the run exits with
an error afterwards. `nucleus skill-gap-learner sweep|backfill [--limit N]
[--dry-run]` runs one pass by hand (`--dry-run` prints which parts would be
reviewed and records nothing). A file lock (`memory/skill-gap-learner.lock`)
keeps two runs from reviewing the same session.

### Terminal surface

`nucleus skills suggestions list|show <id>|accept <id> --draft
<sha256>|reject <id> [--reason]|edit <id> --file <path>` and `nucleus skills suggest --kind
create|patch --name … --rationale … --file …`. Accept, reject and edit are
the operator's only (the ADR-033 caller check: a terminal or an operator
session, not a turn reacting to an agent message). Accept takes the
SHA-256 of the draft the operator read (`show` prints it; the dashboard
sends the hash of the draft on screen) and compares it with the stored
draft inside its transaction, so an edit or a revision made after the
operator read the draft is never accepted unseen. Any session may file a
suggestion; its session id and agent come from its process tree. A manual
patch requires `--base <sha256>` of the SKILL.md its author read before
drafting, so it cannot replace a version the author did not see. The
command has no `--workspace-root`: the caller check reads the caller's
transcript under the configured workspace, and another spelling of the
path would make it look in the wrong directory. CLAUDE.md
Rule 11 tells every session to file a patch suggestion when a loaded skill
turns out wrong.

### Dashboard review

The Skills page (`/skills`) has a Suggestions section above the library
tabs. Its routes (`/skills/api/suggestions`, `…/detail`, `…/accept`,
`…/reject`, `…/edit`) call the same `skill_learner::suggestions`
functions as the terminal commands; the handler only maps their results
and refusals to HTTP. A refusal because of the draft is 422 with each
problem listed (`InvalidDraft`); any other refusal (not pending, the
library lock is held, a symlink) is 409 with core's text. The writes
accept a JSON body only and refuse a request a browser marks as
cross-site, as on the Work and Tasks pages; the dashboard is the
operator's own surface on the tailnet.

- The list shows pending suggestions first (kind, name, origin, evidence
  count, date, the rationale on one line) and the accepted, rejected and
  superseded ones in a collapsed history.
- An open suggestion shows the draft rendered as Markdown (its frontmatter
  as a block above it) or raw, and the evidence: each session with its
  source, date and quoted turns. For a patch it shows the line changes
  from the live SKILL.md and says when the live file changed after the
  suggestion was written.
- `suggestions::review` returns, while a suggestion is pending, the reasons
  accept would be refused now. Review and accept call one function for
  these checks (`plan_accept`: name, real directories, an earlier completed
  write, the base hash, `prepare_draft`), so the page shows accept's
  refusal before the operator tries. The review also carries the draft's
  hash, which the page sends with accept; a draft changed since the page
  showed it is refused with 409 and the operator reloads.
- Accept and reject each take a second inline step (reject with an
  optional reason there); edit replaces the draft in a text area and the
  save is checked like any draft. An accept is committed in the `.nucleus`
  repository like the page's other changes to the private tier; the
  handler holds the library write lock from the accept through that
  commit, so the commit holds no other writer's change.
- `/skills?suggestion=<id>` opens that suggestion.

### WhatsApp notice

After the nightly `learn` run's sweep and backfill, when they filed or
revised suggestions that are still pending, one message is queued to the
operator's DM: `[skill_learner.texts] notice_one` or `notice_many`, with
`{count}` and `{link}` filled by code. The link is
`NUCLEUS_PUBLIC_URL/skills`, or `/skills?suggestion=<id>` when there is
one suggestion; without `NUCLEUS_PUBLIC_URL` there is no link. The URL
stays bare (WhatsApp breaks a link inside `*` markers). Names, rationales,
drafts and quotes are model output and session text and never go into the
message. The row goes through `outbound_queue` with the target `dm`, so
the bot's target policy sends it only to the operator and its secret
filter reads it. The dedup key is the store's random id (`store_meta`,
created with the database by migration 3) and the set of suggestion ids,
so a run repeated with nothing new queues nothing, and a recreated
`memory/skill_learner.db`, whose ids restart, still gets its notices. A
pass that stops on an error keeps the ids it filed before the error, and
they get the notice too. Nothing new pending: no
message. `[skill_learner] notify = false` turns it off. A failure to
queue is logged and does not stop the run.

### Removed

The on-the-fly trigger, with no replacement in the bots:
`skills::fire_skill_review`, `AskResult.review_due` and
`AskResult.transcript_path`, `PoolConfig.review_nudge_interval`, the
`skill_review` capability, `[skill_learner].nudge_interval`, the WhatsApp
`fireSkillReview` / `reviewNudgeInterval` / `skillNudgeInterval`, and the
`skill-gap-learner review` subcommand.

## Consequences

- Terminal sessions, bot chats and dashboard chats are reviewed every
  night, and the text of deleted transcripts is reviewed once by the
  backfill.
- The operator reviews suggestions on the dashboard or in the terminal,
  and gets one WhatsApp message on a night that produced new ones.
- No skill enters the library from a review without the operator, except in
  write mode. The diary gap pass and the curator still write directly
  (ADR-017 arm 2).
- `memory/skill_learner.db` holds the only copy of pending suggestions and
  of the ledger. Losing it makes the next sweep review everything again.
- `memory/session_index.db` must be kept like source data; a full rebuild
  would drop the turns of deleted transcripts.
- Cost: on the transcripts present on 2026-11-03 the pre-filter selects
  about 170 of 390 parts for the backfill, about 17 nights at 10 reviews.

## Rejected

- **Keeping the turn counter next to the sweep.** Two paths for one job;
  the counter never fired in practice.
- **A read-only reviewer with file access** (reading the transcript from
  disk to avoid the prompt limit). Transcript text is untrusted; a session
  with Read could be steered to put other files into a suggestion. The
  no-tools session sees only what code puts in the prompt.
- **One review per whole session.** Long terminal sessions reach several
  MB; parts keep each review within the prompt limit and let a continued
  session be reviewed only for its new text.
- **A separate schedule for the sweep.** See "Passes and schedule".
