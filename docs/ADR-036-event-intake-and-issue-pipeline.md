# ADR-036 — Event intake and the issue pipeline

**Status:** Accepted (2026-09-24) — Implemented (2026-09-24), amended after an adversarial
review (2026-09-24, see "Amendment: review findings"), amended with the hidden-content hold
(2026-09-25, see "Amendment: the hidden-content hold", revised after its review the same day); live verification pending (real `gh`
against the configured repos, real WhatsApp group creation).

**Builds on / changes:**
- [[ADR-033]] — every agent step is a task in the task ledger (`origin =
  pipeline`); the ledger gains a per-task working directory and a tool
  profile. The WhatsApp target policy gains the intake groups; the DM chat
  session gains read-only intake commands.
- [[ADR-020]] — new DB `memory/intake.db` (written only through
  `nucleus_core::intake`); new queue table `intake_group_requests` in
  whatsapp.db; new bot-owned tables `intake_groups` and `intake_inbound`
  that Rust reads.
- [[ADR-029]] — the GitHub poll cursor is a watermark per repo
  (`intake:github:<owner/name>`).
- [[ADR-021]] — operator replies reach an item's agent attributed to the
  operator and the surface they came from.
- [[ADR-011]] — the Intake dashboard page follows the tailnet-only threat
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
   parent links: intake, eval, refinement (for complex items and feature
   requests), implementation, draft pull request. Nucleus code, not an
   agent, pushes and opens the pull request. Nucleus never merges. The issue
   gets a comment only after the operator approves the text.
5. OS sandboxing is deferred. The design keeps it possible: agents do not
   run network steps; Nucleus code does.
6. Every WhatsApp send uses the existing target policy and secret filter;
   the task ledger handles concurrency, cancel and status; the operator can
   ask about items in plain language in the DM.
7. A dashboard page lists the items and shows each item's eval, thread (with
   a reply box), plan, implementation status and pull request.

## Decision

### 1. The common event record and the adapter interface

`core/src/intake/event.rs` defines `NewEvent`: `source`, `external_id`,
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
instead of waiting for a comment decision.

`nucleus events emit --source S --id X --title T [--body - | --body B]
[--project owner/name] [--label L]… [--url U] [--state open|closed]
[--accept]` records an event. `--accept` (the operator's terminal only)
marks it as passing the gate; with a configured repo as `--project` it
becomes an item. A detached process (a launchd script) may emit without
`--accept`. `nucleus events list` shows recent events.

### 2. The GitHub adapter

`core/src/intake/github.rs`, through `gh` (`[intake.github] gh_bin`; launchd
has no shell PATH, Rule 5). One adapter instance per configured repo.

- **Poll.** `gh api -X GET repos/{repo}/issues` with `sort=updated`,
  `direction=asc`, 100 per page, up to `max_pages`. The first poll reads
  open issues only; later polls read every state with `since=<cursor>`, so
  a closed issue closes its item. The cursor is the newest `updated_at`
  seen, stored as a watermark after every event of the batch is recorded.
  `since` is inclusive; the store absorbs the repeated issue. Pull requests,
  which the issues API also lists, are skipped. The tick polls a repo only
  when `poll_interval_secs` (default 300) have passed since its last poll.
- **Gate.** `accepted` is true when the issue carries `[intake] label`
  (default `nucleus`, compared case-insensitively). A poll never creates an
  item. The tick's gate check reads every accepted, open event on a
  configured repo that has no open item and changed since its last check
  live at GitHub (Amendment, finding 1) and creates the item only when the
  gate holds there.
- **Collaborators.** `GET /repos/{repo}/collaborators/{login}`: success means
  collaborator, a 404 means not. Any other failure counts as not trusted for
  that read and is not cached. Answers are cached in intake.db for
  `collaborator_cache_secs` (default 3600). The cache serves display and
  polling only: every brief and every action boundary checks live
  (Amendment, finding 1c). Logins outside GitHub's character set are never
  looked up. `discussion()` returns only comments by collaborators and the
  number left out; Nucleus's own comments (they carry the marker below) are
  skipped. A comment list longer than `max_pages` is an error, not a
  shortened list.
- **Reply.** `gh issue comment` with the body plus an invisible marker
  `<!-- nucleus-intake:item-<n>:comment -->`. Before posting, the comments
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
| `pr` | Nucleus pushes the branch and opens a draft PR |
| `review` | the PR is open; the proposed issue comment waits for the operator |
| `closed`, `cancelled` | terminal; the item's group is left and its clone removed |
| `stale` | terminal; the source changed after the gate was satisfied (finding 1); re-adding the label starts a new item |
| `failed` | a step failed; `retry` resumes it |
| `blocked` | the secret guard stopped a publishing step (finding 4); `retry` scans again, `cancel` stops it |
| `held` | the issue text or a comment the item uses has content GitHub's page does not show; no agent runs until the operator releases or cancels the item (Amendment: the hidden-content hold) |

`core/src/intake/stage.rs::transition` is the only place that decides a
stage change; it refuses events that do not apply (no skipping from `eval`
to `pr`, no approval outside refinement, terminal stages never change). A
failed eval is retried from `queued`; any other failed stage is retried in
place. `store::advance` applies a transition in one `BEGIN IMMEDIATE`
transaction whose `WHERE` re-checks the stage it moves from and logs the
transition in `item_transitions`.

Conditions from the source stop an item at any time: the event closed (the
issue was closed) closes it; the label removed cancels it at every stage,
including `pr` and `review`; a changed title or body makes it `stale`. A
running stage task is cancelled in every case. An event has at most one
open item (not closed, cancelled or stale) and at most one item per gate
event; a new gate event (the label added again, or the issue reopened by a
collaborator) starts a new item.

Every agent step is a task in the ADR-033 ledger: kind `intake-eval`,
`intake-refine` or `intake-implement`, origin `pipeline` (the ledger only,
no delivery), requester `pipeline`, `origin_ref = intake:<n>`, parent = the
item's previous task, links `intake=#<n>`, `event=…`, `issue=owner/name#12`.
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

The brief (`core/src/intake/briefs.rs::eval_brief`) asks for one of
`simple`, `complex`, `feature`, with written reasons and the criteria
`change_size` (small | medium | large), `schema_impact`, `security_impact`,
`public_api_impact` and `confidence` (0–1), as JSON between
`===EVAL===` and `===END EVAL===`. `stage::parse_eval` validates it. Code
then applies the pipeline's own rule: an item goes straight to
implementation only when the agent says `simple` AND no impact holds, the
size is not `large`, and the confidence is at least `[intake]
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

- A new WhatsApp group, when `[intake.whatsapp] refinement_groups` is on
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
the outbound queue (`dedup_key = intake:<n>:m<id>`), so the drain's target
policy and secret filter apply. A message to a group has every identifier
redacted by the filter, as for every group.

**Turns.** Each refinement turn is a separate `intake-refine` task with the
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

**Approval.** Code decides approval from the operator's own typed message,
never the model (who counts as the operator: Amendment, finding 5). A message whose whole text is `approve`, `approve plan`,
`approve vN` (with `#n` in the DM; optional in the item's group) approves
the latest plan; `approve comment`, `skip comment` and `cancel` are the
other commands; everything else is a message. The approval is refused, with
a note in the thread, when there is no plan, when a refinement turn is
running (its reply may replace the plan the operator read), or when the
named version is not the latest. The dashboard's approve button sends the
version shown. The approved plan becomes the implementation brief.

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
5. sends the PR link to the item's thread, and proposes an issue comment
   (`[intake.texts] issue_comment`).

The comment is posted only after the operator approves it, in the thread
(`#n approve comment`) or on the dashboard, where the text can be edited
first, and only after a live read of the issue and a secret scan of the
comment; `skip comment` closes the item without one. Nucleus never merges and
never marks a PR ready.

### 9. The driver

`nucleus intake tick` runs every minute from launchd
(`tools/launchd/intake-tick.plist.example`, `StartInterval = 60`), and once
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
process exits) under `memory/intake-locks/`: `poll`, `inbound`, and one per
item. A tick skips what another holds, so a long test run on one item never
delays the others and no item is advanced by two processes.

### 10. Dashboard

`/intake` lists items (open / all) with stage, repo, eval class, what waits
on the operator and the PR link. The detail shows the source event, the eval
(class, criteria, reasons, escalations), the plan with an approve button,
the thread with a reply box (refinement only; also sent to WhatsApp), the
implementation summary, Nucleus's test result, the PR, the proposed comment
(editable, approve or post nothing), the stage tasks from the ledger and the
stage log. Retry and cancel are row actions. Every confirmation uses
`InlineConfirm`. API: `/intake/api/{list,detail,reply,approve-plan,
approve-comment,skip-comment,cancel,retry,release}`; wire types are generated (Rule
12). Writes accept JSON bodies only and refuse requests a browser marks as
cross-site, as the Tasks cancel does (ADR-033 §7).

### 11. CLI and the DM session

`nucleus intake tick [--poll] | list [--all] [--json] | show <n> [--json] [--hidden] |
reply <n> --text T | approve-plan <n> [--version V] | approve-comment <n>
[--text T] | skip-comment <n> | cancel <n> | retry <n> | release <n> --hold <code> |
group-resolve <n> --left|--absent`.

`list` and `show` print JSON with `--json`. JSON is for programs and is not
fenced; only when the caller is the WhatsApp DM session is the output (JSON
or text) printed inside a data fence, because a session reads it
(Amendment, finding 1b). `group-resolve` is the operator's only way to end a
WhatsApp group creation whose outcome is unknown (finding 6).

| Caller (`crate::caller`) | May |
|---|---|
| Operator | every command (`group-resolve` and `release` only from the operator) |
| WhatsApp DM chat session | `list`, `show`, `cancel` (not in a turn that read an agent message) |
| Detached process (launchd, the bot, the dashboard) | `tick` |
| Workers, other sessions, unscoped chats, unknown | nothing |

The DM persona gets an "Issue pipeline items" section and the pre-approved
patterns `Bash(./target/release/nucleus intake list|show|cancel:*)`. The
session answers questions about items in plain language and tells the
operator how to approve; it cannot approve, reply in a thread, or retry.

### 12. Write ownership (ADR-020)

- **intake.db** — every write through `nucleus_core::intake` inside the
  `nucleus` binary (CLI, tick, the dashboard's write routes). Dashboard reads
  use `open_read_only`. The schema has one version: the feature was never
  deployed before its review rounds, so no intake.db and no whatsapp.db
  intake table existed anywhere, and the intermediate migrations were
  collapsed (round 4). The dashboard shows empty lists, refuses writes and
  creates no intake.db when intake is disabled or the database is missing,
  and reads an empty or half-created database (no schema version recorded,
  or no item tables) as no items (round 5).
- **whatsapp.db** — Rust inserts into `outbound_queue` (thread messages) and
  the new queue table `intake_group_requests`; it reads the bot's
  `intake_groups` (group state) and `intake_inbound` (operator messages,
  read past a watermark kept in intake.db, with a processing state per
  message in intake.db's `inbound_commands`). The bot owns the schema of all
  three intake tables (`messaging/whatsapp/src/intake.ts`);
  `whatsapp_queue::open` creates the queue table only so a producer works
  before the bot booted.
- **Mirrors and item clones** live under `[intake] work_dir` (default
  `~/nucleus-work`), which must be outside the Nucleus checkout.

### 13. Configuration

`[intake]` in `nucleus.toml` (see `nucleus.toml.example`): `enabled`,
`work_dir`, `label`, `min_confidence`, `test_timeout_minutes`,
`commit_author_name`, `commit_author_email`, `import_max_files`,
`import_max_file_bytes`, `import_max_total_bytes`, `scan_max_bytes`,
`hidden_content_hold` (default true), `[[intake.repos]]` (`repo`, `test_command`, `default_branch`,
`pr_issue_keyword`), `[intake.github]` (`gh_bin`, `poll_interval_secs`,
`collaborator_cache_secs`, `max_pages`, `remote_url`), `[intake.whatsapp]`
(`refinement_groups`, `max_groups_per_day`, `group_wait_minutes`),
`[intake.texts]` (every operator-facing text). The repos name the
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
  opens only draft PRs with code-owned text, never merges, and posts a
  comment only after the operator approved its text; the pushed diff, the
  PR text and the comment pass the repository's secret guard first. WhatsApp
  messages go through the outbound queue (target policy, secret filter);
  intake groups are sendable only while active, and only through the drain
  (the TypeScript queue writers `ack.ts` and `enqueue-media.ts` still accept
  only configured groups).
- **Who approves.** Only an operator message read by code (typed by the
  operator's own identity, in the operator's DM or in an intake group whose
  membership still matches the create response), the operator's terminal,
  or the dashboard (tailnet only). A chat session, a worker, an agent
  message, a voice-note transcription or a forwarded message cannot
  approve.

## Verification

- Unit (Rust, `cargo test -p nucleus-core intake`): stage transitions
  (allowed and refused), eval parsing and escalation, plan extraction,
  operator commands, group budget, subjects and branch names; event dedup and
  validation, one item per event, guarded stage changes, thread dedup and
  read marks, collaborator cache; GitHub issue conversion and gate, poll
  cursor and state selection, collaborator filtering (404, other failures,
  cache), reply without double posting; git mirror / clone / branch / commit /
  push cycle, test runs with status and timeout, work dir outside the
  checkout; brief fencing and size; the pipeline against a fake `gh`, a bare
  remote and a launcher that starts nothing: a simple issue to a draft PR and
  a posted comment, a feature refined in a group with refused and accepted
  approvals, dashboard replies, the group budget and fallbacks, the source
  stopping items, step retries and failure recovery, unknown items and
  duplicate messages, polling with the interval, item locks. Tasks: working
  directory and profile validation. CLI authorization. Dashboard: missing
  DBs, JSON-only and same-origin writes.
- Unit (TypeScript, `src/intake.test.ts`): group budget, DM routing by marker
  and by quoted message, inbound dedup, group creation with the operator only
  and within the budget, failure fallback without retry, leaving only after
  the last messages, intake groups in the target allowlist, `[[…]]` TOML.
- Web (`src/lib/intake.test.ts`): action availability per stage, stage
  colours, labels.
- End to end with real tmux + claude sessions (opt-in):
  `cargo test -p nucleus --test intake_pipeline_it -- --ignored --nocapture`
  runs `nucleus intake tick` against a fake `gh` (shell script) and a local
  bare remote: the eval (read-only) and implementation (code) agents run in
  `nucleus-test-intake`; the item reaches a draft PR with passing tests; the
  non-collaborator's comment is absent from every brief; the operator's
  comment approval posts exactly one comment; every thread message is queued
  for the DM with the `[#1]` marker.
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
intake list` (until a newer item of the same issue replaces it), `nucleus
intake show`, the dashboard and a thread note. Re-adding the label produces
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
which the worker session receives in its header, are code-owned (`Intake
item #<n> — evaluation | refinement | implementation`), and so are branch
names (`nucleus/item-<n>`), which the implementation brief names. When the
WhatsApp DM session runs `nucleus intake list` or `show`, the whole output
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

Nucleus keeps one bare mirror per repo (`<work_dir>/<owner>__<name>/mirror.git`)
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

**Credentials Nucleus owns.** Remotes must be HTTPS (`[intake.github]
remote_url`, default `https://github.com/{repo}.git`; a local absolute path
is accepted for tests). The only credential helper is
`!<gh> auth git-credential`, with `<gh>` the absolute path of
`[intake.github] gh_bin` (a bare name is resolved through `PATH`). SSH is
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
when the clone was prepared), with author and committer `[intake]
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
against `[intake] import_max_entries` (default 1 000 000) as it is read,
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
copied into a private rules directory under its own limit (`[intake]
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
process intake starts goes through one function (`Pin::command`) that
checks the hash immediately before it creates the process, local git
operations included; the `gh` that git runs as credential helper is checked
before the git process that may run it; the secret guard script's `git` is
a link to the pinned git. A changed file is refused (the process is not
started) and the item is blocked. With intake enabled and a repo
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
the item at every stage, including `pr` and `review`. Events without an
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
published (the agent's message goes to the operator). The proposed issue
comment's `{summary}` is at most 600 characters on one line, with
Markdown and HTML characters escaped, `@` replaced and URLs broken.

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
an added line. The diff is read through a bounded reader. One longer than `[intake]
scan_max_bytes` (default 16 MiB) blocks the item with that reason: it is
never cut and passed to the guard. (The guard script reads its whole input,
so the bound is what keeps its memory bounded.)

Limit: legitimate content that matches a pattern (an email address in the
repository's code) blocks the item; there is no override.

### Finding 5 — WhatsApp approval identity

The operator is the first `WHATSAPP_ALLOWED_DM_JIDS` entry, normalized to
digits. In an intake group only messages from that identity are read, with
the same check the other groups use (digits of the phone JID, or the phone
number the connection's LID mapping gives for an `@lid` sender). DM messages
are routed to an item only from the operator's DM; another allowed DM
sender's messages go to the chat session. Each routed message stores how it
was written (`input_kind`: `text`, `voice` for a transcription, `forwarded`);
the pipeline accepts a command only from `text` and keeps any other
command-shaped message in the thread with a refusal note. A new group's
membership baseline is set from the create response. A member in that
response that is neither the bot nor the operator starts the group disabled
and alerts the operator in DM; a later change of the member list disables
the group (no message from it is read) and alerts the operator.

Limit: an `@lid` sender whose phone number the connection does not know is
not recognized as the operator.

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
that. The operator ends it with `nucleus intake group-resolve <n> --left`
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

Every operator message read from WhatsApp has a row in intake.db's
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
import within the limit (`core/tests/intake_check_ignore_timeout.rs`).

Round 5: a 4 GiB sparse `.gitignore` is refused quickly without being
parsed (`a_huge_sparse_gitignore_is_refused_without_being_parsed`); the
imported files match what git adds for a root `.gitignore` with a negation,
a nested `.gitignore` and an ignored directory (which holds a FIFO that is
never reached), and a base-tracked file matching a rule is still imported
(`ignore_rules_match_git_and_tracked_files_stay`); a symlink whose target
passes the per-file or total limit is refused before it is created
(`symlink_targets_count_against_the_limits`); an empty intake.db file and one
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
is refused at the first local git call (`core/tests/intake_git_pin.rs`) and
a missing gh stops intake (`tools.rs`); a forged marker from another author
and a PR by another account are ignored
(`forged_markers_and_foreign_pull_requests_are_ignored`); a claim and its
nonce are one write (`intake.test.ts`); a changed submodule URL is refused
(`gitmodules_must_stay_as_in_the_base`); objects arrive as one pack and old
scratch is swept (`objects_arrive_as_one_pack_and_old_scratch_is_swept`);
paths stay bytes (`non_utf8_file_names_survive_an_import`); the dashboard
with intake disabled or no database shows nothing and writes nothing
(`disabled_intake_shows_nothing_and_writes_nothing`,
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
(`intake.test.ts`), and the pipeline never counts an unknown or quarantined
group as closed (`an_unknown_group_is_never_counted_as_closed`).

Round 2 (after a second review): an issue title with fence-escape and
instruction text reaches the typed worker message only inside the fence
(`an_issue_title_reaches_workers_only_inside_the_fence`, and
`session_output_is_fenced` for the CLI); global git configuration in a
temporary HOME (URL rewrite, ssh command, fsmonitor, hooks path, global
attributes with a required filter, global ignore) has no effect
(`core/tests/intake_git_home.rs`); malicious configuration, hooks, filters
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
(`intake.test.ts`; round 3 replaced the token with a random nonce and
removed the `absent` result), and the pipeline never counts an unknown
group as closed (`an_unknown_group_is_never_counted_as_closed`).

Round 1:

Rust (`cargo test -p nucleus-core intake`, `whatsapp_queue`): the gate
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
reconciliation of active groups. TypeScript (`src/intake.test.ts`): operator
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

`core/src/intake/hidden.rs` scans the raw issue title, the raw issue body,
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
decoded value. Tag syntax itself is never rendered: every invisible
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
- **Told.** One thread message (`[intake.texts] item_held`: the counts by
  kind, the first three findings shortened to one line, the hold code, and
  where to read everything) goes through the outbound queue, so the target
  policy and the secret filter apply. An item without a WhatsApp thread yet
  gets the DM as its surface. The complete findings and sources are on the
  dashboard's item page (each finding in full, and each raw source with the
  hidden ranges marked) and in `nucleus intake show <n> --hidden`.
- **Release, bound to the viewed hold (fix round 1).** Every release names
  the hold the operator reviewed: `nucleus intake release <n> --hold
  <code>` (operator terminal only; `show` prints the code and the
  fingerprint), the dashboard's release button (sends the `hold_hash` the
  panel rendered), or `#<n> release <code>` typed by the operator (the
  hold code is the first 6 hex characters of the fingerprint and is in the
  held message; only the operator's identity and `text` input count). A
  release without a code, or with the code of another hold, is refused
  with the current code. The named hold is compared with the current
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
- **Off.** `[intake] hidden_content_hold = false` turns the check off; a
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

Detector (`cargo test -p nucleus-core intake::hidden`, tests in
`core/src/intake/hidden/tests.rs`): each kind with the case that must be
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
(`intake::pipeline`): held before any task with the complete finding and
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

## Rejected alternatives

- **Webhooks.** They need public ingress, which Nucleus does not have
  ([[ADR-011]]); a poll every few minutes is fast enough for issues.
- **One long refinement session per item, resumed per turn.** A session's
  context lasts only as long as its transcript can be resumed; ADR-029
  removed ad hoc resume. The thread in intake.db is the durable record, so
  each turn's brief carries it.
- **Letting the refinement agent decide that a plan is approved.** Approval
  releases implementation work; a model reading "looks good" is not an
  authorization. Code reads a whole-message command from the operator, bound
  to a plan version. (ADR-033 rejected code-parsed commands for tasks
  because the model maps plain language well enough there; an approval gate
  is different.)
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
  intake.db would get a second writer family.

## Consequences

- A labeled issue on a configured repo starts an eval within a few minutes
  (the poll interval) and needs no operator action when it is simple, until
  the comment decision.
- The operator's WhatsApp gets up to `max_groups_per_day` new groups a day;
  each is left when its item closes. Items past the limit use the DM with
  `[#n]` markers.
- Each refinement turn re-reads the repository it needs, since it starts a
  new session; long threads are cut to their most recent part in the brief.
- Nucleus's own test run holds that item's lock for up to
  `test_timeout_minutes`; other items continue.
- The `code` profile is a denylist; until an OS sandbox exists, an
  implementation agent acts with the operator's user rights on the machine.
