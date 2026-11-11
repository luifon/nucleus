# ADR-046 — "What needs you" view

Date: 2026-11-11
Status: proposed

**Relates to:**
- [ADR-015](ADR-015-nucleus-dashboard-unified-operator-app.md) — the
  dashboard gets a new page, always visible, that lists what waits for the
  operator. ADR-015 is amended through ADR-040 §6 for the routes that
  approve, accept or unlock: they need a fresh passkey or TOTP
  confirmation. Reading the view keeps ADR-015's tailnet-only model.
- [ADR-038](ADR-038-skill-suggestions-and-transcript-sweep.md) — amended.
  The nightly skill-suggestion WhatsApp notice ("WhatsApp notice":
  `notice_one` / `notice_many`, `[skill_learner] notify`) is removed in the
  same change that adds the daily message of this ADR. Pending suggestions
  are reported through that daily message.
- [ADR-040](ADR-040-operator-approvals.md) — every open approval request,
  including ADR-041 delete escalations, is listed.
- ADR-042 (agent profiles) — the monthly agent track-record review and
  knowledge suggestions are listed.
- ADR-045 (browser logins through Vaultwarden) — login steps that need the
  operator are listed.
- [ADR-036](ADR-036-work-items-and-issue-pipeline.md) — work items waiting
  for a decision are listed; the WhatsApp message follows Rule 14's limits
  on what text may reach WhatsApp and its message-splitting rule.
- [ADR-006](ADR-006-reminders.md) — failed reminder fires are listed.
- [ADR-033](ADR-033-turn-engine-and-task-ledger.md) — the daily message
  goes through `outbound_queue` (target policy, secret filter).

## Context

What waits on the operator is spread over several places in Nucleus: work
items (ADR-036), background tasks (ADR-033), reminder failures (ADR-006),
skill suggestions (ADR-038) and job applications. He has to check each one.

The surveyed products each give one place to see what waits on the user:
a primary bot that collects it, a badge, or a morning brief. The operator
expects this view to help with the approval and login requests of the new
designs as well as with everything that already exists.

## Decision

### 1. Built by code

The list is built by code from the stores of each source. No model adds or
drops an item. Each line follows the "what depends on me" format: one line
per item, with a recommendation.

The line and its default recommendation are code-written per item kind,
for example "approve or deny #N", "retry fire #N", "unlock the broker".
Where the source already holds a reviewer's recommendation (the ADR-042
monthly review, a suggestion's rationale), the dashboard may also show it,
labelled as that source's text, after the secret filter and a length cap.

### 2. Contents

- Work items waiting for a decision (ADR-036).
- Pending approval requests (ADR-040), including delete escalations
  (ADR-041).
- Login steps that need the operator (ADR-045).
- Skill suggestions (ADR-038) and knowledge suggestions (ADR-042).
- Agent track-record reviews (ADR-042): the monthly keep / split / merge /
  retire recommendations.
- Failed reminder fires (ADR-006).
- Job applications that need action.
- Unanswered questions from sessions.

### 3. Dashboard page

A dashboard page (ADR-015) shows the list and is always visible. Each line
links to the page where the operator acts on the item. Reading the list
needs no confirmation. An action that approves, accepts or unlocks (an
approval request, a suggestion accept, a broker unlock) requires the
passkey or TOTP confirmation of ADR-040 §6, whether it is taken on the
item's page or from this view; one confirmation unlocks that browser for
15 minutes.

### 4. Daily WhatsApp message

- One message per day, sent only when the list is not empty.
- Default: every day at 09:00, in `NUCLEUS_TZ`. The days and the time are
  `nucleus.toml` settings.
- It carries the item lines, built only from code-owned fields: the item
  kind, its number, its state and the code-written recommendation. It
  never carries issue text, titles from outside sources, plans or model
  text (Rule 14). It ends with the dashboard link built from
  `NUCLEUS_PUBLIC_URL`; without that variable there is no link. The URL
  stays bare (WhatsApp breaks a link inside `*` markers).
- A long list is split into messages of at most 1 500 characters, with the
  same rule as the existing ADR-036 reports.
- The message goes through `outbound_queue` with the target `dm`, so the
  target policy sends it only to the operator and the secret filter reads
  it.
- It replaces ADR-038's nightly skill-suggestion notice. The old notice,
  its texts and its setting are removed in the same change.

## Consequences

- The operator has one page and one daily message for everything that
  waits on him, instead of checking each source.
- New pending skill suggestions no longer produce a message on the night
  they are filed; they appear in the next daily message.
- No WhatsApp message on a day with nothing pending. With the default
  settings every day, weekends included, gets a message when the list is
  not empty; a day is excluded only when the operator changes the days
  setting.
- Every new source of pending work needs code that reads its store and a
  code-written line and recommendation for its item kind before it appears
  in the view.
- The WhatsApp message cannot show a reviewer's own recommendation text;
  only the dashboard shows it, labelled.

## Rejected alternatives

- **A model-written brief.** A model could add or drop items; the list
  must be complete and exact, so code builds it.
- **Keeping ADR-038's nightly notice next to the daily message.** Two
  messages for the same pending suggestions; the old notice is removed in
  the same change.

## Open questions / footnotes

1. The dashboard route name and its place in the navigation are decided at
   build time.
2. "Unanswered questions from sessions" needs a source that records a
   session's open question; which store holds it is decided at build time.
3. The length cap for a source's own recommendation text on the dashboard
   is decided at build time.

## Verification

- Builder, unit: one fixture per source (a work item waiting for plan
  approval, an approval request, a login step, a skill and a knowledge
  suggestion, an agent review, a failed fire, a job application, an
  unanswered question) gives exactly one line with its code-written
  recommendation; a source with nothing pending gives no line; nothing
  from model text or outside text appears in a line.
- Dashboard: the page lists every fixture item, each line links to its
  item page, and a source recommendation is shown labelled after the
  secret filter and the length cap.
- Daily message: with a non-empty list at 09:00 on any day of the week,
  weekends included, one message
  (or a split set within 1 500 characters each) is queued to `dm`; an
  empty list, or a day outside the settings, queues nothing; changing the
  days and the time in `nucleus.toml` changes when it is sent; the link is
  bare and absent without `NUCLEUS_PUBLIC_URL`.
- Dashboard actions: an approve, accept or unlock taken from the view
  without the ADR-040 §6 cookie is refused; reading the view needs no
  confirmation.
- ADR-038 removal: the nightly `learn` run with new pending suggestions
  queues no message, and a `nucleus.toml` that still sets
  `[skill_learner] notify` loads.
