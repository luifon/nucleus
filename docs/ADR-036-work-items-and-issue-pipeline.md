# ADR-036 — Work items and the issue pipeline

Formerly called intake: the feature, its module, command, config section, database and dashboard page were renamed to work on 2026-09-26 (see "Amendment: work items after the draft PR").

**Status:** Accepted (2026-09-24) — Implemented (2026-09-24), amended after an adversarial
review (2026-09-24, see "Amendment: review findings"), amended with the hidden-content hold
(2026-09-25, see "Amendment: the hidden-content hold", revised after its review the same day), amended with
operator decisions in plain words and the pull request link comment without approval (2026-09-26, see
"Amendment: operator decisions in plain words"), amended to remove per-item WhatsApp groups, send short
notices with a dashboard link and pass plans whole (2026-09-26, see "Amendment: no WhatsApp groups, short
notices, whole plans"), amended with the dashboard's decision board and dashboard text through the
interpreter (2026-09-26, see "Amendment: the decision board"); live verification pending (real `gh` against the configured repos, the interpreter on
real messages, the one-time group leave on the live account).

**Builds on / changes:**
- [[ADR-033]] — every agent step is a task in the task ledger (`origin =
  pipeline`); the ledger gains a per-task working directory and a tool
  profile. The WhatsApp target policy gains the work groups; the DM chat
  session gains read-only work commands.
- [[ADR-020]] — new DB `memory/work.db` (written only through
  `nucleus_core::work`); new queue table `intake_group_requests` in
  whatsapp.db; new bot-owned tables `intake_groups` and `work_inbound`
  that Rust reads.
- [[ADR-029]] — the GitHub poll cursor is a watermark per repo
  (`work:github:<owner/name>`).
- [[ADR-021]] — operator replies reach an item's agent attributed to the
  operator and the surface they came from.
- [[ADR-011]] — the Work dashboard page follows the tailnet-only threat
  model of the Tasks page (ADR-033 §7).

## Context

Work requests reach the operator from several places: GitHub issues on the
operator's repositories today; email, WhatsApp messages and homelab alerts
later. Each source has its own format, and nothing turned a request into
work that Nucleus could carry out with the operator's oversight.

The operator decided the following, which this ADR implements:

1. One source-agnostic core. Every source is an adapter that turns its items
   into one common event record. GitHub issues are the first adapter. A
   generic `nucleus events emit` command lets scripts add events.
2. GitHub is polled with `gh`, with a cursor per repo; Nucleus has no public
   ingress. The first repos are configured by `owner/name` in the untracked
   `nucleus.toml`.
3. An issue starts work only when it carries the configured label. Only
   people with triage or write access can set labels, so the label proves
   that such a person accepted the issue; the issue author can be anyone.
   Agents read only comments written by repository collaborators.
4. Stages, each persisted, each agent step a task in the ADR-033 ledger with
   parent links: work, eval, refinement (for complex items and feature
   requests), implementation, draft pull request. Nucleus code, not an
   agent, pushes and opens the pull request. Nucleus never merges. The issue
   gets one code-owned comment with the pull request link (at first a comment
   the operator approved; changed by "Amendment: operator decisions in plain
   words").
5. OS sandboxing is deferred. The design keeps it possible: agents do not
   run network steps; Nucleus code does.
6. Every WhatsApp send uses the existing target policy and secret filter;
   the task ledger handles concurrency, cancel and status; the operator can
   ask about items in plain language in the DM.
7. A dashboard page lists the items and shows each item's eval, thread (with
   a reply box), plan, implementation status and pull request.

## Decision

### 1. The common event record and the adapter interface

`core/src/work/event.rs` defines `NewEvent`: `source`, `external_id`,
`project` (the repository, `owner/name`), `kind`, `title`, `body`,
`author`, `labels`, `url`, `state` (`open` | `closed`), `created_at`,
`updated_at`, `raw` (the source's own record, unchanged) and `accepted`
(the adapter's gate decision). The store keeps one row per
`(source, external_id)` in `events`: the first report inserts it; a later
report updates the fields a source may change and returns `Updated` or
`Unchanged`. Timestamps from outside go through `timestamp::to_sortable`.

A polling source implements `SourceAdapter`:

| Method | Purpose |
|---|---|
| `source()` | the `source` value of its events |
| `cursor_key()` | the ADR-029 watermark key of this adapter instance |
| `poll_interval()` | minimum time between two polls |
| `poll(cursor)` | events changed since the cursor, and the next cursor |
| `discussion(event, live)` | the trusted part of the discussion (comments, with ids); `live` checks every author's trust at the source now |
| `live_state(event)` | the event's state at the source now: open, gate label present, title, body, who opened the gate, whether the text changed after the gate |
| `reply(event, body, marker)` | post on the event at its source, without posting twice |

A push-style source (a script, a future webhook receiver) calls
`nucleus events emit`. Adding a source changes neither the store, the
dedup, nor the pipeline. An event from a source without an adapter object
has no discussion and no reply channel; its item closes after the draft PR
without the link comment.

`nucleus events emit --source S --id X --title T [--body - | --body B]
[--project owner/name] [--label L]… [--url U] [--state open|closed]
[--accept]` records an event. `--accept` (the operator's terminal only)
marks it as passing the gate; with a configured repo as `--project` it
becomes an item. A detached process (a launchd script) may emit without
`--accept`. `nucleus events list` shows recent events.

### 2. The GitHub adapter

`core/src/work/github.rs`, through `gh` (`[work.github] gh_bin`; launchd
has no shell PATH, Rule 5). One adapter instance per configured repo.

- **Poll.** `gh api -X GET repos/{repo}/issues` with `sort=updated`,
  `direction=asc`, 100 per page, up to `max_pages`. The first poll reads
  open issues only; later polls read every state with `since=<cursor>`, so
  a closed issue closes its item. The cursor is the newest `updated_at`
  seen, stored as a watermark after every event of the batch is recorded.
  `since` is inclusive; the store absorbs the repeated issue. Pull requests,
  which the issues API also lists, are skipped. The tick polls a repo only
  when `poll_interval_secs` (default 300) have passed since its last poll.
- **Gate.** `accepted` is true when the issue carries `[work] label`
  (default `nucleus`, compared case-insensitively). A poll never creates an
  item. The tick's gate check reads every accepted, open event on a
  configured repo that has no open item and changed since its last check
  live at GitHub (Amendment, finding 1) and creates the item only when the
  gate holds there.
- **Collaborators.** `GET /repos/{repo}/collaborators/{login}`: success means
  collaborator, a 404 means not. Any other failure counts as not trusted for
  that read and is not cached. Answers are cached in work.db for
  `collaborator_cache_secs` (default 3600). The cache serves display and
  polling only: every brief and every action boundary checks live
  (Amendment, finding 1c). Logins outside GitHub's character set are never
  looked up. `discussion()` returns only comments by collaborators and the
  number left out; Nucleus's own comments (they carry the marker below) are
  skipped. A comment list longer than `max_pages` is an error, not a
  shortened list.
- **Reply.** `gh issue comment` with the body plus an invisible marker
  `<!-- nucleus-work:item-<n>:comment -->`. Before posting, the comments
  are searched for the marker, so a crash between the post and its record
  does not post twice.

### 3. Items and stages

An item (`items` table, `#<n>`) belongs to one event and one configured
repo. Its stage is one of:

| Stage | What happens |
|---|---|
| `queued` | Nucleus fetches the repo into its mirror and creates the item's clone detached at the default branch |
| `eval` | the eval agent runs (read-only) |
| `refinement` | discussion with the operator until a plan is approved |
| `implementation` | the implementation agent runs (`code` profile); then Nucleus commits what the agent left uncommitted, checks that the branch has commits, and runs the repo's tests |
| `pr` | Nucleus pushes the branch, opens a draft PR and posts its link on the issue; then the item closes (the `review` stage with an approved comment was removed, see "Amendment: operator decisions in plain words") |
| `closed`, `cancelled` | terminal; the item's group is left and its clone removed |
| `stale` | terminal; the source changed after the gate was satisfied (finding 1); re-adding the label starts a new item |
| `failed` | a step failed; `retry` resumes it |
| `blocked` | the secret guard stopped a publishing step (finding 4); `retry` scans again, `cancel` stops it |
| `held` | the issue text or a comment the item uses has content GitHub's page does not show; no agent runs until the operator releases or cancels the item (Amendment: the hidden-content hold) |

`core/src/work/stage.rs::transition` is the only place that decides a
stage change; it refuses events that do not apply (no skipping from `eval`
to `pr`, no approval outside refinement, terminal stages never change). A
failed eval is retried from `queued`; any other failed stage is retried in
place. `store::advance` applies a transition in one `BEGIN IMMEDIATE`
transaction whose `WHERE` re-checks the stage it moves from and logs the
transition in `item_transitions`.

Conditions from the source stop an item at any time: the event closed (the
issue was closed) closes it; the label removed cancels it at every stage,
including `pr`; a changed title or body makes it `stale`. A
running stage task is cancelled in every case. An event has at most one
open item (not closed, cancelled or stale) and at most one item per gate
event; a new gate event (the label added again, or the issue reopened by a
collaborator) starts a new item.

Every agent step is a task in the ADR-033 ledger: kind `work-eval`,
`work-refine` or `work-implement`, origin `pipeline` (the ledger only,
no delivery), requester `pipeline`, `origin_ref = work:<n>`, parent = the
item's previous task, links `work=#<n>`, `event=…`, `issue=owner/name#12`.
The ledger's concurrency limit, heartbeat, runtime limit, sweep and cancel
apply unchanged. The steps Nucleus code performs (clone, commit, tests,
push, PR, comment) are logged as item transitions, not tasks.

A step that fails (a fetch, a push, a `gh` error) is retried at the next
tick; the third consecutive failure fails the item (`step_errors`). An error
retrying cannot fix (the repo was removed from the configuration, the work
dir is inside the Nucleus checkout) fails it at once.

### 4. The ledger changes (ADR-033)

`tasks.db` v5 adds `workdir` and `profile`:

- `workdir`: the worker session's working directory (the item's clone);
  it must be an existing absolute directory. The session's transcript and
  `.claude/` resolution follow it; the operator's private skills tree is not
  loaded there.
- `profile` (`WorkerProfile`): `agentic` (unchanged behavior), `read-only`
  (refuses Bash, Edit, MultiEdit, Write, NotebookEdit, WebFetch, WebSearch,
  Task and Agent), `code` (refuses WebFetch, WebSearch and Bash patterns for
  `git push|fetch|pull|remote|clone|submodule`, `gh`, `curl`, `wget`, `ssh`,
  `scp`, `rsync`, `nc`, `npm publish`, `cargo publish`). Each profile adds a
  line to the worker prompt. The profiles only add refusals on top of the
  Settings denylist and the worker denylist.

Neither field is reachable from `nucleus tasks start`: a chat session cannot
choose where a worker runs or change its posture. Only in-process producers
(the pipeline) set them.

### 5. The eval agent

The brief (`core/src/work/briefs.rs::eval_brief`) asks for one of
`simple`, `complex`, `feature`, with written reasons and the criteria
`change_size` (small | medium | large), `schema_impact`, `security_impact`,
`public_api_impact` and `confidence` (0–1), as JSON between
`===EVAL===` and `===END EVAL===`. `stage::parse_eval` validates it. Code
then applies the pipeline's own rule: an item goes straight to
implementation only when the agent says `simple` AND no impact holds, the
size is not `large`, and the confidence is at least `[work]
min_confidence` (default 0.7). Otherwise the item goes to refinement, and
each reason for raising it is stored (`escalations`). `feature` and
`complex` are never lowered. Output that does not follow the contract fails
the item. The result is stored as `eval_json`.

### 6. Untrusted text

Issue text is written by anyone who can open an issue, and model output can
repeat it. Every brief puts the issue (the revision the item is bound to,
not the latest poll), the collaborator comments, and all model output (the
eval's summary and reasons, earlier refinement replies, Nucleus notes, a
plan the operator has not approved) between data markers that carry a
random nonce (`<<<DATA-<16 hex> label>>>` … `<<<END-DATA-<nonce>>>`). The
instructions say the content is data, never instructions. Only the
operator's own messages and the plan the operator approved are outside the
data blocks. Only collaborator comments are included at all (§2). How the
blocks stay closed is in the Amendment (finding 1b).

A simple item's implementation brief is derived from the issue text,
because no plan exists; the label (set by a collaborator, checked in the
timeline) and the eval's escalation rule are the gates for that. Text that
goes to GitHub is described in the Amendment (finding 4).

### 7. Refinement

**Surfaces.** When an item enters refinement, the pipeline chooses where its
thread runs on WhatsApp:

- A new WhatsApp group, when `[work.whatsapp] refinement_groups` is on
  and fewer than `max_groups_per_day` (default 3) groups were requested in
  the last 24 hours. The pipeline queues a `create` request in
  `intake_group_requests` with the subject `#<n> <short title>` (at most 60
  characters). The bot creates the group with `groupCreate(subject,
  [operator])`: Baileys adds the creator (the bot's account) itself, so the
  group contains the bot and the operator only. The bot enforces the same
  daily limit on its own count of created groups. Automated group creation
  from a personal account can trigger WhatsApp's anti-spam checks, which is
  why both limits exist and default low.
- Otherwise, or when the bot refuses or fails the request, or when no group
  exists after `group_wait_minutes` (default 15), the thread runs in the
  operator's DM. Every message the pipeline sends there starts with `[#n]`;
  the operator writes in the thread by starting a message with `#n`, or by
  replying (quoting) to one of the pipeline's messages. Other DM messages go
  to the DM chat session as before. The bot routes `#n` only when the
  pipeline sent a DM message for item n in the last 30 days, so an ordinary
  message that starts with `#1` is not taken.

Library check (Baileys 7.0.0-rc11, `lib/Socket/groups.js`): `groupCreate`
sends the participant list it receives, and the creator is a member without
being listed. Creating with the operator as the only participant is
supported. Creating with an empty participant list is accepted by the
library (it sends a create node with no participants); whether WhatsApp's
server accepts that was not tested, and the design does not need it,
because the bot runs on its own number and the operator must be a member.

When the item closes, is cancelled or goes stale, the pipeline queues a
`close` request (also while the group is still being created); the bot
waits until the group has no unsent outbound rows (at most 10 minutes),
leaves the group and removes it from the target allowlist. It does not
archive the chat: Baileys' archive call needs the chat's last messages, and
leaving already ends the thread for the bot. Claims, retries and
confirmation are in the Amendment (finding 6).

The same thread is on the dashboard (§10). Every thread message is stored in
`item_messages` (author `operator` | `agent` | `nucleus`, surface `via`),
and every message not from WhatsApp is sent to the item's surface through
the outbound queue (`dedup_key = work:<n>:m<id>`), so the drain's target
policy and secret filter apply. A message to a group has every identifier
redacted by the filter, as for every group.

**Turns.** Each refinement turn is a separate `work-refine` task with the
`read-only` profile in the item's clone. Its brief contains the eval, the
latest proposed plan, the whole thread so far (the most recent 9 000
characters when longer) and the operator messages it must answer; so a turn
does not depend on an earlier session being resumable. The first turn starts
when the item enters refinement (the agent summarizes the request and asks
its questions or proposes a plan). Later turns start when operator messages
arrive and no turn is running; messages that arrive during a turn are read
by the next one. The turn's final message is the reply. A plan is the text
between `===PLAN===` and `===END PLAN===`; Nucleus stores it as the next
plan version and appends the approval hint.

**Approval.** The operator approves in his own words in the item's group or
the DM (who counts as the operator: Amendment, finding 5). A model with no
tools reads the message with a list of pending decisions built by code, and
code decides and runs the approval bound to the plan version the list
showed; see "Amendment: operator decisions in plain words" (it replaced the
whole-message commands `approve`, `approve vN`, `approve comment`, `skip
comment`, `cancel`). The approval is refused when there is no plan, when a
refinement turn is running (its reply may replace the plan the operator
read), or when the plan changed since the version shown. The dashboard's
approve button sends the version shown. The approved plan becomes the
implementation brief.

### 8. Implementation, pull request, comment

Right before implementation Nucleus reads the issue live, fetches,
creates the item's clone again at the newest default branch (a plan
discussed for days is built on current code), switches to
`nucleus/item-<n>`, and reads the issue again before it starts the agent
(finding 3). On a retry the existing clone is kept. The agent works with
the `code` profile, runs the configured `test_command`, may commit locally
(its commits are not published) and ends with a summary for the operator.
Then Nucleus:

1. imports the clone's file tree into its mirror as one commit on the base
   with a fixed identity and a code-owned message (finding 2); none of the
   agent's commits is used;
2. fails the item when the tree equals the base;
3. runs `test_command` itself (`sh -c` in the clone, `test_timeout_minutes`,
   default 30) and records `passed`, `failed`, `timeout` or `not_run` with
   the end of the output;
4. reads the issue live again, scans the diff and the PR text (finding 4),
   pushes the commit to the item's branch, looks for an existing PR for the
   branch (`gh pr list --head`), and otherwise opens a draft PR (`gh pr
   create --draft`) with the code-owned body of finding 4;
5. sends the PR link and the agent's summary to the item's thread, and
   posts one comment with the PR link on the issue (`[work.texts]
   pr_comment`), after a secret scan, the idempotency lookup and a fresh live
   read; then the item closes.

The comment needs no approval (it was approved by the operator before
"Amendment: operator decisions in plain words"). Nucleus never merges and
never marks a PR ready.

### 9. The driver

`nucleus work tick` runs every minute from launchd
(`tools/launchd/work-tick.plist.example`, `StartInterval = 60`), and once
more right after an operator reply (the WhatsApp bot and the dashboard start
it detached). It is a scheduled job rather than part of an existing service
because it does work of its own (git, `gh`, test runs) that must not block
the WhatsApp connection or the dashboard, and because launchd restarts it
from a clean state every minute.

One tick: poll the due sources; check the gate of changed events (and
create items); read the operator messages the bot stored since the last
read; ask the bot to leave active groups whose item is closed; then, per
item, run the stage step, follow an
immediate next step (eval done → implementation task started) at most four
times, resolve the WhatsApp surface, send new thread messages, and clean up
closed items. Ticks take advisory locks (`flock`, released by the OS when a
process exits) under `memory/work-locks/`: `poll`, `inbound`, and one per
item. A tick skips what another holds, so a long test run on one item never
delays the others and no item is advanced by two processes.

### 10. Dashboard

`/work` lists items (open / all) with stage, repo, eval class, what waits
on the operator and the PR link. The detail shows the source event, the eval
(class, criteria, reasons, escalations), the plan with an approve button,
the thread with a reply box (refinement only; also sent to WhatsApp), the
implementation summary, Nucleus's test result, the PR, whether its link is
posted on the issue, the stage tasks from the ledger and the stage log.
Retry and cancel are row actions. Every confirmation uses `InlineConfirm`.
API: `/work/api/{list,detail,reply,answer,approve-plan,cancel,retry,release}`;
wire types are generated (Rule
12). Writes accept JSON bodies only and refuse requests a browser marks as
cross-site, as the Tasks cancel does (ADR-033 §7).

### 11. CLI and the DM session

`nucleus work tick [--poll] | list [--all] [--json] | show <n> [--json] [--hidden] |
reply <n> --text T | approve-plan <n> [--version V] | cancel <n> | retry <n> |
release <n> --hold <code> | group-resolve <n> --left|--absent | interpret-latest`.
The explicit commands stay for the terminal; WhatsApp uses plain words.
`interpret-latest` has the operator's latest stored DM message interpreted
("Amendment: operator decisions in plain words").

`list` and `show` print JSON with `--json`. JSON is for programs and is not
fenced; only when the caller is the WhatsApp DM session is the output (JSON
or text) printed inside a data fence, because a session reads it
(Amendment, finding 1b). `group-resolve` is the operator's only way to end a
WhatsApp group creation whose outcome is unknown (finding 6).

| Caller (`crate::caller`) | May |
|---|---|
| Operator | every command (`group-resolve` and `release` only from the operator) |
| WhatsApp DM chat session | `list`, `show`, `interpret-latest` (not in a turn that read an agent message); it changes no item itself: a cancel, like an approval or a release, is the operator's stored message read through `interpret-latest` and confirmed |
| Detached process (launchd, the bot, the dashboard) | `tick` |
| Workers, other sessions, unscoped chats, unknown | nothing |

The DM persona gets an "Issue pipeline items" section and the pre-approved
patterns `Bash(./target/release/nucleus work list|show|interpret-latest:*)`. The
session answers questions about items in plain language and tells the
operator where to decide (the item's group, or a DM message that starts
with `#n` or replies to an item's message); it cannot approve, release,
reply in a thread, or retry.

### 12. Write ownership (ADR-020)

- **work.db** — every write through `nucleus_core::work` inside the
  `nucleus` binary (CLI, tick, the dashboard's write routes). Dashboard reads
  use `open_read_only`. The schema has one version: the feature was never
  deployed before its review rounds, so no work.db and no whatsapp.db
  work table existed anywhere, and the intermediate migrations were
  collapsed (round 4). The dashboard shows empty lists, refuses writes and
  creates no work.db when work is disabled or the database is missing,
  and reads an empty or half-created database (no schema version recorded,
  or no item tables) as no items (round 5).
  Later migrations: 2 adds the hidden-content hold, 3 removes the `review`
  stage and 4 adds `confirmations` ("Amendment: operator decisions in plain
  words").
- **whatsapp.db** — Rust inserts into `outbound_queue` (thread messages) and
  the new queue table `intake_group_requests`; it reads the bot's
  `intake_groups` (group state) and `work_inbound` (operator messages,
  read past a watermark kept in work.db, with a processing state per
  message in work.db's `inbound_commands`; a row is interpreted only when
  the bot marked it `sender = 'operator'`; rows with `item_key = chat` are
  read only by `interpret-latest`). Rust also writes the one-row
  `work_chat_block` (the DM chat session's list of waiting decisions). The bot owns the schema of all
  three work tables (`messaging/whatsapp/src/work.ts`);
  `whatsapp_queue::open` creates the queue table only so a producer works
  before the bot booted.
- **Mirrors and item clones** live under `[work] clones_dir` (default
  `~/nucleus-work`), which must be outside the Nucleus checkout.

### 13. Configuration

`[work]` in `nucleus.toml` (see `nucleus.toml.example`): `enabled`,
`clones_dir`, `label`, `min_confidence`, `test_timeout_minutes`,
`commit_author_name`, `commit_author_email`, `import_max_files`,
`import_max_file_bytes`, `import_max_total_bytes`, `scan_max_bytes`,
`hidden_content_hold` (default true), `[[work.repos]]` (`repo`, `test_command`, `default_branch`,
`pr_issue_keyword`), `[work.github]` (`gh_bin`, `poll_interval_secs`,
`collaborator_cache_secs`, `max_pages`, `remote_url`), `[work.whatsapp]`
(`refinement_groups`, `max_groups_per_day`, `group_wait_minutes`),
`[work.texts]` (every operator-facing text). The repos name the
operator's projects and live only in the untracked file. The operator is
the first `WHATSAPP_ALLOWED_DM_JIDS` entry, which must be a phone number
(groups are created with it).

## Threat model

The checks here are the ADR-033 ones (they decide what a caller may do
through the Nucleus CLIs; they are not isolation) plus:

- **Who starts work.** Only an issue whose label a collaborator added (read
  in the timeline, trust checked live), with text unchanged since. The item
  works from that text only. Removing the label cancels the item at every
  stage; changing the text stops it (`stale`).
- **What an agent reads.** The issue text (fenced as data) and collaborator
  comments (fenced as data). Other comments never reach an agent.
- **What an agent can do.** Eval and refinement agents cannot run commands,
  edit files or use the web. The implementation agent edits and runs local
  commands in its clone; the `code` profile refuses the ordinary network
  commands and web tools. A determined agent could still write a program
  that opens a socket, or edit files outside its clone: that needs an OS
  sandbox, which the operator deferred. Because every network step is
  Nucleus code, adding a sandbox later means only denying the agent's
  process network and filesystem access outside the clone.
- **What reaches the outside.** Nucleus pushes only the item's branch (one
  commit it collected, to the configured URL, never the default branch),
  opens only draft PRs with code-owned text, never merges, and posts one
  code-owned comment with the PR link; the pushed diff, the PR text and the
  comment pass the repository's secret guard first. WhatsApp
  messages go through the outbound queue (target policy, secret filter);
  work groups are sendable only while active, and only through the drain
  (the TypeScript queue writers `ack.ts` and `enqueue-media.ts` still accept
  only configured groups).
- **Who approves.** Only a message from the operator's own identity (in
  the operator's DM or in an work group whose membership still matches
  the create response), read by the no-tools interpreter and decided by
  code, the operator's terminal, or the dashboard (tailnet only). A voice
  note or a forwarded message decides only after the operator confirms a
  fixed question. A chat session, a worker or an agent message cannot
  approve (see "Amendment: operator decisions in plain words").

## Verification

- Unit (Rust, `cargo test -p nucleus-core work`): stage transitions
  (allowed and refused), eval parsing and escalation, plan extraction,
  group budget, subjects and branch names; event dedup and
  validation, one item per event, guarded stage changes, thread dedup and
  read marks, collaborator cache; GitHub issue conversion and gate, poll
  cursor and state selection, collaborator filtering (404, other failures,
  cache), reply without double posting; git mirror / clone / branch / commit /
  push cycle, test runs with status and timeout, work dir outside the
  checkout; brief fencing and size; the pipeline against a fake `gh`, a bare
  remote and a launcher that starts nothing: a simple issue to a draft PR and
  the posted link comment, a feature refined in a group with refused and accepted
  approvals, dashboard replies, the group budget and fallbacks, the source
  stopping items, step retries and failure recovery, unknown items and
  duplicate messages, polling with the interval, item locks. Tasks: working
  directory and profile validation. CLI authorization. Dashboard: missing
  DBs, JSON-only and same-origin writes.
- Unit (TypeScript, `src/work.test.ts`): group budget, DM routing by marker
  and by quoted message, inbound dedup, group creation with the operator only
  and within the budget, failure fallback without retry, leaving only after
  the last messages, work groups in the target allowlist, `[[…]]` TOML.
- Web (`src/lib/work.test.ts`): action availability per stage, stage
  colours, labels.
- End to end with real tmux + claude sessions (opt-in):
  `cargo test -p nucleus --test work_pipeline_it -- --ignored --nocapture`
  runs `nucleus work tick` against a fake `gh` (shell script) and a local
  bare remote: the eval (read-only) and implementation (code) agents run in
  `nucleus-test-work`; the item reaches a draft PR with passing tests; the
  non-collaborator's comment is absent from every brief; exactly one link
  comment is posted after the PR, without approval; every thread message is
  queued for the DM with the `[#1]` marker.
- The first end-to-end runs found two defects in shared session code, both
  fixed with this ADR: transcript folder names replaced only `/` (Claude Code
  replaces every character that is not a letter or digit, so a worktree
  path with `_` or `.` broke typed-input verification), and the run-log was
  written under the session's working directory (a `memory/` folder
  appeared in the target repo and would have been committed). Sessions now
  take `state_root` for Nucleus's own files.
- Not verified here (needs the live environment): polling the real
  repositories with the operator's `gh` login, real draft PRs and comments,
  and WhatsApp group creation and leaving on the live account.

## Amendment: review findings (2026-09-24)

An adversarial review of the first implementation found seven problems.
This section describes the behavior that replaced each one and what is
still not covered.

### Finding 1 — untrusted edits reached an acting session

**(a) Revision binding.** A poll only records events. The tick's gate check
reads each candidate issue live: the issue, its timeline (`GET
/repos/{repo}/issues/{n}/timeline`) and its body's last edit time (GraphQL
`lastEditedAt`). The gate holds when the latest `labeled` event for the gate
label has no `unlabeled` after it, the account that added it is a
collaborator (checked live), and neither the body nor the title (a
`renamed` event) changed after that label event. When a collaborator
reopened the issue after the label event, the reopen is the gate event and
its account must be a collaborator too. The item stores the title and body
it was admitted with (`rev_title`, `rev_body`), their SHA-256
(`revision_hash`), the gate event (`gate_event_id`, `label_event_id`,
`gate_actor`, `gate_at`), and in `item_comments` the SHA-256 of every
trusted comment a brief used, by comment id. Briefs use only the stored
revision.

Every step compares the latest polled title and body with the revision;
every brief compares the comments it reads with the recorded hashes. A
difference, a used comment that disappeared or whose author is no longer a
collaborator, or (at an action boundary) a different label event moves the
item to the terminal stage `stale` with `stale_reason`, shown by `nucleus
work list` (until a newer item of the same issue replaces it), `nucleus
work show`, the dashboard and a thread note. Re-adding the label produces
a new label event, and the gate check then creates a new item from the
current text. The same label event never produces a second item (unique
index on event and gate event); an event has at most one open item.
`events.gate_note` (shown by `nucleus events list`) records why a gate check
created no item.

**(b) Model output is data.** The eval's summary and reasons, earlier
refinement replies, Nucleus notes and an unapproved plan are inside data
blocks in every brief; only the operator's messages and the approved plan
are outside. The fence cannot be closed from inside: line breaks
(`\r`, U+0085, U+2028, U+2029) become `\n`, the nonce is replaced, a
third consecutive `<` or `>` gets a space before it (so `<<<` never occurs
inside a block), and lines that start with `===` get a `> ` prefix. A long
thread is shortened by whole messages, so no block loses its start marker.

Nothing outside the fence carries issue text either: stage task titles,
which the worker session receives in its header, are code-owned (`Work
item #<n> — evaluation | refinement | implementation`), and so are branch
names (`nucleus/item-<n>`), which the implementation brief names. When the
WhatsApp DM session runs `nucleus work list` or `show`, the whole output
is printed inside a data fence with a code-owned line in front.

**(c) No cached trust at an action boundary.** The collaborator cache is
used only for display and polling filters. Every brief, the gate check and
every action boundary check collaborator status at GitHub, and a failed
check is an error there, not "untrusted".

Limits: the approved plan is model-written text that the operator
approved; it is the implementation brief as written. Any edit of a used
comment stops the item, including a harmless one. The body-edit check
relies on GitHub's `lastEditedAt` and timeline; a label removed and added
again within one poll interval is seen at the next action boundary (the old
item goes stale) and when the poll reports the issue changed (the new item).

### Finding 2 — the privileged push ran agent-controlled git metadata

Nucleus keeps one bare mirror per repo (`<clones_dir>/<owner>__<name>/mirror.git`)
and one separate clone per item (`item-<n>`; no shared git directory with
the mirror). Before every use Nucleus writes the mirror's `config` again
from a fixed template and removes its `hooks`, alternates, `commondir`,
`config.worktree` and `info/attributes`; a mirror path that is a symlink or
not a repository is created again.

**No configuration the agent can write.** The implementation agent runs as
the same OS user as Nucleus, so it can edit `~/.gitconfig` and
`~/.config/git/*`. Every git command Nucleus runs has
`GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM=1`,
`GIT_ATTR_NOSYSTEM=1`, `core.attributesFile=/dev/null`,
`core.excludesFile=/dev/null`, `core.hooksPath=/dev/null`,
`core.fsmonitor=false`, the credential helper list reset, the `ext::`
transport off, and no inherited `GIT_*` variable. The only configuration
file git reads is the mirror's template.

**Credentials Nucleus owns.** Remotes must be HTTPS (`[work.github]
remote_url`, default `https://github.com/{repo}.git`; a local absolute path
is accepted for tests). The only credential helper is
`!<gh> auth git-credential`, with `<gh>` the absolute path of
`[work.github] gh_bin` (a bare name is resolved through `PATH`). SSH is
not supported: git would need an ssh command, and the only place git reads
one from is a configuration file. Fetch and push name the configured URL,
never a remote name.

**No git against the agent's clone.** After an agent could have written the
clone, Nucleus runs no git command against it: no fetch or `upload-pack`
from it, and no command with its `.git` as git directory. `git::import`
reads only the clone's file tree: `git --git-dir=<mirror>
--work-tree=<clone>` with a Nucleus-owned temporary index, `read-tree` of
the base commit, then `add --all`. Git never reads or stores a path named
`.git`, so the clone's repository (a directory, a file or a symlink named
`.git`) is ignored. Symlinks are stored as symlinks (their target text),
never followed. A clone path that is not a real directory (for example a
symlink to another directory) and a nested repository (a gitlink) are
refused. The clone's `.gitignore` files decide which untracked files are
left out (they can only leave content out). Its `.gitattributes` can name
`filter`, `diff` and `merge` drivers but cannot define them; import checks
that the trusted configuration defines none, so a named driver resolves to
nothing and the file's bytes are imported unchanged.

**One code-owned commit.** Import creates exactly one commit: the imported
tree on the base commit the clone started from (`items.base_sha`, recorded
when the clone was prepared), with author and committer `[work]
commit_author_name` / `commit_author_email` and a code-owned message
(`Implement #<issue>` and a `Nucleus-Item: <n>` trailer). None of the
agent's commits, authors, dates or messages is read or published. A tree
equal to the base fails the item. The commit id is stored (`head_sha`);
exactly that commit is scanned and pushed, with `--no-verify`, no force, to
`refs/heads/<branch>` only. Branch names are code-owned (`nucleus/item-<n>`).
The push is refused unless the branch is the item's own, and refused for
the remote's default branch (read live with `ls-remote --symref` during
preparation) and for `main`, `master`, `develop`, `development`, `trunk`,
`production`, `release`, `gh-pages`.

**Bounded import (rounds 3 to 6).** No git command reads the agent's clone.
Nucleus walks it itself, one directory level at a time: every path
component is opened relative to its parent's descriptor with `O_NOFOLLOW`,
entry types come from `statat` without following symlinks, and `.git`
entries are skipped. Any other letter case of `.git` refuses the import:
git refuses such a path on every file system, even with `core.ignoreCase`
and `core.protectHFS` off, so skipping it would publish the change without
that file and without saying so. Every directory entry counts
against `[work] import_max_entries` (default 1 000 000) as it is read,
before it is stat-ed or kept, so the iterator stops at the limit. A nested
repository is found by a direct no-follow `statat(".git")` in the child
directory. The import probes, in its private scratch directory on the same
file system as the clone, whether the file system folds letter case, and
passes `core.ignoreCase` explicitly to `check-ignore` and to the snapshot's
`git add`: on a case-insensitive file system a rule `secrets/` ignores
`Secrets/token`, a `.GitIgnore` is read as the directory's ignore file,
base-tracked paths are compared case-folded, and a `.GIT` directory is a
repository, as git in the clone would treat them. A case variant of
`.gitmodules` is refused on every file system. Each `.gitignore` is
copied into a private rules directory under its own limit (`[work]
import_max_ignore_bytes`, default 1 MiB, counted toward the total), so a
huge or sparse ignore file is refused without being parsed. Which entries of
the next level are ignored is decided by `git check-ignore --no-index
--stdin -z` (pinned git, trusted configuration, no excludes file) with the
rules directory as its work tree; directories exist there as empty
directories so that directory patterns apply; the paths are written and
the answer read concurrently, the answer through a capped reader, and the
whole exchange is limited to 120 s (on expiry the process is killed and
reaped and the import is refused). An
ignored directory is not entered, unless the base tracks paths inside it;
a file the base tracks is imported even when a rule matches it, as git
does. A directory holding a repository is refused unless it is ignored; a
FIFO, socket or device is refused unless it is ignored. The base tree
listing (`ls-tree -r`) is read through the capped reader, and an oversized
one refuses the import.

Each path to import is then copied into a private snapshot directory next
to the temporary object directory: a regular file is checked on its opened
descriptor (`fstat`: a regular file with one hard link) and copied by
streaming, counting bytes as they are read against
`import_max_file_bytes` (default 10 MiB) and `import_max_total_bytes`
(default 200 MiB), so a sparse file or a file that grows during the copy is
refused when it passes the limit; a symlink is recreated as a symlink from
`readlinkat`, its target length counted against the same limits (checked
before the link is created); the number of paths is capped by
`import_max_files` (default 20 000). A symlink that replaces a directory is
imported as a symlink; nothing behind it is read. `git add` runs only
against the snapshot. Paths stay bytes end to end, so a name that is not
UTF-8 is imported unchanged (APFS itself refuses such names). Staged
submodule entries (mode 160000) must equal the base's, a base submodule
directory that holds a repository is refused, and `.gitmodules` must stay
byte-identical to the base when the base has submodules or either tree has
the file. New objects are written into a temporary object directory (the
mirror's objects as an alternate), packed there, and installed as `.pack`
(and `.rev`) then `.idx` last, each by rename: git ignores a pack without
its index, so a crash leaves nothing half-visible. Scratch directories
(rules, snapshot, index, objects) are removed after every import, and ones
older than six hours are swept when the mirror is opened. Every refusal
blocks the item with its reason.

**Pinned executables (rounds 3 and 4).** `git` and `gh` are resolved once
to canonical absolute paths (symlinks followed to the real file) and pinned
by SHA-256: git at its first use in the process, gh when the pipeline
context opens, before anything a worker wrote is read. Every git and gh
process work starts goes through one function (`Pin::command`) that
checks the hash immediately before it creates the process, local git
operations included; the `gh` that git runs as credential helper is checked
before the git process that may run it; the secret guard script's `git` is
a link to the pinned git. A changed file is refused (the process is not
started) and the item is blocked. With work enabled and a repo
configured, a `gh` that cannot be resolved stops the tick with the reason;
nothing falls back to a bare name. Only canonical paths are stored.

**Bounded test output (round 4).** Nucleus's own test run drains stdout
and stderr at once into a buffer that keeps the last 256 KiB, runs the
command in its own process group, and on timeout kills the whole group and
reaps the shell.

**Leased pushes (rounds 3 and 4).** Before pushing, Nucleus reads the exact
remote ref `refs/heads/nucleus/item-<n>` (`ls-remote`, pinned git, trusted
configuration, configured URL). When it already holds the commit to push (a
push that happened before a crash), `pushed_sha` is recorded and the step
continues without pushing; when it holds another commit Nucleus did not
record, or moved after Nucleus's last push, the item is blocked. The first
push only creates the ref (`--force-with-lease=<ref>:` with an empty
expected value); a later push leases on exactly the recorded commit.

Limits: without an OS sandbox the implementation agent runs with the
operator's user rights and can change files between Nucleus's checks and
its use of them (for example replace the clone directory after the check).
The executable pins cover one process: a same-user worker can replace `git`
or `gh` between two ticks, or change whatever the next tick pins, until the
OS sandbox or a separate OS identity exists.
The `gh` credential helper reads gh's own configuration, which the agent
can also write; a changed gh configuration can at most make authentication
fail or use another account's token for the configured URL.

### Finding 3 — the source gate was stale at the irreversible step

GitHub has no write that is conditional on an issue revision, so the time
between a check and a write cannot be removed; it is kept as short as
possible, and the last live read before a write is its authorization
point. A change at GitHub after that read is not seen for that one write.

Implementation start, the push, the pull request and the issue comment each
depend on live reads (`live_gate`). A step reads first, before its
preparation (mirror sync, clone, import, secret scan, reading the remote's
default branch), and then reads again right before every distinct write,
with nothing between that read and the write:

- the worker start: read, then the task is created and launched;
- the push: read, then the push;
- the pull request: after the push, the PR lookup (`gh pr list --head`,
  so a retry does not open a second PR), then a read, then `gh pr create`;
  the lookup matches only a PR whose head is exactly the item branch in the
  configured repository and whose author is the account `gh` is
  authenticated as (`gh api user`, read once per process);
- the comment: the comments-list lookup (so a retry does not post twice),
  then a read, then `gh issue comment`; each comment has a random 128-bit
  operation id, stored before the post and embedded as an exact marker
  line, and the lookup matches only that exact line on a comment written
  by the authenticated account.

Each read requires the issue to be open, carry the label from the same
label event, set by an account that is a collaborator now, with the bound
title, body and comments: a closed issue closes the item, a missing label
cancels it, anything else makes it stale. A read after the first must also
see the same revision as the first: the issue's `updated_at`, its
`lastEditedAt`, the label event and the (id, hash) of every trusted
comment; a difference means the issue changed during the step, which then
does nothing, records why in `error`, and starts again at the next tick. An
item stopped after its push keeps the record of the pushed branch
(`items.pushed_sha`). Any read failure (network, API error, a timeline
longer than `max_pages`) is a step error: nothing is written; after three
errors the item fails and `retry` reads again. The label removed cancels
the item at every stage, including `pr`. Events without an
adapter (`nucleus events emit --accept`) have no live source; their stored
event is checked instead.

Limit: a gate change between the last read and the write it authorizes
(the time of one API call) is not seen for that write.

### Finding 4 — public pull request text

The draft PR title is `Nucleus #<item>: <issue title>` (one line, `@`
replaced by the full-width `＠`). The body holds only code-owned fields:
`<pr_issue_keyword> #<issue>` (or `Refs owner/name#N` / `Source: …`), the
branch, the changed files as code spans (backticks and control characters
replaced, at most 100 listed), the test command and Nucleus's result, the
pipeline footer, `🤖 Generated with [Claude Code](https://claude.com/claude-code)`
and the item marker. The agent's final message and raw test output are not
published (the agent's message goes to the operator). The issue comment
holds only the pull request link (`[work.texts] pr_comment`; before
"Amendment: operator decisions in plain words" it held an escaped summary
the operator approved).

Before the push, the PR title, the body, the commit's author line and
message, and every line the commit adds relative to its base (with every
file name it touches, and symlink targets) go through the secret guard; the comment goes through it
before it is posted. The guard is `tools/check-secrets.sh` run from the
Nucleus workspace root (`.env` values, the `.claude/secret-strings`
denylist, personal-information patterns, home paths, private skill names)
plus the credential shapes of `secret_filter::CredentialRules`. A finding,
or a guard that cannot run, moves the item to `blocked` with the finding
categories (for example `pii-email`, `env-value`, `credential-…`,
`guard-unavailable`), never the matched text, and a thread note; nothing is
published. `retry` scans again; `cancel` stops the item.

The diff parser counts `---` / `+++` as file headers only outside a hunk
(after `diff --git`, before the first `@@`), so an added line whose content
starts with `++` is kept; the whole bounded raw diff (removed lines
included) is passed to the guard as well, so a parsing mistake cannot hide
an added line. The diff is read through a bounded reader. One longer than `[work]
scan_max_bytes` (default 16 MiB) blocks the item with that reason: it is
never cut and passed to the guard. (The guard script reads its whole input,
so the bound is what keeps its memory bounded.)

Limit: legitimate content that matches a pattern (an email address in the
repository's code) blocks the item; there is no override.

### Finding 5 — WhatsApp approval identity

The operator is the first `WHATSAPP_ALLOWED_DM_JIDS` entry, normalized to
digits. In an work group only messages from that identity are read, with
the same check the other groups use (digits of the phone JID, or the phone
number the connection's LID mapping gives for an `@lid` sender). DM messages
are routed to an item only from the operator's DM; another allowed DM
sender's messages go to the chat session. Each routed message stores how it
was written (`input_kind`: `text`, `voice` for a transcription, `forwarded`);
a decision from anything but `text` is confirmed first ("Amendment: operator
decisions in plain words"; before it, only `text` could be a command). A new group's
membership baseline is set from the create response. A member in that
response that is neither the bot nor the operator starts the group disabled
and alerts the operator in DM; a later change of the member list disables
the group (no message from it is read) and alerts the operator.

Limit: an `@lid` sender whose phone number the connection does not know is
not recognized as the operator, unless that LID is listed in
`WHATSAPP_OPERATOR_LIDS` ("Amendment: operator decisions in plain
words").

### Finding 6 — group lifecycle

The bot claims a request with `UPDATE … SET status = 'creating'|'closing'
WHERE id = ? AND status = 'pending'` before any WhatsApp call. A create
claim stores a random 64-bit recovery nonce in the same statement, so a
`creating` row always has its nonce; the nonce goes at the end of the group
subject (` ~<16 hex>`). `calling_at` is written right before the create
call, after every check that can refuse it.

A failed creation is `fallback` only when nothing was created for certain:
a 4xx answer from WhatsApp other than 408, or a call that was never sent (no
live connection, or a stuck claim without `calling_at`).
Anything else (a timeout, a closed or lost connection, a server error, an
unrecognized error, or a claim older than 10 minutes because the bot
stopped mid-call) is `unknown`. An unknown creation is never repeated,
never treated as closed, and keeps counting against `max_groups_per_day`.

Every two minutes the bot lists the groups it participates in
(`groupFetchAllParticipating`). Exactly one group whose subject ends with
the nonce is `quarantined`: recorded with its JID, never added to the
target allowlist, never sent to, and left (the item's thread moved to the
DM when the creation became unknown); once the bot confirms it left, the
record is `closed`. Zero or several matches, or a failed listing, change
nothing: a search that does not find the group is not evidence that it does
not exist, and WhatsApp gives no definite answer. An unresolved creation is
reported to the operator in DM after one hour and at most once a day after
that. The operator ends it with `nucleus work group-resolve <n> --left`
(the operator left the group) or `--absent` (no such group exists); the
command queues a `resolve` request that the bot applies. That command is
the only way to a closed record without the bot confirming it left.

An item closed before the bot handles its create request gets no group; one
closed while the create call runs gets the new group left at once. Leaving
is retried with backoff (30 s, doubling, at most 1 hour, 8 attempts, then an
alert); a failed leave counts as done when the bot is confirmed not to be a
member. The bot sets `closed_at` only when it left or the operator resolved
the creation. The pipeline sets `group_closed_at` only when the bot's table
shows `closed` or `fallback`, never for `unknown` or `quarantined`. The
pipeline adds a close request only when none is pending, and every tick
asks the bot to leave active groups whose item is closed, missing, or moved
to the DM.

Limit: a creation whose group lost its nonce (the subject was changed by
hand) stays unknown until the operator resolves it.

### Finding 7 — lost operator commands

Every operator message read from WhatsApp has a row in work.db's
`inbound_commands` (`received`, then `applied` or `failed`, with an attempt
count), keyed by chat and WhatsApp message id. A command's effect (the
stage change or item update) and its `applied` mark are one transaction; a
plain thread message is applied by being stored, in one transaction. A
message stored but not applied before a crash is applied on the next read.
The read watermark moves only past messages that are applied or failed for
good. A message that fails for another reason stops the read, so later
messages keep their order, and is tried again next tick; after 5 attempts
it is marked failed and the operator is told in DM.

Limit: the thread note that follows a command (for example "plan approved")
is written after the transaction; a crash between the two keeps the
command's effect and loses the note.

### Verification of the amendment

Round 6: the directory iterator stops right after the entry limit
(`the_entry_budget_stops_the_iterator_at_the_limit`, counter asserted with a
limit of 5); on the test's file system the imported files equal git's own
view in the clone for `Secrets/token` under `secrets/`, `A.TMP` under
`*.tmp` and a `sub/.GitIgnore`, a `.GIT` directory is refused as a nested
repository when the file system folds case, and a `.GitModules` is refused
(`letter_case_follows_the_clone_file_system_like_git`); a hanging
`check-ignore` (a pinned fake git that sleeps) is killed and refuses the
import within the limit (`core/tests/work_check_ignore_timeout.rs`).

Round 5: a 4 GiB sparse `.gitignore` is refused quickly without being
parsed (`a_huge_sparse_gitignore_is_refused_without_being_parsed`); the
imported files match what git adds for a root `.gitignore` with a negation,
a nested `.gitignore` and an ignored directory (which holds a FIFO that is
never reached), and a base-tracked file matching a rule is still imported
(`ignore_rules_match_git_and_tracked_files_stay`); a symlink whose target
passes the per-file or total limit is refused before it is created
(`symlink_targets_count_against_the_limits`); an empty work.db file and one
without the items table read as no items
(`an_unmigrated_database_is_an_empty_list`).

Round 4: an added line `++<guard hit>` blocks the push and the parser keeps
it (`an_added_line_starting_with_plus_plus_is_scanned`,
`added_lines_starting_with_plus_plus_are_kept`); endless test output is
stopped at the timeout with its tail kept and the whole process group
killed (`tests_run_with_a_status_and_a_limit`); a file that grows while
copied, a hard-linked file, a symlinked parent, a sparse file and too many
files are refused (`a_file_that_grows_while_copied_is_refused`,
`the_snapshot_refuses_hard_links_and_symlinked_parents`,
`oversized_trees_and_special_files_are_refused_before_git_add`); a push made
before a crash is recognized and an unrecorded branch blocks the item
(`a_push_before_a_crash_is_recognized_and_not_repeated`,
`an_existing_branch_is_never_overwritten_by_the_first_push`); a changed git
is refused at the first local git call (`core/tests/work_git_pin.rs`) and
a missing gh stops work (`tools.rs`); a forged marker from another author
and a PR by another account are ignored
(`forged_markers_and_foreign_pull_requests_are_ignored`); a claim and its
nonce are one write (`work.test.ts`); a changed submodule URL is refused
(`gitmodules_must_stay_as_in_the_base`); objects arrive as one pack and old
scratch is swept (`objects_arrive_as_one_pack_and_old_scratch_is_swept`);
paths stay bytes (`non_utf8_file_names_survive_an_import`); the dashboard
with work disabled or no database shows nothing and writes nothing
(`disabled_work_shows_nothing_and_writes_nothing`,
`a_missing_database_is_an_empty_list`).

Round 3: a label removed right after the push stops the item before
`gh pr create` and keeps the pushed branch recorded
(`a_label_removed_after_the_push_stops_the_pull_request`); the comment is
posted only after a read that follows the comments lookup
(`the_comment_is_written_only_after_a_fresh_read_that_follows_the_lookup`);
a changed `gh` file blocks the next privileged step
(`a_changed_executable_blocks_the_next_privileged_step`, `tools.rs`);
a file over the per-file limit, a sparse file, too many files, a FIFO and
the total limit are refused before `git add`, with no object or scratch
directory left (`oversized_trees_and_special_files_are_refused_before_git_add`),
and a pipeline item is blocked with the reason, as is a diff over the scan
limit (`limits_block_the_item_with_a_clear_reason`); an unchanged base
submodule is kept and an added, deleted or moved one refused
(`submodules_of_the_base_may_stay_but_not_change`); the first push is
create-only and later pushes lease on the recorded commit
(`pushes_are_leased_on_the_exact_ref`,
`an_existing_branch_is_never_overwritten_by_the_first_push`); `show --json`
parses as JSON (`show_json_is_valid_json`); random nonces, one exact match
quarantined and left, decoys untouched, a match with extra members left,
several matches unknown, daily alerts, operator resolution
(`work.test.ts`), and the pipeline never counts an unknown or quarantined
group as closed (`an_unknown_group_is_never_counted_as_closed`).

Round 2 (after a second review): an issue title with fence-escape and
instruction text reaches the typed worker message only inside the fence
(`an_issue_title_reaches_workers_only_inside_the_fence`, and
`session_output_is_fenced` for the CLI); global git configuration in a
temporary HOME (URL rewrite, ssh command, fsmonitor, hooks path, global
attributes with a required filter, global ignore) has no effect
(`core/tests/work_git_home.rs`); malicious configuration, hooks, filters
and drivers in the agent clone's `.git` and in the mirror are never used,
a nested repository and a clone replaced by a symlink are refused, a `.git`
file is ignored, a symlink is stored as a symlink (`git.rs` tests); an
agent commit whose author, email and message carry a guard-hit value, and
an empty commit, publish none of that metadata, and an empty change
publishes nothing (`only_one_code_owned_commit_is_published`); an issue
changed during the secret scan stops the push, and an edited body during
the scan makes the item stale (`a_change_during_the_scan_stops_the_push`);
a group creation that timed out but happened is found by its token and
left, and an unresolvable one stays unknown and is reported
(`work.test.ts`; round 3 replaced the token with a random nonce and
removed the `absent` result), and the pipeline never counts an unknown
group as closed (`an_unknown_group_is_never_counted_as_closed`).

Round 1:

Rust (`cargo test -p nucleus-core work`, `whatsapp_queue`): the gate
check (collaborator label, edit after the label, rename after the label,
failed reads retried), stale on a polled edit and new item on re-labeling,
reopen by a collaborator versus a non-collaborator, live reads before
implementation (changed body, labeler no longer a collaborator with a
positive cache, a network error, a label added again), before push (label
removed, read failures never push), changed and removed comments, label
removal in review; brief fences (marker-breaking input, model output only
inside fences, approved plan outside); mirror, clone, collect and push
with hooks, fsmonitor, filters and rewritten remotes in both the clone and
the mirror, refused pushes to the default and protected branches, work not
based on the default branch; code-owned PR text, summary escaping, a guard
hit on the diff and on the comment, a guard that cannot run; a command
stored before a crash applied exactly once, a failing message holding later
ones back until given up, voice and forwarded commands refused; the v3
migration keeping rows and foreign keys; the group closed only after the
bot confirms, a group requested for an item cancelled meanwhile,
reconciliation of active groups. TypeScript (`src/work.test.ts`): operator
identity in phone and LID form, unexpected members, claims, unknown
creations not repeated, items closed before and during creation, a
disabled baseline with an alert, leave retries with backoff and give-up,
`closed_at` only after leaving, `input_kind` stored.

## Amendment: the hidden-content hold (2026-09-25)

### Why

The operator reads an issue on its GitHub page before adding the label. The
page is GitHub's rendering of the Markdown; the pipeline reads the raw text
through the API. GitHub's rendering drops some text (HTML comments, link
reference definitions, table cells beyond the header), collapses some
(`<details>`), shows some only on hover (link titles) or only when an image
fails to load (alt text), removes tags and attributes it does not allow, and
draws nothing for invisible characters. An issue author can put an
instruction in one of those places. The operator does not see it, the label
admits the issue, and the instruction reaches an agent that can edit files
and run commands with no OS sandbox (§4, Threat model). The data fence (§6)
tells the agent that the text is data, but it does not tell the operator
that the text exists.

### What is detected

`core/src/work/hidden.rs` scans the raw issue title, the raw issue body,
and the raw body of every collaborator comment the item uses.

**How the Markdown is read (fix round 1).** The body and each comment are
parsed with `comrak` 0.55 (a port of GitHub's cmark-gfm) with the
extensions GitHub enables for issues: tables, strikethrough, autolinks,
task lists, footnotes, `$`/`` $` `` math and alerts, with source positions
on. (The tag filter is a rendering step; the tags it escapes are flagged
anyway.) Line endings are normalized first (CRLF and a bare CR become LF;
GitHub ends a line at a bare CR), and an offset map takes every position
back to the raw text. The syntax tree decides what is code — fenced and
indented code blocks and code spans, at any depth of block quotes and list
items.

Which checks read the tree and which read the text (fix round 2):

- **The tree** (node kind, literal content, source position): fences and
  their info strings, tables, links, images, footnote definitions, math
  (inline, display, `` $`…`$ ``, math fences), and all raw HTML. Every
  `HtmlBlock` and `HtmlInline` node is read from its literal, wherever the
  tree places it (quotes, lists, table cells, any depth): comments,
  forbidden elements and attributes, incomplete or unterminated tags, and
  `<details>`. The literal's positions are mapped back line by line (each
  literal line is a suffix of its source line after the container prefix).
  Reading HTML from raw lines was the round-1 gap: a tag after `> ` or
  `- ` was not at a line start, so an unterminated `<iframe title="…` in a
  quote went unflagged.
- **The text**, with the code the tree found masked, only where the tree
  gives nothing to read: link reference definitions (comrak removes them
  and keeps no node); HTML entities (the tree hands back decoded text, the
  source keeps the entity and its position); `<!--` outside every HTML
  node (a backstop in case GitHub starts a comment where comrak reads
  text); the always-flagged math macros outside the math nodes (in case
  GitHub reads math where comrak does not).
- **Invisible characters** are read from the raw text, code included,
  with every character reference outside code decoded into the same
  character stream, so the emoji and joining rules apply to `&zwj;`
  exactly as to a literal ZWJ.

**Character references (fix rounds 3 and 4).** GitHub decodes references
in two places with different rules, and the hold decodes each range
exactly as its renderer does. Decoding more than the renderer is not
stricter: it changes the characters the exemptions look at (round 4: with
`&copy` decoded in Markdown, `&copy&#xFE0F;` looked like the sequence
©+VS16, while GitHub shows `&copy` as text and the VS16 after `y` is
invisible).

- **Markdown text** (outside code and outside the placed HTML nodes):
  CommonMark decodes only references that end in `;` — `&#` and 1–7
  digits, `&#x` and 1–6 hex digits, or a name of the table below — and
  maps code point 0 and invalid ones to U+FFFD, with no Windows-1252
  remapping (`&#128;` is the C1 control U+0080, flagged). A
  backslash-escaped `&` stays text. An unescaped `*` may be an emphasis
  delimiter the page does not show, so it stands in the character stream
  as U+FFFC, which no exemption accepts as a neighbour.
- **Raw HTML** (the placed HTML nodes): the browser's WHATWG rules
  (`hidden/charref.rs`, "character reference state"): numeric references
  with or without `;`, U+FFFD for 0, surrogates and values past U+10FFFF,
  the Windows-1252 mapping of 0x80–0x9F; every named reference of the
  WHATWG table (`hidden/entities.rs`, generated from
  `https://html.spec.whatwg.org/entities.json`, last modified 2025-11-12:
  2,231 names, 106 legacy names valid without `;`), by longest match; the
  attribute-value rule inside tags (a legacy name without `;` followed by
  `=` or a letter or digit stays text). `<div>ig&#8203nore</div>` shows
  `ignore` with a zero-width space.
- **An HTML node whose position is unknown**: which rule applies is not
  known, so its source is read as Markdown with the rest of the text, and
  its literal is read again with the WHATWG rules; a finding from either
  counts, over the whole location.

Only inline HTML tags are raw HTML: in `<span>&copy&#xFE0F;</span>` on a
line of its own the text between the tags is Markdown text (a `<span>` does
not start an HTML block), so `&copy` stays text and the VS16 is flagged; in
an HTML block (`<div>…</div>`) it is decoded and not flagged. A reference
that produces two characters (`&caps;` is ∩ + VARIATION SELECTOR-1) gives
two characters with the reference's range; the finding lists the hidden
ones. The exemptions that look at neighbours (variation sequences, RGI ZWJ
sequences, joining scripts) all read this rendered stream.

**Positions (fix round 3).** comrak reports shifted positions for the
inline nodes of a paragraph that starts with a reference definition it
removed. Every code span and inline HTML node is checked against the
source at its reported position. When it does not match, the nodes of the
same leaf block (paragraph, heading, table cell) with the same kind and
literal are matched in document order against their occurrences in the
block's source, skipping placed code spans, backslash-escaped `<` and
reference definitions, and multi-line literals are matched line by line.
A reported position counts only when the node is there and the place is
not inside a reference definition (or, for HTML, inside code that checked
out); when any node of a leaf block fails, every node of that block goes
through the matcher (round 4), so a wrong reported position that happens
to hold the same literal inside a definition is never used.
Only a one-to-one match places them. Otherwise nothing is guessed: a code
span stays unmasked (its content is read), and an HTML node's findings say
"position unknown" and cover the whole location. HTML blocks keep comrak's
position (block positions are exact). Tests:
`comrak_positions_after_a_reference_definition_do_not_hide_anything`,
`shifted_html_is_placed_in_document_order_or_reported_unknown`.

**Each finding** stores its location (`title`, `body`, `comment <id>`), its
kind, its raw line and column (characters; CR, LF and CRLF each end a
line), its range in the raw text (`start`..`end`, in characters) and its
complete hidden text, never shortened: invisible characters as code points
(`U+200B ZERO WIDTH SPACE ×2`; tag characters also as the ASCII text they
spell), everything else as its literal source with any invisible character
in it shown as `[U+XXXX]`. The hold also stores the complete raw text of
every location that has findings.

| Kind | What | Why it is hidden |
|---|---|---|
| `html_comment` | `<!-- … -->`, also unclosed, `<!-->`, `<!--->` | not rendered |
| `invisible_characters` | every Unicode Cf character; the other default-ignorable code points; blank-rendering characters (Hangul fillers U+115F, U+1160, U+3164, U+FFA0; U+2800); line and paragraph separators; control characters other than tab, LF, CR; private-use characters | drawn as nothing (private use: a box at most) |
| `invisible_entity` | a character reference that produces one of those, with or without `;` as the WHATWG rules allow (`&#8203`, `&#x2060;`, `&zwj;`, `&shy`, `&caps;`, …) | GitHub decodes it; the page shows nothing |
| `details` | a `<details>` block without `open` (with `open` the block is shown and only its content is checked) | collapsed until clicked |
| `html_tag` | in any HTML node of the tree: a tag outside `VISIBLE_TAGS`; an attribute outside `attribute_allowed`; an incomplete or unterminated tag; nested `<sub>`/`<sup>`; declarations, processing instructions, CDATA | removed by the sanitizer, or hides or restyles content |
| `link_definition` | `[label]: url "title"` (every definition, used or not) | renders as nothing |
| `footnote_definition` | `[^label]: text` | shown only at the page bottom, only when referenced |
| `image_alt` | non-empty alt text (Markdown images and `<img alt>`) | not shown while the image loads |
| `link_title` | `[a](url "title")`, also on images | shown only on hover |
| `link_destination` | a link (Markdown or `<a href>`) whose visible text, normalized, is not its destination | the destination shows only on hover |
| `image_source` | an image address that is not one of the exact attachment shapes below | the page shows the picture, not the address |
| `table_extra_cells` | cells beyond the header's column count, in any container | GFM drops them |
| `math_styling` | in `$…$`, `$$…$$`, `` $`…`$ `` and `math` blocks, a macro outside `MATH_VISIBLE` or in `MATH_ALWAYS_FLAG`, a CSS-like `[…]` argument, or a comment (an unescaped `%` to the end of its line; `\%` is a percent sign); `MATH_ALWAYS_FLAG` macros also outside math | can draw nothing, move, recolor or link text; MathJax skips a comment |
| `rendered_block` | a fence with info `mermaid`, `geojson`, `topojson` or `stl`, in any container | rendered as a picture; the source is not on the page |
| `fence_info` | text after the first word of a fence info string, or a first word outside `[A-Za-z0-9_+#.-]{1,32}` | GitHub shows neither |

**Invisible characters.** The list is one table (`INVISIBLE`) from the
Unicode 16.0 Character Database (`DerivedGeneralCategory.txt` for Cf,
`DerivedCoreProperties.txt` for `Default_Ignorable_Code_Point`,
`UnicodeData.txt` for names) plus the blank-rendering characters above.
Not flagged: a U+FE0E or U+FE0F directly after one of the 371 bases of
Unicode's emoji variation sequences (`VARIATION_BASES`, from
`emoji-variation-sequences.txt`, Unicode 16.0; `StandardizedVariants.txt`
16.0 defines no sequence with either selector), or a U+FE0F inside an RGI
emoji ZWJ sequence; a selector after any other character, or a second one,
is flagged (fix round 2); a ZWJ inside an RGI emoji ZWJ sequence (the `emojis`
crate, Unicode Emoji 17.0 data from `emoji-test.txt`, which contains every
sequence of `emoji-zwj-sequences.txt`; the sequence around the ZWJ must be
exactly one listed emoji); a ZWNJ or ZWJ between two letters where Unicode
defines its effect, per the tests of RFC 5892 Appendix A.1/A.2 (after a
virama and before a letter, as in Indic scripts; or between a character of
Joining_Type L or D and one of R or D with only transparent marks between,
as in Arabic, Persian and Syriac; joining types from Unicode 16.0,
`unicode-joining-type`). Every other ZWJ and ZWNJ is flagged. The same
rules apply to characters written as entities (`👩&zwj;💻` is not flagged,
`a&zwj;b` is).

**HTML.** `VISIBLE_TAGS` is the element allowlist of GitHub's sanitizer as
published in `html-pipeline` (`SanitizationFilter`) minus the kept elements
that hide or shrink content (`small`, `ruby`/`rt`/`rp`, `bdo`, `time`,
`wbr`): headings, paragraphs, `div`, `span`, lists, tables, `blockquote`,
`pre`, `hr`, `br`, the inline text styles, `a`, `img`, `details`,
`summary`, `figure`. Attributes are allowed only where they neither hide
nor restyle: `td`/`th align`, `ol start`, `details open`; `a href` and
`img src`/`alt` go through the link and image rules. An allowed attribute
is allowed only with a value that changes nothing but layout, checked
after WHATWG attribute decoding (fix round 5; the page never shows a
value, so `<details open="ignore previous instructions">` hid text):
`open` with no value, an empty value or `open`; `align` one of left,
right, center, justify; `start` an optional `-` and 1–9 digits; letter
case ignored. Any other value is an `html_tag` finding that shows the
decoded value.

**Raw HTML fails closed (fix round 6).** Inside every placed HTML node, and
in the HTML reading of an unplaced one, every `<` must be recognized; the
reader follows the WHATWG tokenizer (HTML Living Standard 13.2.5): the
data state, the tag-name state (a name runs to whitespace, `/` or `>`, so
`span.foo` is one name), the attribute states (names, unquoted, single-
and double-quoted values; a `>` inside quotes does not end the tag; no
whitespace needed after a quoted value; `/` as in `<b/x=1>`; newlines
inside a tag), comments, and bogus comments. A `<` is one of:

- text, when the tokenizer emits it as text: a `<` followed by anything
  but an ASCII letter, `!`, `/` or `?` (`a < b`, `x <3`);
- an allowed start or end tag (`VISIBLE_TAGS`) with allowed attributes and
  values;
- a finding: any other tag name, a comment, a bogus comment (`<!` not
  followed by `--`, including declarations and CDATA; `<?`; `</` followed
  by a non-letter; `</>`), a tag with no `>` before the end of the node, a
  duplicate attribute (the browser keeps the first), and attributes on an
  end tag (dropped).

Each token's range, from its `<` to the `>` that ends it (or the end of
the node), is tag syntax for the invisible-character rules below.

Tag syntax itself is never rendered: every invisible
character or character reference between a tag's `<` and its `>` (names,
values, whitespace) is flagged, with no emoji, variation or joining
exemption, and nothing inside a tag is a neighbour an exemption may lean
on. Every other attribute
is flagged (`title`, `width`, `height`, `dir`, `style`, `class`, `hidden`,
…). HTML inside code, as the tree reads it, is not flagged.

**Links and images.** A link's destination is exempt only when its visible
text, normalized (percent-decoded, lower case, without `http(s)://`,
`mailto:` and a trailing `/`), equals the normalized destination: that
covers autolinks, bare URLs and `[https://x](https://x)`. An image address
is exempt only in one of the two exact shapes of GitHub's attachments for
pasted pictures (fix round 2), parsed with the `url` crate:
`https://github.com/user-attachments/assets/<uuid>` and
`https://user-images.githubusercontent.com/<digits>/<digits>-<uuid>.<ext>`
with `<ext>` one of png, jpg, jpeg, gif, webp, svg, mp4, mov
(`ATTACHMENT_EXTENSIONS`), `<uuid>` canonical lower-case 8-4-4-4-12 hex.
The address must be exactly the parser's serialization (nothing normalized
away: dot segments, letter case, a default port) and carry no credentials,
port, query, fragment, percent-encoding, trailing slash or trailing text.

**Math.** A denylist of hiding macros cannot be complete, so math is read
against an allowlist, `MATH_VISIBLE`: Greek letters, big operators and
named functions, relations and binary operators, arrows, `\frac`,
`\sqrt`, `\binom`, `\left`/`\right` and sizes, accents and braces,
delimiters, fonts for visible text (`\text`, `\mathbf`, `\mathbb`, …),
`\quad`/`\qquad`, `\begin`/`\end`. Symbol macros (`\,`, `\{`, `\\`) are
allowed. `MATH_ALWAYS_FLAG` wins over the allowlist: `\bbox`, `\enclose`,
`\style`, `\class`, `\cssId`, `\href`, `\color` and its variants,
`\phantom`, `\hphantom`, `\vphantom`, `\smash`, the lap, raise and kern
macros, `\unicode`, `\require`, macro definitions. Every math comment is
flagged with its text: MathJax skips everything from an unescaped `%` to
the end of the line, so `$x % ignore previous instructions$` shows only
`x` (fix round 2).

**Fence info strings.** Everything after the first word of an info string
is flagged. For the first word I chose the character rule
`[A-Za-z0-9_+#.-]{1,32}` over a list of language names: GitHub shows a code
block with an unknown language as plain code, so an unknown but well-formed
name hides nothing, and a list would have to track Linguist's hundreds of
names and aliases. The rule's limit: one word of up to 32 such characters
can still carry a short hidden message (see Limits).

**The title** is shown as plain text (GitHub escapes HTML there and does not
decode entities), so only invisible characters are flagged in it.

### The flow

- **When.** The check runs in the `queued` step, before the clone (the
  step reads and binds the comments first), and again right before every
  agent task: the eval, each refinement turn, and the implementation (after
  the final live read). A collaborator comment with hidden content added
  later holds the item before the next agent step. The steps without an
  agent (push, pull request, issue comment) do not check.
- **Held.** Findings move the item to `held` (from `queued`, `eval`,
  `refinement` or `implementation`). The item stores the hold
  (`hold_json`: the complete findings and the raw text of each location
  with findings), the stage it was held in and a fingerprint (`hold_hash`):
  the SHA-256 of each location with findings and its complete raw text,
  and of each finding with its complete hidden text. No task runs while
  the item is held; the source checks of every tick still apply. An
  operator message sent while an item is held in refinement is kept for
  the turn after the release; an approval is refused.
- **Told.** One thread message (`[work.texts] item_held`: the counts by
  kind, the first three findings shortened to one line, the hold code, and
  where to read everything) goes through the outbound queue, so the target
  policy and the secret filter apply. An item without a WhatsApp thread yet
  gets the DM as its surface. The complete findings and sources are on the
  dashboard's item page (each finding in full, and each raw source with the
  hidden ranges marked) and in `nucleus work show <n> --hidden`.
- **Release, bound to the viewed hold (fix round 1).** Every release names
  the hold the operator reviewed: `nucleus work release <n> --hold
  <code>` (operator terminal only; `show` prints the code and the
  fingerprint), the dashboard's release button (sends the `hold_hash` the
  panel rendered), or the operator's WhatsApp message read as a release,
  which is always confirmed first and bound to the fingerprint the list of
  pending decisions showed ("Amendment: operator decisions in plain
  words"; before it, `#<n> release <code>` typed by the operator). The hold
  code is the first 6 hex characters of the fingerprint. A release without
  a hold, or with another hold, is refused with the current code. The named hold is compared with the current
  `hold_hash` before the live read, and again in the statement that
  changes the stage (`store::advance_if_hold`), so a release of hold A that
  arrives after the item was held again for hold B changes nothing. Then
  the source is read live: a changed title, body or used comment makes the
  item `stale`; a fingerprint that differs for another reason (a new
  comment with hidden content) is refused and makes the item `stale` too.
  Otherwise the item returns to the stage it was held in.
- **After the release.** The next check with the same fingerprint lets the
  item go on; a new comment without findings keeps it, a new one with
  findings holds the item again. The hidden content is not removed; it
  reaches the agent inside the data fence, and every brief of a released
  item carries the fixed line `briefs::RELEASED_NOTE` outside the fence.
- **Cancel** works as for any open item.
- **Off.** `[work] hidden_content_hold = false` turns the check off; a
  held item can still be released or cancelled.

### Limits

- The check finds content that GitHub does not show. It cannot find
  content that GitHub shows but a person misses: a line far down a long
  body, look-alike characters from another script, an instruction written
  in plain sight, visible but tiny text made in ways not listed above.
- A fence info word that passes the character rule is not flagged, so up to
  32 characters such as `please_run_the_install_script` can hide in a
  fence's first word.
- comrak follows cmark-gfm but is not GitHub's renderer; the sanitizer
  list is the published `html-pipeline` one, which GitHub's production
  sanitizer may differ from. Rendering features GitHub adds later are not
  covered until the lists are extended.
- The checks lean toward flagging: every non-autolink Markdown link and
  every image not in an exact attachment shape is a finding, a
  reference-definition-shaped line inside a paragraph is flagged, a ZWJ in
  a minimally-qualified emoji sequence (without its U+FE0F) is flagged,
  math macros outside the allowlist are flagged. A false finding costs the
  operator one release.
- Content in a comment the item does not use (by a non-collaborator) is not
  scanned, because it never reaches an agent. The repository the agent
  reads is not scanned. Text that changes at GitHub between the last live
  read and the agent's start (one API call) is not seen for that step.
- The hold code has 6 hex characters: two holds of one item share a code
  with probability 1 in 16.7 million, in which case the code check passes
  and the fingerprint comparison in the live read decides.

### Verification of the hold

Detector (`cargo test -p nucleus-core work::hidden`, tests in
`core/src/work/hidden/tests.rs`): each kind with the case that must be
flagged and the case next to it that must not — an emoji with U+FE0F, RGI
ZWJ sequences and joiners in Persian and Devanagari (not flagged) next to
a ZWJ between non-sequence emoji, in Latin text and after a right-joining
letter (flagged); `<!-- x -->` in a fence, an indented block, a fence in a
quote and in a list item (not flagged) next to a fence of a list item that
ended (flagged); a zero-width space in a fence (flagged); `&#8203;`
(flagged); link reference definitions (flagged); the sanitizer list
(`<span>`, `<div>`, tables not flagged; `<small>`, `style`, `hidden`,
`title`, nested `<sub>` flagged); `<details open>` (content still checked)
next to bare `<details>`; link destinations, autolinks, attachment images
and other images; fence info tails and non-language words, also in a
quote; rendered fences indented, quoted, in list items and in a quote in a
list; dropped table cells at the top level, in a quote and in a list item;
`\bbox[opacity:0]{…}` and other math; the CR-only case
`~~~text\rvisible\r~~~\r<!-- hidden -->`; a 5 000-character comment kept
whole; comrak's shifted positions after a reference definition. Pipeline
(`work::pipeline`): held before any task with the complete finding and
the raw source stored and one DM message with the hold code; release with
no code or a wrong code refused, with the right code continuing to eval
with the brief line; release after an edit or a new hidden comment refused
and stale; a new hidden comment holds an item in refinement; voice,
forwarded and code-less releases refused; a delayed WhatsApp release and a
dashboard release naming hold A after hold B refused, hold B released
(`a_new_comment_with_hidden_content_holds_an_item_in_refinement`); the
stage change re-checks the hold (`a_release_racing_a_new_hold_changes_nothing`);
hold off. CLI: only the operator releases, and `--hold` is required.
TypeScript: `#n release <code>` is routed as `text` only from the
operator's typed DM. Dashboard API: the detail carries the complete
findings with their ranges and the raw sources; a release without the
hold, with another hold, after hold B, and after the event changed is
refused (`a_held_item_shows_its_findings_and_can_be_released_unless_it_changed`).
Web: `markRanges` splits a source at code-point ranges.

Fix round 2: math comments inline, escaped `\%`, a comment on one line of
a display block and in a math fence (`math_comments_are_hidden_text`); the
two accepted attachment shapes and each rejected variation, including
`/user-attachments/ignore-previous-instructions`, a query, a fragment,
`%2e%2e`, dot segments, credentials, ports, letter case and a UUID with
trailing text (`only_exact_attachment_addresses_are_exempt`); an
unterminated `<iframe title="…` in a quote, in a list item and in a quote
in a list item, a forbidden element in a table cell
(`html_is_read_from_the_tree_in_every_container`); selectors after defined
bases, after `a`, `x`, a non-emoji arrow, repeated
(`variation_selectors_need_a_defined_sequence`); `👩&zwj;💻`, numeric
emoji with `&zwj;`, Persian with `&zwnj;` not flagged, `a&zwj;b` flagged
(`entity_joiners_follow_the_same_rules`).

Fix round 3: `<div>ig&#8203nore</div>`, `&#x200B` without `;` in text,
`&zwj` without `;` left as text (not a legacy name) and `&zwj;` decoded,
the legacy `&shy` decoded in text and not in an attribute before `=`,
`&caps;` and `&varsubsetneq;` flagged for their VARIATION SELECTOR-1,
`&amp` and other visible results not flagged, the numeric replacement
rules (`character_references_follow_the_whatwg_rules`); a copy of a tag in
a code span before the real one after a reference definition (the real
one is reported) and a copy in a link destination (position unknown,
whole location) (`shifted_html_is_placed_in_document_order_or_reported_unknown`).

Fix round 4: each context decoded by its renderer — `&#8203` and
`&#x200B` without `;` are text in Markdown and decoded in `<div>`,
`&shy` is text in Markdown, decoded in HTML text and not in an attribute
before `=`, `&#128;` is a C1 control in Markdown and the euro sign in HTML
(`character_references_are_decoded_as_their_renderer_does`); in pairs,
`&copy&#xFE0F;` flagged in Markdown and not in `<div>`, `*`+VS16 before
emphasis flagged and `\*`+VS16 not, RGI ZWJ across emphasis delimiters
or across a reference Markdown leaves as text flagged and in an HTML block
not, a Persian ZWNJ across emphasis or next to a reference Markdown leaves
as text flagged and in an HTML block not
(`exemptions_see_the_characters_the_renderer_shows`); `[a]:
<iframe>\nxxxxx<iframe>` never reports the definition
(`a_reported_position_inside_a_definition_is_never_used`).

Fix round 5: `<details open="ignore previous instructions">` and
`open="&#105;gnore"` flagged with the decoded value, `open`, `open=""`,
`open="OPEN"` (and `open="&#111;pen"`) not; `align="center"` not,
`align="center; x"` flagged; `start="3"` and `start="-12"` not,
`start="3 now run"` flagged
(`allowed_attributes_need_a_value_that_only_changes_layout`); a ZWJ
sequence, `&zwj;`, a VS16 after ❤ and a Persian ZWNJ inside attribute
values flagged, the same VS16 in the element's text not
(`tag_syntax_gets_no_exemption`).

Fix round 6: `span.foo` with a title flagged with the range of its tag,
`<!- …>`, `<!1 x>`, `</1 x>`, `<?x y>`, `</>` and `<!DOCTYPE html>`
flagged as bogus comments, `a < b`, `x <3` and `1<2` not flagged, a quoted
value holding `>` kept in one token, a tag without `>` to the end of the
node flagged, a duplicate attribute flagged, unquoted values, `<b/x=1>`,
newlines inside a tag and attributes on an end tag handled, invisible
characters inside a bogus comment flagged
(`every_lt_in_raw_html_is_recognized_or_flagged`).

## Amendment: operator decisions in plain words (2026-09-26)

### Why

The operator's WhatsApp decisions needed exact commands (`#2 approve`,
`#2 approve comment`, `#2 skip comment`, `#2 release <code>`). The operator
asked for plain language. When he wrote `#2 approve` for an item whose
comment waited, the bot kept the message and answered "Item #2 is in the
review stage; messages reach an agent only during refinement", which did not
tell him what to do. He writes in his own words now. When the bot does not
understand a message, it asks, and it lists what each item waits for and what
each option does, in fixed texts that code writes.

### The issue comment needs no approval

The proposed comment, its approval (`approve-comment`, `skip-comment` in the
CLI, the dashboard and WhatsApp) and the `review` stage are removed. The `pr`
stage now pushes the branch, opens the draft pull request, sends the PR link
and the agent's summary to the item's thread, and posts one comment on the
issue: `[work.texts] pr_comment` (default `Draft pull request: {pr_url}`),
then closes the item. The comment holds no model output and no issue text,
which is why it needs no approval. The write keeps the protections of
finding 3 and finding 4: the secret guard scans the text; a random 128-bit
operation id is stored before the post and its exact marker line, on a
comment by the authenticated account, is the only proof of an earlier post,
so a retry never posts twice; the comments lookup comes first, then a fresh
live read right before `gh issue comment`; `gh` runs from its pinned path.
An event without a reply channel closes with the draft PR open and
`comment_state = skipped`.

Migration 3 of work.db moves stored items: an item in `review` whose
comment was not posted goes back to `pr` (its `comment_op` is kept, so a
comment an earlier attempt posted is found by its marker), one whose comment
was posted or skipped is closed, a failed or blocked item that stopped in
`review` resumes in `pr`, `proposed` and `approved` become `none`, and
`comment_draft` is dropped. Each stage change is logged in
`item_transitions`. The deployed work.db had no open items; the migration
is tested on a database with rows in every removed state.

The WhatsApp PR message carried the GitHub-escaped copy of the agent's
summary, so the operator saw a backslash before every backtick
(`I changed \`README.m…`). The WhatsApp message now shows the summary as the
agent wrote it (at most 600 characters); nothing escaped for GitHub reaches
WhatsApp, and GitHub gets no summary.

### Which messages are read

Only messages from the operator's exact identity count (finding 5). One
function decides it for every check of the pipeline (`isOperatorId` in
`messaging/whatsapp/src/work.ts`): a chat or sender id is the operator
when its digits are the operator's phone digits (the first
`WHATSAPP_ALLOWED_DM_JIDS` entry), when it is an `@lid` id whose digits are
in `WHATSAPP_OPERATOR_LIDS` (the operator's own LIDs, a separate variable),
or when it is an `@lid` id that the connection's LID mapping resolves to
the operator's phone. `WHATSAPP_ALLOWED_DM_JIDS` is a general DM allowlist:
its other entries are contacts the bot chats with, never the operator. The
same function decides both inbound gates (the DM gate admits the allowlist
as normal chat and the operator by this check, so a LID only the mapping
resolves is admitted as the operator; the work-group sender gate admits
only this check), approvals, the members of a new group, the DM fast paths,
the stored `chat` rows and the decision block, so a DM chat keyed
`<digits>@lid` is the operator's DM everywhere. The asynchronous checks
call the live mapping every time, so a LID the mapping no longer resolves to
the operator stops counting at once. The two synchronous checks (the role
lookup and the target policy) keep a cache of LIDs the mapping resolved to
the operator (`OperatorLidCache`): an entry counts for 10 minutes, every
inbound message verifies each entry against the live mapping again, and an
entry that fails is dropped. `WHATSAPP_OPERATOR_LIDS` entries do not depend
on the mapping. The bot
routes to the pipeline: every operator message in an item's group; and, in
the operator's DM, a message that starts with an item's `#n` marker, a reply
that quotes a message the pipeline sent (a thread message names its item; a
question or note the pipeline sent in the DM, source `work:ask` or
`work:note`, names none), and the next DM message after the pipeline asked
a question in the DM (`work:ask`), within 15 minutes of it being sent.
These fast paths go straight to the interpreter. The bot stores each routed
message with `sender = 'operator'` after its identity check; the pipeline
refuses to interpret a row without it.

Every other operator DM message goes to the DM chat session, as before, so
a cold "approve it" hours later also reaches the chat session first. Two
things make it work there without sending every DM message to a one-shot
interpreter (that would add a model session to every unrelated chat
message):

- **The chat session knows what waits.** Every tick writes a code-built
  block (`pipeline::chat_block`) into whatsapp.db (`work_chat_block`, one
  row, Rust writes, the bot reads): a code-owned header, the interpreter's
  pending lines for the DM (item number, what it waits for, allowed
  decisions; no issue text), and an instruction (`[work.texts]
  chat_block_header`, `chat_block_instruction`). The bot types the block
  after every operator message in the operator's DM chat session while it
  is not empty (passed with the message to the turn engine), only in a DM
  chat that `isOperatorId` accepts, in phone or LID form.
- **The chat session can only trigger.** The bot also stores each operator
  DM message that goes to the chat session in `work_inbound` with
  `item_key = chat` and `sender = operator`; the tick skips these rows. When
  the operator's message asks for one of the listed decisions, the chat
  session runs `nucleus work interpret-latest`. The command takes no text:
  code reads the `chat` rows with `sender = operator` that the calling
  session's current turn covers: the turn engine's running `chat_turns` row
  for the session's own DM chat (the exact chat id, phone or LID form, that
  keys both the session's task scope and the stored rows) and the
  `chat_inbound` rows it marked with that turn, joined by WhatsApp message
  id. A follow-up that arrived before the command ran therefore cannot take
  the place of the message that started the turn. The rows are interpreted
  in order with the same interpreter flow (`pipeline::interpret_latest`).
  Every one is interpreted: running a decision does not stop the loop, so a
  second decision in the same turn is not lost. A confirmation question
  asked by the loop changes what follows: every later message of the turn
  arrived before that question was sent, so none of them can answer it. A
  later decision is not run; the line `[work.texts] also_received`
  ("Also received: '<preview>' — send it again after answering the question
  above.") is added to the question while it is still `pending` in the
  queue (a guarded `UPDATE … WHERE status = 'pending'`), or queued as its
  own note right after it when the drain already claimed the question, and
  the message is marked final. The drain claims a row and reads its body in
  one statement (`claimForSend`: `UPDATE … SET status = 'in_flight' … RETURNING
  body`), so an appended line is either in the body that is sent or refused
  and sent separately, never lost. Anything else is
  left to the session, and the question stays open. From the
  operator's terminal it reads the newest stored row of the last 15
  minutes. A row is interpreted at most once: its `inbound_commands` state
  is final after the first run. On a decision (or an answer to an open
  question) the full flow runs, confirmations and replies included, and the
  command prints `HANDLED` with the line the session ends its turn with
  (`[handled by the issue pipeline]`); the turn engine sends no reply for a
  turn whose whole final text is that line, because the pipeline already
  answered. The output lists the turn's messages by position and a short
  preview: which ones the pipeline handled, and which ones the session must
  answer normally (without repeating the decisions); only when none is left
  does it give the silent line. On a discussion
  or anything unclear, the pipeline sends and keeps nothing, and the
  command prints `NOT A DECISION`; the session answers normally.

Who may run `interpret-latest` (`crate::caller`, `cmd/work.rs::authorize`):
the WhatsApp DM chat session and the operator's terminal, not in a turn
that read an agent message. A worker, a group chat session, any other
Nucleus session, a detached process or an unknown caller may not. A session
that runs it without reason costs one interpreter session and changes
nothing unless the operator's own latest message is a decision.

### The interpreter

Every routed message is read by an interpreter: a one-shot session through
`nucleus_core::claude_session` (Rule 4), profile
`SessionProfile::one_shot_no_tools` (`--tools ""`, `--strict-mcp-config`
without a config, `--disable-slash-commands`, `--permission-mode dontAsk`,
no `--add-dir` and no `--allowed-tools`, whatever a caller sets; the argv
test checks this). It runs in the `nucleus-work` tmux session, started by
`nucleus work tick`.

It receives a `decide::Request` and nothing else:

- the operator's message (for a voice note, its transcription), inside a
  nonce data fence;
- where it came from: the DM (naming item #n or not) or the group of item
  #n;
- the pending decisions, one code-built line per item: the item number, a
  `wait_*` text (for example `plan v2 is waiting for your approval`,
  `held for hidden content (3 findings)`,
  `in refinement, plan v2 is proposed and the agent is writing a reply`), the
  allowed decisions, and whether a discussion message reaches the refinement
  agent;
- the confirmation question waiting for an answer there, if any.

No issue title, body, comment, plan, thread message or agent output is part
of the request type, so none can reach it; the item title is issue text and
is left out too. In an item's group the list holds that item only; in the
DM it holds every item that waits for the operator (a plan, a reply, a
release) and the item the message names.

It answers with one JSON object: `{"kind": "decision" | "discussion" |
"unclear" | "confirm" | "decline", "item": <n or null>, "decision":
"approve_plan" | "release" | "cancel" | null, "question": <text or null>}`.
`decide::parse_reading` validates it: exactly these keys, known values,
fields that fit the kind (`confirm` and `decline` only answer a question).
Any failure, or text around the object, is `unclear` without a question.

### Code decides

- **decision**: the item must be in the list and the decision must be one
  that item allows now. It runs through the existing functions, bound to
  what the list showed: `approve_plan` to the plan version (the approval's
  `UPDATE` re-checks `plan_version` and that no refinement turn runs,
  `store::advance_if_plan`), `release` to the hold fingerprint (the release
  re-checks it before the live read and in its `UPDATE`,
  `store::advance_if_hold`). The live read, the stale checks and the other
  refusals of these functions apply unchanged. The operator never types a
  hold code. A decision the item does not allow is refused with the list.
- **discussion**: the message goes to the thread of the item it came from
  or names, of the item the interpreter named from the list, or of the only
  waiting item; during refinement (or while held during refinement) the
  refinement agent reads it, as before. Outside refinement the message is
  kept and the answer says so, with the item's options. With several items
  waiting and none named, the operator is asked which item.
- **unclear**: the answer is the interpreter's question, or the fixed
  `unclear` text, followed by the list: for each item, what it waits for and
  what each option does (`options_*`, `option_*` texts), ending with
  "Answer in your own words." His answer is interpreted again.

The old command syntax has no separate parser: `#2 approve` is read like any
other message.

### Confirmation

A confirmation question naming the item and the action (`confirm_*` texts,
for example "Release item #4 (held for hidden content, 3 findings)? Answer
yes or no.") comes before:

- every release;
- every cancel (a cancel cannot be undone; this goes beyond the operator's
  request, which named releases and inferred items);
- every decision from a voice note or a forwarded message;
- a decision whose item the interpreter inferred in the DM while several
  items wait.

A message can answer only a question that was sent before the message
arrived. The bot stamps every message's arrival in its `messages.upsert`
handler before any await, for the whole batch at once, and stores it as
the row's `received_at`; a later message of the same batch, or a voice note
whose transcription finishes later, keeps that stamp. It also stores
WhatsApp's own `messageTimestamp` (`work_inbound.wa_ts`, seconds), and the
drain stores the server timestamp of every sent message from the send
result (`outbound_queue.wa_ts`). Both must hold: the question's row is
`sent` with a `sent_at` earlier than the message's arrival stamp, and, when
both WhatsApp timestamps are known, the question's is earlier than the
message's (whole seconds; equal is not earlier). For a message that
arrived earlier, the question is not shown to the interpreter, is not
answered and is not replaced. When either WhatsApp timestamp is missing for
a message received over WhatsApp, the order cannot be confirmed and the
check fails closed: a yes or no is not taken as an answer, the question
stays open and is sent again with the same fixed text, and the next answer
is checked against the re-sent question. A path that never carries WhatsApp
timestamps (none exists today) is judged by the arrival order alone.

A plan approval typed in the item's own group, or in the DM naming the item,
or in the DM while only that item waits, runs at once.

The question is stored in work.db (`confirmations`, migration 4) with the
item, the decision, the bound plan version or hold fingerprint, where it was
asked (`dm` or `group:<n>`) and an expiry 15 minutes later. Storing it and
marking the message applied are one transaction. The operator's next
message from the same place is interpreted with the question shown: `confirm`
runs the stored decision, bound to what was shown when he was asked (a newer
plan or hold is refused); `decline` closes it ("Nothing was done for item
#n."); anything else replaces it and is handled as a new message. The
answering message is recorded on the question before the decision runs, and
the decision's own transaction settles it, so a crash never applies one
answer twice. A question past its expiry is not shown to the interpreter and
is marked `expired`; a late answer changes nothing and is told so.

### Voice notes and forwarded messages

Before, only typed text could be a command. Now a transcribed voice note is
interpreted like typed text, and so is a forwarded message, but a decision
from either always gets the confirmation question; the answer may be typed
or spoken. This is acceptable because the input still comes only from the
operator's own identity (the check of finding 5 is unchanged), and the
confirmation names the item and the action in fixed text, so a
transcription error or a forwarded third-party text cannot change anything
without the operator reading that question and answering yes.

### Replies

Every reply is a code-owned text from `[work.texts]`, sent to the chat the
message came from through `outbound_queue` (target policy, secret filter).

The target policy keeps operator-only messages with the operator
(`target_policy.ts`, `resolveQueuedTarget`). The `dm` shorthand resolves to
the most recently active chat that is the operator's (his phone, a
`WHATSAPP_OPERATOR_LIDS` entry, or a LID the mapping resolves to him,
checked live at send time), else his phone JID; never to another allowed
contact, however recent. A message from the issue pipeline (`work`,
`work:*`), a reminder or a vault check reaches a DM only when that DM is
the operator by the same live check, whatever target it names. Chat-engine
replies to another allowed contact stay in that contact's chat. Reminders to
`whatsapp-dm` name the operator's phone (the first allowlist entry) and pass.

**Task results for the operator's LID chat.** A task started in the
operator's DM chat session carries that chat as its origin, also in LID
form. The Rust side cannot ask the live mapping, so the bot mirrors every
LID it verified as the operator into whatsapp.db (`operator_lid_verified`,
digits and verified-at time; rows are removed when the mapping no longer
resolves them to him). A Rust process accepts an `@lid` origin chat that is
on the allowlist, in `WHATSAPP_OPERATOR_LIDS`, or verified there within the
last 10 minutes. Task creation refuses any other chat. At delivery, a
result for an operator LID whose verification expired goes to the
operator's phone JID; it is not dropped. The drain checks the live mapping
again at send time and sends a result to an `@lid` chat outside the lists
only when the live check accepts it. When the live check rejects the `@lid`
target of an operator-only row or a task result (sender `task:<id>`), the
bot deletes that LID's `operator_lid_verified` row, drops it from its cache,
moves the queue row to the operator's phone JID in place and sends it
there: a result for the operator is never dropped because his LID went
stale. Tasks and replies for other allowed contacts keep their own chat.

**After a restart.** The bot marks a turn it interrupted `interrupted`. The
next tick finds operator DM rows of such turns that were stored for
`interpret-latest` and not interpreted to the end: never read, or left in
`received` because the restart came while they were being interpreted.
While items are open, it sends the
operator one code-owned message (`[work.texts] interrupted_messages`)
naming them with short previews and asking him to send any decision again.
Either way the rows are marked final, so they are never interpreted
later.
The interpreter's question is the one piece of model text: it is cut to one
line of at most 300 characters, the Markdown characters WhatsApp renders
(`*`, `_`, `~`, backticks, leading `>` and `#`) are removed, and it passes
the secret guard (a hit replaces it with the fixed text); the drain's secret
filter applies after that. A decision taken in the DM for an item whose
thread runs in a group is also answered in the DM with the result note.

### Limits

- The interpreter can misread a message. A misread discussion goes to the
  wrong read-only refinement agent or thread; a misread decision is either
  refused (not allowed, not in the list), confirmed first (release, cancel,
  voice, forward, inferred item), or a plan approval in the item's own
  context, which is what the operator's group or `#n` message addresses.
- Each routed message costs one interpreter session (about ten seconds to a
  minute); a message is not read when nothing waits for a decision and no
  question is open. An interpreter that cannot start is retried by the
  inbound mechanism (finding 7) and reported after 5 attempts.
- A decision written cold in the DM depends on the chat session noticing
  it against the block and running `interpret-latest`. When the session
  misses it, the operator gets an ordinary chat answer; the chat session
  itself still cannot decide anything.
- A LID listed in `WHATSAPP_OPERATOR_LIDS` counts as the operator without
  the live mapping, on purpose: the variable exists for the times the
  mapping is unavailable, so the operator must keep it limited to his own
  LIDs.
- A LID that is neither listed in `WHATSAPP_OPERATOR_LIDS` nor resolved
  to the operator's phone by the connection's mapping is not the operator:
  its chat gets no block, its messages are not stored for
  `interpret-latest` and never reach the pipeline (an allowlisted contact's
  LID still chats normally). Listing the operator's LIDs in
  `WHATSAPP_OPERATOR_LIDS` removes the dependence on the mapping.
- The 15-minute window for an unmarked DM answer starts when the question
  was sent; an unrelated DM message sent in that window goes to the
  pipeline (and gets the list) instead of the chat session.

### Verification of the amendment

The chat-session trigger: a cold "approve it" in the DM with one plan
waiting goes to the chat session, the tick does not interpret it, a fake
chat session that sees the published block runs `interpret-latest`, and the
plan is approved (`a_cold_approve_in_the_dm_runs_through_the_chat_sessions_trigger`);
the command interprets the newest stored operator row of its own chat, not
another sender's, a group's, another chat's or an older one's
(`interpret_latest_interprets_the_stored_operator_text_only`); a row is
interpreted once and a discussion sends nothing
(`a_dm_row_is_interpreted_at_most_once`); an unrelated DM message with items
waiting starts no interpreter (`an_unrelated_dm_message_starts_no_interpreter`);
the caller rules (`cmd::work::tests::authorization_by_caller`); the block
after the operator's message and the silent handled turn
(`chat_engine.test.ts`); the block table and the `chat` rows
(`work.test.ts`). The operator's DM in LID form: a chat keyed
`<digits>@lid` with those digits allowlisted, and one the mapping resolves,
get the block, their text is kept and the fast paths route; an unknown LID
gets none of it (`work.test.ts`, "the operator's DM in LID form …"); a
cold "approve it" stored under a LID-keyed chat runs end to end through
that chat's session, a phone-keyed session takes nothing from it, and an
unmarked unknown-LID row is never interpreted
(`a_cold_approve_in_a_lid_keyed_dm_runs_end_to_end`). Review fixes: a
second allowlisted LID is admitted as normal chat and never the operator,
an operator LID from `WHATSAPP_OPERATOR_LIDS` is the operator without a
mapping, a LID only the mapping resolves is admitted by the DM gate as the
operator, and the work-group gate (the same check) admits an operator LID
with the mapping unavailable (`work.test.ts`); operator LIDs are sendable
(`target_policy.test.ts`); the chat session is refused `cancel`,
`approve-plan` and `release` (`authorization_by_caller`); a cancel from the
DM goes through `interpret-latest` and a confirmation
(`a_cancel_from_the_dm_goes_through_interpret_latest_and_a_confirmation`);
"approve it" followed at once by "any update?" in one turn approves the plan
and leaves the follow-up to the session, in either order, each row once
(`a_quick_follow_up_does_not_take_the_place_of_the_decision`). Round 2:
"approve the plan" and "cancel the old item" in one turn approve the plan
and ask the cancel's question, and a later message that is not an answer
stays with the session (`two_decisions_in_one_turn_are_both_taken`,
`the_turn_output_says_what_the_session_still_answers`); with another allowed
contact active more recently a `dm` message goes to the operator, an
operator-only message addressed to another contact's chat is refused, a
reminder to the operator's phone passes, and a LID the live check rejects
falls back to the phone (`target_policy.test.ts`, "operator-only messages
reach only the operator…"); an interrupted turn's uninterpreted row is
reported once and never interpreted
(`a_message_of_an_interrupted_turn_is_reported_once_and_never_interpreted`);
a remapped LID stops being the operator in the live check at once and in
the cache at the next refresh (`work.test.ts`, "a remapped LID stops being
the operator"). Review round 3: a "yes" in the same turn as "cancel item #1"
confirms nothing and a "yes" after the question was delivered cancels
(`a_yes_in_the_same_turn_cannot_confirm_a_question_not_yet_sent`); a later
decision gets the "send it again" line and is not run
(`two_decisions_in_one_turn_are_both_taken`); a task from a
`WHATSAPP_OPERATOR_LIDS` chat is created and delivered there, one from a
mapped LID is created while verified and delivered to the phone JID after
expiry, and one from another contact stays there
(`operator_lid_chats_get_their_task_results`); the verification mirror and
the drain's live check for task LID chats (`work.test.ts`,
`target_policy.test.ts`); a row left in `received` is reported too
(`a_message_of_an_interrupted_turn_is_reported_once_and_never_interpreted`).
Review round 4: a "yes" of the same batch recorded after the question was
sent, a voice "yes" transcribed after it, and a "yes" in the question's own
WhatsApp second do not confirm, and a later "yes" does
(`a_yes_that_arrived_before_the_question_was_sent_does_not_confirm`); the
batch arrival stamp and both WhatsApp timestamps are stored
(`work.test.ts`, "a batch's messages keep the arrival…"); an append
racing a claim is in the claimed body or refused, never neither
(`work.test.ts`, "an appended line is in the claimed body or refused…";
`whatsapp_queue::tests::an_append_succeeds_only_on_a_pending_row`); a task
result or operator-only row for a LID the live check rejects goes to the
phone and drops the LID, another contact's LID keeps its result
(`target_policy.test.ts`).

Rust (`cargo test -p nucleus-core work`, fake interpreter): "looks good,
go ahead" in item #1's group approves plan v1 at once
(`looks_good_in_the_items_group_approves_the_plan_it_shows`); a DM approval
runs at once with one item waiting, after a confirmation with two, and at
once when it names the item
(`a_dm_approval_runs_at_once_with_one_item_waiting_and_after_a_confirmation_with_several`);
a release always asks first (`a_release_always_asks_first`); "no" does
nothing (`no_to_a_confirmation_does_nothing`); an expired question does
nothing and says so (`an_expired_confirmation_does_nothing`); an unclear
message gets the cleaned question and the list, a question the guard stops
gets the fixed text, a non-JSON answer reads as unclear
(`an_unclear_message_gets_the_question_and_what_each_option_does`); a
decision the stage does not allow is refused with the list
(`a_decision_the_stage_does_not_allow_is_refused_with_the_options`); a voice
note or forward is always confirmed and the answer may be spoken
(`a_decision_from_a_voice_note_or_a_forward_is_always_confirmed_first`); a row
from another sender is never interpreted
(`a_message_from_another_sender_is_never_interpreted`); an approval binds to
the listed version, directly and through a confirmation, and the `UPDATE`
re-checks it (`an_approval_binds_to_the_plan_version_in_the_list`); the
interpreter's prompt has no issue title, body, comment, plan or agent text
even when they carry instructions
(`the_interpreter_receives_only_the_operators_text_and_lines_code_built`);
the schema, the question cleaning and the fence (`decide::tests`); the
no-tools argv (`the_no_tools_profile_grants_no_tool_directory_or_skill`).
The PR link comment is posted after the draft PR without approval, through
the guard and the fresh read after the lookup
(`simple_issue_goes_from_work_to_a_draft_pr_and_the_pr_link_on_the_issue`,
`the_secret_guard_blocks_a_push_and_a_comment`,
`the_comment_is_written_only_after_a_fresh_read_that_follows_the_lookup`,
`removing_the_label_stops_an_item_before_the_pr_link_is_posted`,
`forged_markers_and_foreign_pull_requests_are_ignored`); the backtick bug
(`the_implementation_summary_reaches_whatsapp_without_markdown_escapes`);
migration 3 on a database with rows in every removed state
(`the_review_stage_is_migrated_away`). TypeScript (`src/work.test.ts`):
routing by marker, by quoted pipeline message and by the answer window,
the window's start, end and use by the first DM message, another sender
never routed, `sender` stored. Not verified here: the interpreter with a
real model on real messages, and the answer window on the live account.

## Amendment: no WhatsApp groups, short notices, whole plans (2026-09-26)

### Why

Three problems came up in use.

- A real plan v5 of 12 191 characters reached the implementation agent cut
  off in step 3: the brief clipped the approved plan to 6 000 characters,
  so its later steps, its tests and its out-of-scope list were missing.
  Refinement turns also saw earlier plans cut off (each thread message was
  clipped to 4 500 characters). The agent built from a plan the operator
  did not approve.
- Per-item WhatsApp groups cost more than they gave. Creating groups from a
  personal account needs a daily limit and anti-spam care, an unknown
  creation needs nonce recovery, quarantine and a manual `group-resolve`,
  and every group needs a membership tripwire and a sender gate. The
  operator reads and decides in the DM anyway.
- Long plans, agent replies and finding lists do not read well on a phone.
  The dashboard shows them whole; WhatsApp needs to say what happened and
  where to read it.

### Plans are never cut

- `briefs::PLAN_LIMIT` = 20 000 characters. A refinement reply whose plan
  is longer does not become a plan version. The thread shows a placeholder
  in its place and a Nucleus note (`[work.texts] plan_too_long`: "The
  proposed plan has N characters; the limit is 20000. The agent was asked
  to shorten it."). The note is a new message of the next turn, which
  starts at once and carries a code-owned paragraph, outside the data
  fence, that names the refused length. After two automatic turns in a row
  (`MAX_PLAN_RETRIES`) the third refusal stops (`plan_too_long_stopped`)
  and the agent waits for the operator, who gets the agent-reply notice.
  `items.plan_refused_chars` and `items.plan_refusals` hold that state; an
  accepted plan or a reply without a plan clears it.
- The refinement brief carries the latest plan whole, and the
  implementation brief carries the approved plan whole. A brief that
  carries a plan caps the issue text lower (refinement: body 3 000,
  comments 2 000; implementation: body 4 000, comments 3 000; eval text
  2 000 everywhere), and the refinement history gives way, oldest message
  first, until the brief fits `tasks::MAX_BRIEF_CHARS` (32 000). New
  operator messages are never cut. A brief that still does not fit is
  `briefs::BriefTooLong`: the item goes to `blocked` with that reason, and
  nothing is cut.
- In the history, an agent reply that carried plan vN shows "(plan vN,
  shown in full above)" for the latest plan and "(plan vN, see the
  dashboard)" for an older one (`item_messages.plan_version` records which
  version a reply carried; the text is replaced between its `── plan vN ──`
  lines).
- A finished refinement turn is recorded in one transaction
  (`store::record_turn`): the reply, the note, the item's columns and the
  plan version, only while that turn is still the item's current task.

### Plan versions are kept

`plan_versions (item_id, version, text, proposed_at)` keeps every accepted
plan version whole. The dashboard detail has `plans: WorkPlanVersion[]`
(`version`, `text`, `at`), oldest first. Migration 7 backfills, where
known: the plan each stored agent reply carried (between its shown
markers), then the item's `approved_plan` and `plan_draft`, which are the
authoritative text of their versions.

### No WhatsApp groups

Removed, with no parallel path: group requests and surfaces, reconciliation
and `group_closed_at`, `[work.whatsapp]` (`refinement_groups`,
`max_groups_per_day`, `group_wait_minutes`), `nucleus work group-resolve`,
the group origin of operator decisions (the "in a group, the list holds only
that group's item" rule), and in the bot the group executor, nonce recovery,
quarantine, the membership baseline, the `work` role in the target
allowlist and the group sender gate. Every item's WhatsApp surface is the
operator's DM. A stored `work_inbound` row from a group chat is never
interpreted.

Migration 5 moves items with surface `group` or `pending` to `dm`, closes
open confirmation questions in a `group:<n>` scope (`replaced`) and drops the
three group columns. The bot, once per process on its first open connection
(`cleanupLegacyGroups` in `messaging/whatsapp/src/work.ts`), leaves every
group `intake_groups` still records as open (an unknown creation is looked
for once by its nonce: one match is left, none means there is no group,
several count as a failure), marks it closed, and drops `intake_groups` and
`intake_group_requests` when no group is left open. A failed leave keeps its
row for the next start and is reported to the operator in the DM after 3
failures (`LEGACY_LEAVE_ALERT_AFTER`), once per item.

### WhatsApp gets short notices

Every WhatsApp message the pipeline sends about an item is a one-line,
code-owned `[work.texts] notice_*` text with the item's dashboard link,
`${NUCLEUS_PUBLIC_URL}/work?item=<n>` (no link when the variable is
unset): a new item needs a plan; the agent replied (the first ~200
characters of the reply, cut at a word boundary with "…", WhatsApp
formatting characters and quote and heading marks removed, passed through
the secret guard, otherwise sent without the preview); plan vN is ready to
approve; held for hidden content (the count only); released;
implementation started; draft PR opened (with its link); blocked or failed
(a one-line reason of at most 160 characters, replaced by a fixed text when
the secret guard flags it); stopped (stale or closed at the source);
cancelled. The confirmation questions keep their words and get the link.
No plan, agent reply, finding list or implementation summary goes to
WhatsApp; the thread notes keep them for the dashboard, and the operator's
own dashboard messages are not echoed. Each thread message stores its notice
(`item_messages.notice`, migration 6); the flush sends only notices, under
source `work:<n>`, so a reply to one still reaches the item. A thread
message still waiting to be copied to WhatsApp in full when migration 6 runs
is not sent.

The operator's DM replies work as before: the interpreter reads decisions,
discussion goes to the item's thread (`via = whatsapp`) and to the
refinement agent, confirmations are asked and answered in the DM.

### Review fixes (2026-09-26)

- **The guard reads raw text first.** Normalization for WhatsApp
  (`publish::plain_line`) replaces `@` with `＠` and removes formatting, so
  an address in normalized text passes the email patterns of
  `tools/check-secrets.sh` and of the bot's outbound filter. The guard now
  reads each raw source before it is normalized: the whole agent reply (a hit
  sends the notice without a preview), the raw reason of a failed or stopped
  item, the raw issue title of a notice that uses it, and the raw issue title
  and source name next to the pull request text. Every finished notice is
  scanned again; a hit on the title or the notice sends the fixed
  `notice_withheld` text ("Item #n has an update on the dashboard").
- **Queued full messages are withdrawn.** Migration 6 marks unsent thread
  messages in work.db, but an earlier version may already have put full
  thread messages in `outbound_queue`. When the bot starts, before the drain
  runs, `withdrawLegacyThreadMessages` marks `failed` (nothing is deleted)
  every pending or in-flight work row that is a full thread message (source
  `work:<n>`, body marked `[#n] `) or is addressed to a group, and queues one
  `workWithdrawn` notice per item ("Item #n has messages on the dashboard"
  and the link), keyed so a second start queues none.
- **No stored operator message is dropped silently.** An operator message the
  bot stored from an item's group and the pipeline never interpreted is
  reported in one DM message with short previews (`group_messages_dropped`,
  "send it again") and then marked final, like the messages of an interrupted
  chat turn.
- **The request table stays while a creation is unknown.** The bot drops
  `intake_group_requests` only when no old creation can still mean a group
  that `intake_groups` does not record (a `create` claimed and sent to
  WhatsApp, with no closed or refused group row). Such a creation is looked
  for by its nonce at each start: none found settles it, one found is left.
  While it stays unknown the table is kept, the start is logged, and after 3
  starts the operator is asked in the DM to check by hand
  (`workGroupRequestUnresolved`). `intake_groups` is dropped on its own
  when no group in it is open.

### Messages typed on the dashboard

`POST /work/api/reply` stores the text in the item's thread with
`via = dashboard` in every stage and returns `WorkReplyResult { item,
reaches_agent, note }`. During refinement (or while held from refinement)
the agent reads it at its next turn; otherwise `note` says that the item is
not in refinement and the message is saved. The text is discussion only and
is never interpreted; decisions stay on their explicit routes
(`approve-plan` bound to a version, `release` bound to a hold, `cancel`).
"Amendment: the decision board" replaces this: while the item waits for the
operator the text is interpreted.

### Verification of the amendment

Rust: `a_12000_character_plan_reaches_both_briefs_byte_identical` and
`a_12000_character_plan_reaches_both_briefs_whole_and_every_version_is_kept`;
`a_plan_over_the_limit_is_refused_with_a_note_and_the_next_turn_is_told`;
`the_largest_allowed_plan_with_a_maximal_issue_fits_the_ledger`;
`a_brief_that_cannot_fit_is_refused_never_cut` and
`a_brief_over_the_ledger_limit_blocks_the_item_with_the_reason`;
`earlier_plans_in_the_history_are_references`;
`the_migrations_keep_every_known_plan_and_move_a_group_item_to_the_dm` and
`the_group_surfaces_are_migrated_to_the_dm`;
`a_notice_carries_the_item_link_only_when_the_public_url_is_set`;
`no_notice_exceeds_600_characters`;
`the_reply_preview_is_plain_and_cut_at_a_word_boundary`;
`refinement_notices_carry_a_preview_or_the_plan_version_and_the_link`;
`a_failure_notice_has_a_one_line_reason_and_a_guarded_one_is_withheld`;
dashboard: `a_dashboard_message_is_saved_in_every_stage_and_reaches_the_agent_in_refinement`,
`the_detail_lists_every_plan_version_oldest_first`. TypeScript
(`src/work.test.ts`): the one-time leave, its retry and its report after 3
failures, an ambiguous nonce match, the tables dropped only when no group is
open. Not verified here: the one-time leave on the live account.

## Amendment: the decision board (2026-09-26)

### Why

The operator typed "approve the plan" into the item page's reply box. Text
typed on the dashboard was discussion only, so it went to the refinement
agent, which cannot approve, and nothing happened. The operator must never
need to know a command: when an item waits on him, the page shows the valid
options, and words typed on the page work as they do on WhatsApp.

### The board

While the item waits on the operator, the bottom of the conversation shows a
board in place of the composer (`DecisionBoard.tsx`, drawn in the canvas box
of ADR-012, `CanvasFrame.tsx`, which the chat's canvas blocks also use). Its
options come from code (`boardFor` in `web/src/lib/work.ts`), from the
item's stage and data, never from model text:

| Item | Options |
|---|---|
| refinement, plan vN proposed, no turn running | Approve plan vN (bound to vN), Continue discussing, Cancel item |
| refinement, no plan, the agent waits for a reply | no board: the composer |
| held | Release (bound to the `hold_hash` the page shows; the findings are listed above the options), Continue discussing, Cancel item |
| failed, blocked | Retry, Cancel item |
| queued, eval, a refinement turn running, implementation, pr | the status line and Write a message |
| closed, cancelled, stale | neither the board nor the composer |

Approve, release and retry call their explicit routes at once: the option is
the explicit choice and names the version or the hold. Cancel item asks a
second step on the board. "Continue discussing" and "Write a message" show
the composer, focused, with "Back to options"; the board comes back when the
stage, the plan version, the running turn, the hold or the open question
changes. A refusal shows the server's text on the board. The arrow keys move
the highlight (wrapping), Home and End jump, Enter selects; a tap selects.

### Dashboard text through the interpreter

`POST /work/api/reply` calls `pipeline::dashboard_message`. The dashboard
is the operator's own authenticated surface (tailnet only), so its text is
his. While the item's pending entry is `waiting` (a plan to approve, a
reply, a release) or a confirmation question of the page is open, the text
goes through `handle_message` with `Origin::Dashboard { item }`: the pending
list holds this item only, and the interpreter, the binding and the
confirmation rules are the WhatsApp ones. A typed plan approval on the page
names its item and runs at once; a release or a cancel is asked first. The
question is stored with scope `dashboard:<n>` (a WhatsApp "yes" does not
answer it), added to the thread as a Nucleus note, returned in the detail
(`question: WorkQuestion`) and shown on the board as a Yes / No step,
which calls `POST /work/api/answer {id, question, yes}`
(`pipeline::dashboard_answer`, the same `settle` path as a typed yes or no).
Answers to dashboard messages are thread notes, never WhatsApp messages;
decisions are recorded `via dashboard`. A dashboard message has no WhatsApp
timestamps: it answers a question stored (`created_at`) before its arrival
stamp, which is taken before anything else. The `inbound_commands` row of a
dashboard message has `wa_row_id = 0`.

No interpreter session starts when nothing waits on the operator: the text
is discussion (`pipeline::reply`). A canvas answer is always discussion, so
a click on an agent's question can never become a decision, whatever its
option keys or labels contain (they are model text). The dashboard marks
it (`kind: "canvas"` on the reply request), and the server routes on that
mark without reading the text. As a second guard, any text that carries a
canvas-response tag is discussion too (`decide::is_canvas_discussion`):
`decide::canvas_text` parses each response as the tag, its attributes and
one JSON object payload (the payload ends where the JSON ends, so a key
containing `</canvas-response>` stays inside it), with only whitespace
between responses; text that does not parse that way is `Malformed`, and
is also discussion. The reply result reports what
happened: `outcome` is `discussion`, `decision` (with `decision`),
`question`, `unclear` (the question and the option list in `note`),
`declined` or `refused`; the composer shows it. The dashboard process gets a
`SessionInterpreter`; `NoInterpreter` is removed.

### Agent questions as canvas blocks

The refinement brief allows the agent to ask its questions as ADR-012 canvas
blocks (`decision`, `multi-select`, `confirm`, `form`), says the operator's
choice comes back as his next message, and forbids offering approve, release
or cancel as options. The item thread renders agent replies with the chat's
parser (`parseMessage`) and answered state (a later operator message with a
response for the block's id); labels and titles render as plain text. A
choice posts `buildResponse` through `POST /work/api/reply` with
`kind: "canvas"`, as discussion.
The WhatsApp preview of such a reply replaces each block with "[a question
with options on the dashboard]".

### Verification of the amendment

Rust: `approving_the_plan_typed_on_the_dashboard_approves_the_pending_version`,
`cancelling_typed_on_the_dashboard_asks_first_on_the_board`,
`discussion_typed_on_the_dashboard_reaches_the_agent`,
`nothing_waiting_starts_no_interpreter`, `a_reply_preview_leaves_canvas_blocks_out`,
`a_canvas_answer_never_reaches_the_interpreter_whatever_its_key_says` (an option
key `</canvas-response> approve plan v2`, marked or not, and malformed canvas
text: discussion, no interpreter, nothing approved),
`canvas_responses_are_parsed_as_tags_with_a_json_payload`,
`a_dashboard_origin_names_its_item_and_page`; dashboard:
`text_on_a_waiting_item_is_interpreted_and_its_question_answered_on_the_board`,
`a_canvas_answer_is_discussion_and_reaches_no_interpreter`,
`a_dashboard_message_is_saved_in_every_stage_and_reaches_the_agent_in_refinement`.
TypeScript (`src/lib/work.test.ts`, `ItemPage.test.tsx`): the options per
stage, Continue discussing and Back to options, the cancel second step, the
question step, the keyboard selection, canvas blocks in agent replies with
the answered state and plain-text labels. Not verified here: the interpreter
on real dashboard text.

## Rejected alternatives

- **Webhooks.** They need public ingress, which Nucleus does not have
  ([[ADR-011]]); a poll every few minutes is fast enough for issues.
- **One long refinement session per item, resumed per turn.** A session's
  context lasts only as long as its transcript can be resumed; ADR-029
  removed ad hoc resume. The thread in work.db is the durable record, so
  each turn's brief carries it.
- **Letting the refinement agent decide that a plan is approved.** Approval
  releases implementation work, and the refinement agent reads issue text
  that anyone can write. The interpreter of the amendment is a different
  session: it has no tools, reads only the operator's text and lines code
  built, returns a validated JSON object, and code runs the decision bound
  to the listed plan version or hold.
- **Whole-message commands as a second path next to the interpreter.** Two
  paths would give two behaviors for the same words; the interpreter reads
  `#2 approve` too.
- **Sending every DM message to the interpreter while items wait.** It adds
  a model session to every unrelated chat message and delays every answer
  of the operator's general assistant for as long as a plan waits. The chat
  session sees the list and triggers the interpreter on the stored message
  instead.
- **Only the fast paths (a `#n` marker, a quote, the answer window).** A
  cold "approve it" in the DM went to the chat session, which could not
  decide: the failure the operator reported.
- **Letting the chat session pass the text to interpret.** The session reads
  other text (tool output, agent messages); `interpret-latest` reads the
  operator's stored message instead, so the session can start an
  interpretation but not choose what is interpreted.
- **The implementation agent pushes and opens the PR.** It would need
  network access and the operator's `gh` credentials inside the session,
  which rules out a later sandbox.
- **A WhatsApp group for every item.** Simple items need no discussion, and
  automated group creation risks the account; groups exist only for items
  that reach refinement, within a daily limit.
- **Stripping hidden content before the brief.** The operator would release
  one text and the agent would read another; a stripped comment can also
  change the meaning of what is left. The hold shows the content and the
  release passes it on unchanged, as data, with a note.
- **Reading Markdown structure with hand-written rules.** The first version
  did, and the review found constructs inside block quotes and list items,
  fences indented by up to three spaces and CR-only line endings that the
  rules read differently from GitHub. The syntax tree of a cmark-gfm port
  answers those from the grammar; the source-text checks remain only where
  the tree has no node or may report a wrong position.
- **Rendering the Markdown and diffing the visible text.** It needs
  GitHub's sanitizer and renderer exactly, and a difference in the visible
  text cannot always be mapped back to a source range to show the
  operator. The tree plus a list of hiding places gives a source range for
  every finding.
- **Running the pipeline inside the WhatsApp bot or the dashboard.** Git,
  `gh` and test runs would block processes that must stay responsive, and
  work.db would get a second writer family.

## Consequences

- A labeled issue on a configured repo starts an eval within a few minutes
  (the poll interval) and needs no operator action when it is simple: it
  closes when the draft PR is open and its link is posted.
- The operator's WhatsApp gets up to `max_groups_per_day` new groups a day;
  each is left when its item closes. Items past the limit use the DM with
  `[#n]` markers.
- Each refinement turn re-reads the repository it needs, since it starts a
  new session; long threads are cut to their most recent part in the brief.
- Nucleus's own test run holds that item's lock for up to
  `test_timeout_minutes`; other items continue.
- The `code` profile is a denylist; until an OS sandbox exists, an
  implementation agent acts with the operator's user rights on the machine.
