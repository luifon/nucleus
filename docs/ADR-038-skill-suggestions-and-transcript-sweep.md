# ADR-038 — Skill suggestions and the transcript sweep

**Status:** Accepted + built (2026-11-03; patches as edits and section
requests 2026-11-04; completed-turn reading, candidate workflows, the
backlog check, and suggestions from every pass 2026-11-07). Supersedes arm
1 (the on-the-fly review) of ADR-017 and the direct writes of its arm 2.

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

The first version of the sweep (2026-11-03) left three more gaps, found on
2026-11-07:

4. It skipped every transcript modified in the 30 minutes before the run
   (`quiet_minutes`). The WhatsApp DM chat is one long-lived session, and
   its 04:00 `/compact` or the bots' 04:00 restart (which writes close
   records without timestamps) touches its transcript every night, so the
   04:30 run found it "in use" every night and stopped reviewing it.
5. Each part was reviewed alone. A workflow the operator repeats once a
   day looked like a one-off to every review.
6. The diary gap pass and the curator still wrote skills directly, without
   a notice, so the operator learned about a new skill only by finding it.

## Decision

### Suggestions

A suggestion is a proposed change to the skill library that waits for the
operator. It is the only way a model pass changes the library: the
sweep, the backfill, candidate promotion, the diary gap pass and the
curator all file suggestions, and none writes a skill.
`nucleus_core::skill_learner` owns `memory/skill_learner.db`
(ADR-020: one program writes it, every state change is one `BEGIN
IMMEDIATE` transaction that re-checks the status it changes). A suggestion
has a kind (`create`, or `patch` = a change to an existing personal
skill), the skill name, the full draft (for a patch, the full SKILL.md
that results from its edits; see "Patches are edits"), a rationale, an
origin (`live`, `backfill`, `manual`, `candidate`, `gap`, `curator`),
evidence rows (session id, source `transcript` / `index` / `manual` /
`diary` / `library`, agent label, short quotes), the skills it retires
(curator consolidations only; see "Gap pass and curator"), a status
(`pending`, `accepted`, `rejected`, `superseded`), the operator's reject
reason and timestamps. Migration 4 rebuilt `suggestions`,
`suggestion_evidence` and `review_runs` to widen their CHECK constraints
(on one connection with foreign keys off, one transaction, a foreign key
check before the commit, ids and AUTOINCREMENT counters kept).

- A patch stores the SHA-256 of the live SKILL.md it was written against.
  Accept refuses it when the live file has a different hash.
- Every draft passes `skills::validate` (the ADR-017 contract with
  `# Failure modes`), a strict YAML parse of the frontmatter, a kebab-case
  class-level name check, a credential scan (`secret_filter`) and a size
  limit, when it is filed and again at accept. The limit is 24 KiB for a
  `create` (`MAX_DRAFT_BYTES`: the reviewer writes a new skill whole in one
  answer) and 64 KiB for the result of a `patch` (`MAX_PATCH_BYTES`: the
  live file plus edits may be larger than any new skill; the ceiling keeps
  a skill that sessions load from growing without a bound). Code sets the ownership
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
  base-hash check through the rename and the archive moves of the skills
  it retires; the learner's stale-skill archive (no model) and the
  dashboard's skill moves hold it too. So no other writer changes a skill
  between an accept's hash check and its write.
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

### Completed turns

Every run reads each transcript from its ledger position to the end of
its last completed turn, whether or not the session is in use. Claude Code
writes a `{"type":"system","subtype":"turn_duration"}` record when a turn
ends (the record the turn tracker and the WhatsApp engine use, ADR-033);
`claude_session::transcript_events` reports each one with its line and
time. Reading stops after the last of them. What follows it stays for a
later run: a turn still running, an interrupted turn (Claude Code writes
no `turn_duration` until the next prompt's turn ends; measured on this
workspace's transcripts, 27 of 28 interruptions), the bots' 04:00
`/compact` (a compaction outside a turn writes none; 14 of 14), and the
close records written when a session ends (`last-prompt`, `cost-state`,
without timestamps). The ledger moves exactly to that point. A session
that runs for weeks gets each day's completed turns reviewed the next
night.

A slash command line such as `/compact` is Claude Code's, not the
operator's: `transcript_events` marks it like a skill body (the same
rule as the turn tracker's), so it counts as no operator turn, and a part
made only of compaction records is filtered.

A session closed or killed in the middle of a turn never writes that
turn's `turn_duration`. When nothing with a timestamp was written for
24 hours (the file's modification time stands in when no record has a
timestamp), no turn can still be running, and the whole file is read. Ten
of the 595 transcripts present on 2026-11-07 end that way.

### Ledger

`reviewed_sessions` records, per session, how far it was read (transcript:
the line after its last completed turn; index: next turn number), the
file size at that point, the hash of the transcript's first line, the
number of text turns before the position, and the outcome (`reviewed`,
`filtered`, `skipped-agent`, `own`, `failed`, `gave-up`). A session that
continues later is read again only from that position; a transcript whose
size did not change is not opened; a transcript whose first line changed
or that has fewer lines than the position was rewritten and is read from
the start. A transcript that grew without a new completed turn triggers
no review. When what it grew by is not conversation (`/compact` records,
close records), the run records its new size and the next run skips it
until it grows again. When a turn is still running after the last
completed one, no size is recorded: the next run reads the file again,
and reads the turn whole once the 24-hour rule above applies, so a turn
killed after a sweep is not hidden by the unchanged-size shortcut. When Claude Code deletes a transcript that was read
only part-way, the backfill continues in the session index after the text
turns already read (the index numbers the same text turns). A part that
did not pass the pre-filter is recorded too. The learner's own review
sessions are recorded as `own`. `review_runs` keeps one row per review
session: the pass (`live`, `backfill`, `promote`, `gap`, `curate`), the
part or the candidate it read, the outcome and what it filed, including
the candidate workflows it opened or added evidence to.

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

Every review uses a fresh `SessionProfile::one_shot_no_tools` session
(no tools, no MCP servers, no skills, `dontAsk`), 300 s ceiling. A review
reads one task: a session part, a candidate workflow to promote, the agent
diaries (gap pass) or the agent-created skills (curator). The session
review's prompt is ADR-017's ported review prompt, changed so the model
writes no files and answers with one JSON object: `create`, `patch`,
`add_evidence` or `revise` items, an optional `need_sections` list, and
optional `candidates` (see "Candidate workflows"). The prompt carries
the library summary, the pending and rejected suggestions, the heading
outline of every personal skill, the whole text of the small personal
skills of the task (loaded in the part, tied to a candidate, or
agent-created for the curator), the open candidate workflows (session
reviews), and the task's data. Everything except the library summary is
between nonce markers (`work::briefs::Fence`) as data.
Code parses the answer strictly (unknown fields are errors), checks every
item, and asks once more when the model asked for skill sections, quoted
skill text it was not shown, or an item failed a check. Items that still
fail are dropped and recorded (`dropped N: …` in the run's detail). Quotes
are kept only when they appear in the part. A create or patch for a skill
with a pending suggestion becomes evidence for that suggestion.

#### Patches are edits

A `create` carries the whole new SKILL.md (`draft`). A `patch`, and a
`revise` of a pending patch, carry `edits` instead, never a whole file:

```json
{"action": "patch", "name": "<personal skill>", "rationale": "…",
 "edits": [{"old": "<exact text of the skill>", "new": "<replacement>"},
           {"append_to_section": "<exact heading line>", "new": "<lines>"}],
 "quotes": ["…"]}
```

`nucleus_core::skill_learner::skill_text::apply_edits` applies the edits in
order to the skill's current text (normalized: no byte order mark, `\n`
line breaks, a final line break). Each `old` must occur exactly once in the
text as it stands after the earlier edits (overlapping occurrences count);
an empty `old`, and an edit whose `old` equals its `new`, are refused. An
empty `new` deletes the text. `append_to_section` names one heading line
that occurs once; `new` is added after the last non-blank line of that
section. Any edit that does not apply refuses the whole item, with every
problem listed. A patch has at most 40 edits.

The result is the full draft. It goes through `prepare_draft` and is
stored exactly like a whole draft, so the store, accept, the base-hash
binding, the dashboard and its diff view do not depend on how the draft
was written. The edits themselves are not stored.

The current text of a skill is its live SKILL.md, bound to the live file's
hash. For a skill with a pending patch suggestion it is that suggestion's
draft, bound to the suggestion's base hash: a `revise` edits the pending
draft, so a revision keeps the earlier change, and a new `patch` item for
that skill only adds evidence. The outline says which text it shows.

#### Section requests

The first prompt shows, for each personal skill, its size and its heading
outline: every `#`, `##` and `###` line outside the frontmatter and fenced
code blocks, each with the size of its section, plus `(frontmatter)` for
the text before the first heading (at most 8 KiB of outlines, those of
the skills loaded in the part first; a skill whose outline does not fit is
named with its size). A section
runs from its heading line to the next heading of the same or a higher
level. A personal skill loaded in the part is also shown whole when it is
at most 8 KiB and fits the room the rest of the prompt leaves (at most
12 KiB of such texts).

The model may answer with `"need_sections": [{"skill": "<name>",
"heading": "<exact heading line>"}]` (at most 12; without `heading` for a
whole skill of at most 8 KiB). The second prompt is a separate typed
message with its own 64 KiB limit; it includes the requested sections that
were not shown yet, as data blocks, until the prompt would exceed the limit,
and lists the requests left out (for length, unknown heading, a skill too
large to show whole, not a personal skill), so the model knows what it did
not see. A skill of about 40 KB fits in one second prompt whole by
sections.

While a second answer can still follow, every `old` and every
`append_to_section` heading must be text the model was shown (an outline
heading line, a section, a whole skill, or the `new` of an earlier edit of
the same item). A quote of text that is in the skill but was not shown
makes code show the section that contains it in the second prompt and ask
the model to check the edit against it. In the second answer the edits only
need to apply.

Fresh sessions instead of ADR-029's daily session: each review prompt is
self-contained and up to ~60 KB, so one resumed session would carry every
earlier review into the next and reach auto-compaction within a night.
Each review session is labelled `skill-gap-learner` (skipped by the sweep)
and recorded in the ledger as `own`.

The write mode of the first version (`[skill_learner] live_mode =
"write"`, an agentic session that wrote skill files behind a validation
gate) is removed; no model pass writes a skill.

Reviews run one at a time, so each review sees what the previous one filed.
A review that cannot reach its reviewer (spawn failure, a usage-limit
banner) stops the pass without moving the ledger. An answer that is not the
required JSON, twice in one review, counts a failure against the part;
after two failed reviews the part is given up.

### Candidate workflows

One review sees one part, so work the operator repeats once a day looks
like a one-off to each review. A session review may therefore name
*candidate workflows* next to its items: work in the part that is likely
to be asked for again on other days, when the part alone does not warrant
a suggestion. An entry is either `{"candidate_id": <open id>, "note": …}`
or `{"name": "<kebab-case>", "description": "<one line>", "skill":
"<personal skill it belongs to>", "note": …}` (at most 5 per review; a
name that matches an open candidate adds evidence to it). Code records the
evidence: session id, source, agent, the part's position range, and the
local date (`NUCLEUS_TZ`) of the turn the part ends in. A transcript part
never spans two local days: parts are also split where the completed
turns change date, so a part's date is the date of all its turns.
`nucleus_core::skill_learner::candidates` owns the `candidates` and
`candidate_evidence` tables (migration 5); the same part is recorded once
per candidate. Later session reviews see the open candidates as a
code-built list (id, name, description, the days seen, within 3 KiB, as
data) and add evidence instead of opening a duplicate.

Code, not the model, decides what happens next, in a step after the
backfill (`promote`):

- An open candidate with no new evidence for `candidate_expire_days` (14)
  becomes `expired`; the row stays.
- An open candidate with evidence on `candidate_min_days` (2) distinct
  local dates is promoted. Evidence of one day counts once, however many
  sessions show it. Code reads the session parts at its evidence positions
  (at most 6, every day first, within 24 KiB together) and a review drafts
  exactly one item from them: a `create`, a `patch` of the skill the
  candidate is tied to, or a `revise` / `add_evidence` of a pending
  suggestion that already covers it. The item goes through the same checks
  and filing as every review (the SKILL.md contract with `# Failure
  modes`, the rejected-before rule, one pending suggestion per skill,
  supersede on revise). The suggestion has origin `candidate` and one
  evidence row per session the candidate was seen in. The candidate
  becomes `promoted` and points at the suggestion it became or added
  evidence to. An unusable answer or an item that fails its checks counts
  a failure; after two the candidate becomes `failed`. At most
  `promotions_per_night` (5) promotions per run.

The run's summary line counts the candidates opened, the evidence added,
the promotions (with the suggestion ids), the failures and the expiries.

### Gap pass and curator

ADR-017's diary gap pass and curator file suggestions like the sweep
does, through the same review (no tools, JSON items, the same checks):

- The **gap pass** reads the last days of every agent's diary (routine
  entries removed, newest day first, each agent's day cut to 4 KiB, at
  most 24 KiB; the days left out are named) and files `create` or `patch`
  suggestions with origin `gap`. Its evidence row has source `diary`.
- The **curator** runs when there are two or more agent-created, unpinned
  personal skills. It files the consolidation as a `patch` of the umbrella
  skill (or a `create` of a new umbrella) with origin `curator`, evidence
  source `library`, and an explicit `retires` list: the siblings whose
  content the umbrella absorbs. Code accepts a `retires` only from the
  curator and only names from its agent-created list, and checks each
  name when the suggestion is filed and again at accept
  (`suggestions::check_retires`): a real personal skill directory, not a
  symlink, frontmatter that parses as strict YAML (a retire fails closed
  instead of trusting the lenient parser's reading of `pinned`),
  `created_by: agent`, not pinned, not the suggestion's own skill, at most
  10. Accept writes the umbrella's draft, then moves each
  retired skill into `.nucleus/.claude/skills/.archive/<name>-<time>` under
  the library lock (never a delete; a retired skill already gone has
  nothing to move), and supersedes the pending suggestions for the umbrella
  and for every retired skill. A retired skill that became hand-written or
  pinned blocks the accept. A revision keeps the pending suggestion's
  `retires` unless it names its own.

The pure stale-skill archive (no model: agent-created, unpinned skills
idle past `archive_after_days`) stays as ADR-017 describes it, now under
the library lock.

### Backlog check

`nucleus skill-gap-learner backlog [--hours 48]` lists the live sessions
the sweep reviews (not skipped for their agent, not the learner's own)
whose oldest completed turn after the ledger position ended more than
`--hours` ago: the session id's first 8 characters, the agent label, the
local time the oldest waiting turn ended, and the number of completed
turns waiting (an abandoned turn counts when it has operator text). It
prints nothing and exits 0 when no session waits that long. It opens every
database read-only and writes nothing; the heartbeat uses it to notice a
sweep that stopped reviewing a session. Backfill sessions are left out:
the backfill reviews them at its own limited rate.

### Passes and schedule

- **Live sweep.** Transcripts modified at or after the live start (an
  ADR-029 watermark, `skill-gap-learner.sweep.live-start`, set to "one day
  ago" on the first run), each read to the end of its last completed turn.
  At most `live_max_reviews` (20) reviews.
- **Backfill.** Older transcripts and index-only sessions, oldest first. At
  most `backfill_per_night` (10) reviews; the pass stops for the night once
  more than `backfill_max_new_per_night` (5) backfill suggestions were filed
  since local midnight. That limit applies only to the nightly run; a
  backfill the operator starts from the terminal (`backfill --limit N`)
  stops only at its review limit. Progress is the ledger; the backfill ends
  when no old session has unread text.

Both run inside the existing nightly `skill-gap-learner learn` job (04:30),
before candidate promotion, the stale-skill archive, the diary gap pass
and the curator, with no new launchd job: suggestions wait for the
operator anyway, so a shorter interval would only spread the review cost
over the day without letting anything happen sooner. A step that fails
does not stop the later ones (promotion is skipped when the sweep could
not reach its reviewer); the run exits with an error afterwards.
`nucleus skill-gap-learner sweep|backfill|promote [--limit N] [--dry-run]`
runs one step by hand. `--dry-run` prints, per session, the lines it would
read and its completed turns, and which parts would be reviewed (or which
candidates would be expired or promoted); it works on a private copy of
the store (`skill_learner::open_snapshot`: `VACUUM INTO` through a
read-only connection, then the copy is migrated) and reads the watermark
read-only, so it writes nothing. A file lock
(`memory/skill-gap-learner.lock`) keeps two runs from reviewing the same
session.

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
drafting, so it cannot replace a version the author did not see. A manual
patch is either a full file (`--file`) or a JSON array of edits in the
format above (`--edits`); the edits are applied to the live file, which
must still have the `--base` hash. The
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
  suggestion was written. A curator consolidation lists the skills it
  retires, and the accept confirmation names them; the result lists the
  `.archive/` directory each one moved to.
- `suggestions::review` returns, while a suggestion is pending, the reasons
  accept would be refused now. Review and accept call one function for
  these checks (`plan_accept`: name, real directories, the skills it
  retires, an earlier completed write, the base hash, `prepare_draft`),
  so the page shows accept's
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

Once per nightly `learn` run, at its end, when its passes (sweep,
backfill, candidate promotion, gap pass, curator) filed or revised
suggestions that are still pending, one message is queued to the
operator's DM; a step that failed does not prevent it: `[skill_learner.texts] notice_one` or `notice_many`, with
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

Removed 2026-11-07: `[skill_learner] quiet_minutes` and the "in use" count
of the run summary (replaced by completed-turn reading), `[skill_learner]
live_mode` and the write path it selected, the diary gap pass's and the
curator's direct writes, and the validation gate that checked those writes
(`gate_touched_skills`, the `.rejected/` quarantine and its alert). A
`nucleus.toml` that still sets `live_mode` or `quiet_minutes` loads; the
keys are ignored.

## Consequences

- Terminal sessions, bot chats and dashboard chats are reviewed every
  night, a long-lived session included, and the text of deleted
  transcripts is reviewed once by the backfill.
- Work repeated over days reaches the operator as one suggestion, with the
  sessions of each day as evidence.
- The operator reviews suggestions on the dashboard or in the terminal,
  and gets one WhatsApp message on a night that produced new ones,
  whichever pass filed them.
- No model pass writes a skill. A skill enters, changes or leaves the
  library (apart from the stale-skill archive) only when the operator
  accepts a suggestion.
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
- **A patch as a whole new SKILL.md** (the first version). The model had to
  see the whole skill and write it back whole inside one typed prompt, so a
  skill of about 15 KB or more could not be patched next to a 24 KiB
  conversation part, and the model filed a new skill instead.
- **A patch as a unified diff.** Line numbers and hunk context are easy to
  get wrong for a model that saw only some sections; an exact quote that
  must occur once is checked directly and fails with a clear reason.
- **A separate schedule for the sweep.** See "Passes and schedule".
- **A quiet period before a session is read** (`quiet_minutes`, the first
  version). A session the bots use every day is never quiet at the run's
  time: its transcript changes at 04:00 every night. The end of the last
  completed turn is a property of the transcript itself and does not
  depend on when the run starts.
- **The model deciding that work repeats.** A review sees one part; the
  number of distinct days is a fact code counts from the evidence.
