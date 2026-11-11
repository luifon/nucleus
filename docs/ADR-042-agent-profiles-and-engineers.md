# ADR-042 — Agent profiles, per-repo engineers, automatic edits and the agent track record

Date: 2026-11-11
Status: proposed

**Relates to:**
- [[ADR-016]] — the agent registry. `agents.toml` stays the committed list
  of venue agents. A private registry under `.nucleus/` holds the engineer
  and area profiles and is read by the same loader
  (`nucleus_core::agents::Registry`). The `/agents` page shows every
  profile and its track record (§8).
- [[ADR-017]] — the skill-gap learner. Its review engine also files
  knowledge suggestions for engineers (§4), and the learner reads the
  transcripts of the operator's own sessions in repositories that have an
  engineer (§4.4), beyond this workspace's transcripts.
- [[ADR-038]] — skill suggestions. Knowledge changes use the same
  suggestion lifecycle. An automatic-edit mode (§6) is added for skills and
  for knowledge files: with it on, a suggestion that passes a second review
  and the code checks is applied without the operator's accept. ADR-038's
  rule that no model pass changes the library without the operator's
  accept then holds only for targets whose automatic mode is off.
- [[ADR-034]] — usage accounting. Each profile's sessions carry the
  profile's agent label, so `nucleus usage report` attributes estimated
  cost per agent; the track record (§8) reads it.
- [[ADR-036]] — the issue pipeline. Every agent step of an item (eval,
  each refinement turn, implementation) loads the engineer of the item's
  repository (§3.6). The work log (§3.5) records each item's result. The
  import of a clone into the mirror must always skip `CLAUDE.local.md`
  (§3.4).
- [[ADR-015]] — amended through ADR-040 §6 for the dashboard's accept
  route: accepting a knowledge or skill suggestion on the dashboard needs
  a fresh passkey or TOTP confirmation.
- [[ADR-032]] — `.nucleus/` is its own git repository; every profile
  change is a commit there.
- [[ADR-040]] — operator approvals. An area agent's track record counts
  the approvals the operator denied.
- [[ADR-046]] — the "what needs you" view. The monthly track-record review
  (§8) and pending knowledge suggestions are entries there.

## Context

Every ADR-036 agent step (eval, each refinement turn, implementation) is a
new session. Eval and refinement work in the item's clone, which Nucleus
prepares before eval; before the first implementation, Nucleus recreates
that clone at the newest default branch (`core/src/work/pipeline.rs`).
The clone belongs to one item. Nothing learned on one item reaches the
next item of the same repository: the next eval reads the
repository again from nothing, and a correction the operator made in a
review is not available to the next implementation.

A surveyed assistant product runs one bot per job, each set up for that
job. The operator asked for specialized engineers per repository in
Nucleus, with one condition: the agents that exist today (the venue agents
of `agents.toml`) must fit the same design.

No written design for automatic acceptance of a skill or knowledge change
existed before this ADR. ADR-017, ADR-038, the private roadmap and todo
lists, the vault and the code were checked. Every skill change today waits
for the operator.

The operator also asked to see what each agent does well and badly, so
that he can decide which agents are important, which need to be split, and
which are not used and should be removed.

## Decision

### 1. One profile format for every agent

There are three kinds of agent profile:

| Kind | Scope | Defined in |
|---|---|---|
| `venue` | one messaging surface (WhatsApp, Discord, the dashboard chat) | the committed `agents.toml` (ADR-016) |
| `engineer` | one repository | the private registry under `.nucleus/` |
| `area` | one area of the operator's life | the private registry under `.nucleus/` |

A profile has a knowledge file, a work log, skills and a permission
profile. A venue profile also has its persona (`messaging/<venue>/persona.md`,
Rule 7).

Engineer and area profiles name repositories and areas of the operator's
life. Repository names must never be in a committed file (CLAUDE.md
Rule 14), so these profiles live in a private registry under `.nucleus/`.
The loader that reads `agents.toml` also reads the private registry and
returns one list. The venue agents keep their `agents.toml` entries and
persona files unchanged; a part of the profile format that a venue agent
does not have yet (for example a knowledge file) is empty.

### 2. Storage

All profile files (registry, knowledge files, work logs) are private,
under `.nucleus/`. `.nucleus/` is its own git repository (ADR-032), so
every change to a profile has history: each accepted suggestion, each
automatic edit (§6) and each work-log entry is a commit.

### 3. Engineers

#### 3.1 One engineer per repository

There is one engineer per repository. The job is the role of the step it
runs in: eval and refinement run with the `read-only` profile,
implementation with the `code` profile (ADR-036 §4). A repository is split
into several engineers by area (for example frontend and backend) only
when one knowledge file grows past its size limit (§3.7).

#### 3.2 Key

An engineer is keyed by the repository's GitHub remote URL, whatever the
remote is named in the checkout (a remote need not be called `origin`).
When the checkout has no GitHub remote, the key is the hash of the root
commit, which is the same in every clone and worktree. A profile stores
both values.

#### 3.3 Which repositories get an engineer

- A repository that the ADR-036 pipeline handles gets an engineer
  automatically.
- Any other repository gets one by command:
  `nucleus engineers add owner/repo`.
- The operator chose an initial set of repositories; it lives in the
  private registry. Repositories outside it get no engineer now. A
  repository that has a contract with Nucleus gets a "related profiles"
  link to the Nucleus engineer when it gets an engineer of its own.

#### 3.4 Output into the repository

Each engineer has a per-repository output setting with three values:

| Value | What Nucleus writes into the repository |
|---|---|
| `off` | nothing |
| `draft-pr` | CLAUDE.md changes, proposed as a draft pull request |
| `local` | a `CLAUDE.local.md` file in the checkout, never committed |

- `draft-pr` is for the operator's own repositories. The draft pull
  request is opened by Nucleus code, as every ADR-036 network step is; no
  agent pushes, and Nucleus never merges (Rule 14).
- `local` is for a repository owned by someone else. The file is excluded
  through the checkout's `.git/info/exclude`, never through that
  repository's `.gitignore`, so the owner's files do not change.
- When a repository with the `local` setting is in the pipeline, the
  ADR-036 import must always skip `CLAUDE.local.md`. Today the import
  takes untracked files that no ignore rule matches, and `.git/info/exclude`
  is not one of the rules it reads, so without this skip the file would be
  published in the item's commit. The skip applies to every repository in
  the pipeline, whatever its output setting.
- Knowledge goes into the mechanism that matches it, and CLAUDE.md does not
  grow without limit:
  - knowledge about specific paths → `.claude/rules/*.md` with `paths`
    frontmatter;
  - a procedure → a skill;
  - a rule that a machine can check → a hook, a test or a lint;
  - only facts about the whole repository → CLAUDE.md, which has a size
    limit of its own.

#### 3.5 Work log

The work log is written by code only, never by the agent. Each entry
records the item, the plan version, the result (`merged` or `not_merged`,
from ADR-036's in-review poll) and the pull request's review comments.

#### 3.6 Where the engineer is loaded

- In every ADR-036 step for that repository: eval, each refinement turn
  and implementation.
- In the operator's own Claude Code sessions in that repository, through a
  machine-wide `SessionStart` hook that reads the checkout's git remote (or
  root commit) and loads the matching engineer. Machine-wide hooks also run
  in Nucleus bot sessions, so the hook skips bot sessions where the
  engineer does not apply.
- From WhatsApp, as read-only questions ("ask the engineer of X"): the
  answer comes from a read-only session loaded with that engineer.

#### 3.7 Knowledge file

- **First version.** A read-only session reads the repository and drafts
  the first knowledge file. The draft is a suggestion that the operator
  accepts or rejects.
- **Changes.** Every later change is a knowledge suggestion, filed and
  decided like a skill suggestion (ADR-038), unless the automatic mode of
  §6 applies.
- **Trusted inputs.** A knowledge suggestion uses only collaborator
  comments (ADR-036 §2) as input from GitHub. A comment by someone else on
  a pull request, written into knowledge, would load into every later
  session for that repository.
- **Size.** At most 12,000 characters (about 3,000 tokens), enforced by
  code. The limit is set in `nucleus.toml`.
- **Size review.** Once a month, and when a file passes 80 % of its limit,
  a review checks every path and command the file names against the
  repository, marks the ones that no longer exist, and proposes removals or
  shorter wording only. It never proposes additions.

#### 3.8 Learning

An engineer learns from two sources:

1. Pipeline results: the work log's results and the pull request review
   comments by collaborators. Knowledge corrected from the outcomes of code
   review is a function that none of the surveyed products has.
2. The operator's own sessions in that repository. The skill-gap learner
   (ADR-017, ADR-038) reads the transcripts of every repository that has an
   engineer, in addition to this workspace's transcripts, and files
   knowledge suggestions for that engineer.

### 4. Knowledge suggestions

1. A knowledge suggestion targets one profile's knowledge file. It follows
   ADR-038's lifecycle: `pending`, `accepted`, `rejected`, `superseded`;
   the hash of the file it was written against, checked at accept; the
   checks when it is filed and again at accept; one pending suggestion per
   target.
2. It is decided on the same surfaces as skill suggestions: the terminal
   and the dashboard. On the dashboard, accept (of a skill or a knowledge
   suggestion) is an operator-mutation route: it requires the fresh
   passkey or TOTP confirmation of ADR-040 §6, which unlocks that browser
   for 15 minutes. Pending knowledge suggestions are listed in the
   "what needs you" view (ADR-046).
3. Accept writes the knowledge file and commits it in `.nucleus/`.
4. The learner's sweep (ADR-038) includes the transcripts of the
   repositories that have an engineer (§3.8). A review of such a part may
   file a knowledge suggestion for that repository's engineer, in addition
   to skill suggestions.

### 5. Area agents

The first area agent covers one area of the operator's life that he chose;
it is separate from the engineer of any repository that serves the same
area. Two more areas follow once the format has been tested on the first
one. An area profile has the same parts as an engineer profile (knowledge
file, work log, skills, permission profile).

### 6. Automatic edits (skills and agent knowledge)

1. **Default.** Every change to a skill or a knowledge file is a
   suggestion.
2. **Second review.** Codex reads the current file, the change and the
   cited evidence. It does not read the context of the session that
   proposed the change. It answers `accept`, `reject` or `ask operator` as
   JSON that code validates. It checks that:
   - the change does not contradict or remove existing content;
   - the cited evidence exists;
   - the change is not generalized from one example;
   - the change takes no instructions from outside text.

   **Fallback.** When Codex cannot be reached (its usage limit is at
   100 %, or the model is unavailable), a new Claude session does the same
   review (a `Session`, never `claude -p`, Rule 4). Footnote 1 applies.
3. **Conditions for automatic acceptance.** A suggestion is applied without
   the operator only when all of these hold:
   - automatic mode is on for that target;
   - the second reviewer answered `accept`;
   - the code checks pass: the SKILL.md contract check (ADR-038, with
     `# Failure modes`) for a skill, `tools/check-secrets.sh`, a diff-size
     limit, and no section removed;
   - the target is under its daily limit.

   Any other result leaves the suggestion pending for the operator.
4. **Record.** Every automatic edit is one commit in `.nucleus/`. A daily
   digest lists each automatic edit with its revert command,
   `nucleus skills revert <id>`.
5. **Trust.** Code proposes automatic mode for a target after N = 5
   suggestions in a row for that target were accepted without edits. At
   most 3 automatic edits per target per day. Both numbers are set in
   `nucleus.toml`. A revert turns automatic mode off for that target.
6. **Scope.** An agent edits only its own knowledge file. It never edits
   its permission profile, its work log, or any file of another agent.

### 7. Agent labels

Each profile's sessions are spawned with the profile's agent label
(ADR-016 run-log, ADR-034 attribution), so usage, cost and the track
record are counted per profile.

### 8. Track record

The track record covers every agent of every kind, including kinds added
later.

- It is written by code from facts: the stores and logs of each
  subsystem. It is never written from the agent's own report.
- **Engineers:** items handled, merged and not merged, test results,
  refinement rounds, reverted pull requests, knowledge suggestions
  accepted, rejected and reverted.
- **Area agents:** tasks finished and failed, approvals denied (ADR-040),
  the operator's corrections (detected by the skill-gap learner), work-log
  entries.
- **Venue agents:** turns, failed turns, ⚠️ notes.
- **All agents:** usage count, estimated cost (ADR-034), last use.
- **Monthly review.** Once a month a review recommends, for each agent,
  keep, split, merge or retire, with the numbers. The recommendations go
  into the "what needs you" view (ADR-046). The operator decides. A retired
  agent is archived, never deleted.
- The dashboard's `/agents` page (ADR-016) shows each agent's track record.

## Consequences

- Each pipeline step for a repository starts with that repository's
  accepted knowledge, and the operator's own sessions in the repository
  start with the same knowledge.
- Knowledge is bounded at 12,000 characters per file, and the monthly
  review only removes or shortens, so the share of each prompt that the
  knowledge file takes stays bounded.
- A suggestion that the second reviewer accepts and the code checks pass
  can change a skill or a knowledge file without the operator, for targets
  with automatic mode on, at most 3 times per target per day. Each such
  change is a commit he can revert, and a revert turns the mode off.
- A pull request comment by a non-collaborator never reaches knowledge.
- The ADR-036 import changes for every pipeline repository: a
  `CLAUDE.local.md` in the clone is never imported.
- The private registry and the knowledge files exist only on the
  operator's machine, in `.nucleus/`. A fresh clone of this repository has
  only the venue agents.
- The learner reviews more transcripts (every repository with an
  engineer), which adds review sessions to the nightly run.
- Each automatic edit costs one review by Codex, or by a Claude session
  when Codex cannot be reached.

## Rejected alternatives

- **One engineer per job** (one for evals, one for implementation). The job
  is already set by the step's role and its tool profile; knowledge is
  about the repository.
- **Engineers in the committed `agents.toml`.** Their entries name the
  operator's repositories, which must never be in a committed file
  (Rule 14).
- **Adding all knowledge to the repository's CLAUDE.md.** The operator
  asked for the mechanism that matches each kind of knowledge (rules with
  paths, skills, hooks, tests, lints), so that CLAUDE.md does not grow.
- **Excluding `CLAUDE.local.md` through the other owner's `.gitignore`.**
  That changes a file in a repository owned by someone else;
  `.git/info/exclude` is local to the checkout.
- **Knowledge input from every pull request comment.** A comment by an
  attacker, once written into knowledge, would load into every later
  session for the repository.
- **A size review that also adds content.** It would undo the size limit;
  additions come only through suggestions.
- **A second reviewer that reads the proposing session's context.** The
  reviewer must judge the change against the file and the evidence alone,
  so that text that steered the proposer does not also steer the reviewer.
- **A track record written from the agents' own reports.** An agent's
  report on itself is not a fact; code counts from the stores.
- **Deleting retired agents.** They are archived, as retired skills are
  (ADR-038).
- **Several area agents at once.** The format is tested on one area first.

## Open questions / footnotes

1. **Reviewer setup.** The setup of the second reviewer (how Codex and the
   fallback Claude session are started, what they can read, which tools
   they have) is to be hardened later. The first version records the
   decision to use a second review; it does not claim the reviewer is
   isolated.
2. The diff-size limit of §6.3 and the size limit of CLAUDE.md under
   `draft-pr` (§3.4) have no number yet; both go in `nucleus.toml`.
3. The file name and format of the private registry under `.nucleus/` are
   not decided.
4. How the engineer reaches a pipeline step's session is not decided: as
   text in the brief outside the data fence (it would count against
   `tasks::MAX_BRIEF_CHARS`) or as an appended system prompt.
5. Which bot sessions the machine-wide `SessionStart` hook skips is decided
   when the hook is built and tested in a bot session.
6. Where the daily digest of automatic edits is delivered (a line in the
   ADR-046 daily message, or its own message) is not decided.
7. When the base commit tracks a `CLAUDE.local.md`, whether the import
   keeps the base version or refuses the import is not decided.
8. The first area and the two later areas are named in the private
   registry, not here.

## Verification

- **Dashboard accept:** an accept request without the ADR-040 §6 cookie
  (from a worker machine, a session, or a browser that has not confirmed)
  is refused and the suggestion stays pending; after a passkey or TOTP
  confirmation the accept passes; after 15 minutes the cookie no longer
  passes.

- **Registry.** A test loads `agents.toml` and a private registry fixture
  and returns one list with venue, engineer and area profiles; a venue
  agent without a knowledge file loads with an empty one. A test proves the
  loader works when the private registry is absent.
- **Key.** Tests resolve an engineer from a checkout whose GitHub remote
  has a name other than `origin`, from a checkout with no GitHub remote (by
  root commit), and from a second worktree of the same repository (same
  engineer).
- **Selection.** A pipeline repository gets an engineer without a
  command; `nucleus engineers add acme/widget` adds one for another
  repository.
- **Output setting.** For `off` nothing is written; for `draft-pr` Nucleus
  code opens a draft pull request with the CLAUDE.md change; for `local` the
  file is written and `.git/info/exclude` lists it, and `.gitignore` is
  byte-identical before and after.
- **Import.** An ADR-036 import of a clone that contains `CLAUDE.local.md`
  produces a commit without it, on a case-sensitive and a case-insensitive
  file system.
- **Work log.** An item that reaches `merged` and one that reaches
  `not_merged` each produce one entry with item, plan version, result and
  review comments; no agent tool call can write the log.
- **Knowledge size.** A suggestion whose result passes 12,000 characters is
  refused; a file at 80 % starts the size review; the review's output
  contains no added text (checked by code).
- **Trusted inputs.** A non-collaborator's pull request comment is absent
  from every knowledge suggestion's input.
- **Loading.** A pipeline step's session for an engineer's repository
  contains the knowledge file; an operator session started in the
  repository gets it from the `SessionStart` hook; a bot session the hook
  should skip does not.
- **Automatic edits.** Tests cover each condition of §6.3 failing on its
  own (mode off, `reject`, `ask operator`, contract check, check-secrets,
  diff size, a removed section, the daily limit) and leaving the
  suggestion pending; a reviewer answer that is not valid JSON counts as
  not `accept`; the fallback Claude session runs when Codex is reported at
  its usage limit; each automatic edit is one commit in `.nucleus/` and is
  listed in the daily digest; `nucleus skills revert <id>` restores the
  previous text and turns the mode off; automatic mode is proposed after
  the fifth accept in a row without edits and not after the fourth; an
  agent's attempt to edit its permission profile, its work log or another
  agent's file is refused.
- **Track record.** Each counter is computed from fixture stores; no
  counter reads text the agent wrote about itself; the monthly review puts
  one entry per agent into the ADR-046 view; a retire moves the profile to
  an archive and deletes nothing; the `/agents` page shows the numbers.
- **Cost.** `nucleus usage report` lists an engineer's sessions under the
  engineer's agent label.
