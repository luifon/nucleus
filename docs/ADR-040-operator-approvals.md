# ADR-040 — "Ask me first" approvals

Date: 2026-11-11
Status: proposed

**Relates to:**
- [ADR-003](ADR-003-permissions.md) — amended. ADR-003 has two levels for
  a tool call: allowed (auto mode and its classifier) and denied (the
  `--disallowedTools` denylist). This ADR adds a third level between them:
  the call is held until the operator approves it.
- [ADR-036](ADR-036-work-items-and-issue-pipeline.md) — reused. Approval
  answers on WhatsApp are read by the ADR-036 interpreter
  (`SessionProfile::one_shot_no_tools`, code-built list of pending
  decisions, JSON validated by code) and follow its confirmation rules.
  The work pipeline's own `git push` stays outside this ADR.
- [ADR-033](ADR-033-turn-engine-and-task-ledger.md) — the operator's DM
  path, the `outbound_queue`, the target policy and the secret filter carry
  every approval message; background tasks are one of the sources of
  requests.
- [ADR-021](ADR-021-agent-session-messaging.md) — the message Nucleus
  types into a waiting session after an approval is a code-written typed
  prompt.
- [ADR-015](ADR-015-nucleus-dashboard-unified-operator-app.md) — amended
  for the operator-mutation routes only. ADR-015 authenticates the operator
  routes by tailnet membership alone. The routes that approve, accept or
  unlock (the Allow and Deny buttons of this ADR, suggestion accept of
  ADR-038 and ADR-042, the broker unlock and the live login view of
  ADR-045, and the actions reached from ADR-046's view) also require a
  fresh operator confirmation (§6). The rest of the dashboard keeps
  ADR-015's tailnet-only model.
- [ADR-020](ADR-020-architecture-hardening.md) — the request store has one
  writer and one transaction per state change.
- [ADR-037](ADR-037-ntfy-push-channel.md) — a push with Allow and Deny
  buttons is a possible later channel (footnote 2).
- ADR-041 (delete guard and write sandbox) — uses the same request record,
  which holds the action hash and the delete's dry-run manifest; its
  escalations are approval requests.
- ADR-046 ("what needs you") — lists every open request.

## Context

Every always-on assistant product surveyed in 2026-11 has a level between
"allowed" and "denied": an automatic review of risky actions, approval
answers in chat, or a guard that holds an action until the user answers.
Nucleus has auto mode plus a denylist (ADR-003). A tool call is either run
or refused; nothing waits for the operator.

Rule 6 of the repository's `CLAUDE.md` (outbound messages to shared
audiences need the operator's explicit authorization) is written policy.
No code enforces it. The same holds for the operator's rule that a
`git push` happens only after he has reviewed the change.

## Decision

### 1. Scope

Only actions that Nucleus has built, or that the operator uses, need an
approval:

- Sending email, through the mail connector or the local mail app.
- Deleting or moving many mail messages at once.
- Creating a calendar event that invites anyone other than the operator.
- Browser form submissions: issuing or cancelling a service invoice on the
  government portal, and submitting a job application.
- `git push` outside the work pipeline (ADR-036). The work pipeline's
  pushes are Nucleus code and keep their own checks.
- Deleting files outside the session's own working folder. ADR-041 decides
  when a delete needs the operator; when it does, it creates a request of
  this ADR.

Out of scope: posting to social media, GitHub comments outside the work
pipeline, and payments. Nucleus does not do these and will not.

WhatsApp messages are out of scope as well. The target policy
(`messaging/whatsapp/src/target_policy.ts`) and the allowed IDs
(`WHATSAPP_ALLOWED_DM_JIDS`, the configured groups) already decide whom the
bot can reach. See footnote 1 for the planned contacts feature.

### 2. Mechanism

1. A `PreToolUse` hook recognises an action in the scope above and
   resolves it (see "The approved object" below). It refuses the tool call
   and records a request: a request number, the session that made it, the
   agent, the resolved action in readable form, and the **action hash**: a
   hash of the resolved action.
2. The hook's refusal text tells the session to end its turn with
   "waiting for approval #N". The session does not retry on its own.
3. When the operator approves, Nucleus types a code-written message into
   that session (a typed prompt, ADR-021 and ADR-033). The hook then allows
   that exact action once, within 10 minutes of the approval. At that
   attempt the hook resolves the action again and computes its action hash
   again; it allows the call only when the new hash equals the approved
   one. An action whose hash differs is a new request.
4. The request record and the check are shared with the delete guard
   (ADR-041); see "One request record" below.

A denial is also typed into the session as a code-written message, so the
session can stop or choose another path.

**The approved object.** The operator approves the resolved action, not
only the text of the command or the tool input. The action hash covers:

- the tool name and its full input (the full command for Bash);
- every destination: the recipients of an email (to, cc, bcc), the
  invitees of a calendar event, the remote and ref of a push, the folder
  or label of a bulk mail move;
- a hash of the material payload: the body, the bytes of every attachment,
  and the contents of every file the command reads to build the action
  (for example a file that holds the email body or the recipient list);
- the execution context: the working folder, the session and the agent;
- for a browser form submission: the origin of the page and a hash of the
  field values that the submission sends.

A file-backed or script-backed action whose payload the hook cannot
resolve (a script that builds the message itself, a command whose
recipients or body come from a source the hook cannot read before the call)
is refused with a request to rewrite it in a form the hook can resolve:
the destinations and the payload as arguments or as files named in the
command.

**One request record.** ADR-040 and ADR-041 use one request record. It
holds the action hash and, for a delete, the dry-run manifest of ADR-041
(the list of what would be deleted) with its **manifest hash**. A grant
for a delete is used only when both match at execution: the command
resolves to the approved action hash, and the dry run computed again
gives the approved manifest hash. A request for an action that is not a
delete has no manifest.

What each attempt carries:

- the first attempt carries nothing; the hook records the request, or for
  a delete under ADR-041's threshold returns the manifest and its hash;
- a second attempt of a delete under ADR-041's threshold carries the
  manifest hash (ADR-041 §1.4);
- an attempt after an approval carries the request number; the hook
  computes the action hash and, for a delete, the manifest hash, and
  compares them with the hashes stored in that request.

### 3. Saved rules

An approval can be saved as a rule for one agent and one site ("always
allow here"). The job-application automation gets saved rules for its
application sites, so it keeps running without a request per form. Any
other session that submits a form on those sites still asks. A saved rule
never applies to a different agent or a different site.

### 4. Where requests go

- **A session the operator is working in** (an interactive session): the
  request goes to that session's chat. If he does not answer there within
  10 minutes, it also goes to WhatsApp.
- **Background work** (bots, reminder fires, tasks, pipeline steps): the
  request goes to the operator's WhatsApp DM and to the dashboard.
  - On WhatsApp he answers in plain words. The ADR-036 interpreter reads
    his message with a code-built list of pending requests (request number,
    a code-written description of the action, the allowed answers); it
    never receives model output or text from outside. Code runs the answer
    only for a request the list showed, bound to its request number and
    hash. ADR-036's confirmation rules apply: an answer from a voice note
    or a forwarded message, and a request the interpreter inferred while
    several wait, are confirmed first.
  - The dashboard shows Allow and Deny buttons for each request; they
    need the operator confirmation of §6.
  - WhatsApp messages are code-owned texts sent through `outbound_queue`
    (target policy, secret filter) to the operator's DM only.
- **Every request** is listed in the "what needs you" view (ADR-046),
  whichever channel it went to.

### 5. Expiry

- A request from background work expires after 30 minutes.
- A request from a chat waits until the operator answers it or sends a new
  message in that chat. A new message ends the request.
- Both values are settings in `nucleus.toml`.

An expired request is closed; the action needs a new request.

### 6. Operator confirmation on the dashboard

The dashboard has no login (ADR-015): any process that reaches it over the
tailnet can call its routes, including a worker machine or an injected
session. The routes that change state on the operator's behalf therefore
require a fresh operator confirmation that code cannot produce:

- **Covered routes:** Allow and Deny of an approval request (this ADR and
  ADR-041 delete escalations), accept of a skill or knowledge suggestion
  (ADR-038, ADR-042), the broker unlock and the input of the live login
  view (ADR-045), and every one of these reached from the "what needs you"
  view (ADR-046).
- **Confirmation:** a passkey (WebAuthn, for example Touch ID or Face ID)
  or a TOTP code (footnote 5).
- **Duration:** one confirmation unlocks that browser for 15 minutes, a
  `nucleus.toml` setting. The unlock is held in an HttpOnly, same-site
  cookie of that browser only. A request without a valid cookie is refused
  and the page asks for a new confirmation.
- **Other routes:** every other dashboard route keeps ADR-015's
  tailnet-only model.
- **WhatsApp:** decisions made in the operator's WhatsApp DM are
  unchanged: the ADR-036 interpreter and the operator-identity checks
  decide them.

Residual: during the 15 minutes, automation that drives the operator's own
browser could act through the cookie. Nucleus sessions never drive the
operator's own browser; their browsers are the isolated browsers of
ADR-022.

## Consequences

- Each action in the scope stops at the hook until the operator answers.
  Background work that reaches one waits up to 30 minutes, then fails that
  step.
- Rule 6 and the push-after-review rule are enforced by code for the
  actions in the scope.
- The job-application automation runs unattended only on sites with a
  saved rule.
- Approvals use the WhatsApp interpreter path and the dashboard; no new
  command syntax is added.
- Approving, accepting or unlocking on the dashboard needs a passkey or
  TOTP confirmation at most every 15 minutes per browser (§6).
- The scope list is fixed in code and configuration. An action outside it
  is decided by ADR-003's two levels as before.

## Rejected alternatives

- **Covering social media, GitHub comments outside the pipeline, and
  payments.** Nucleus does not do these and will not, so no rule is needed.
- **Approvals for WhatsApp messages.** The target policy and the allowed
  IDs already limit who the bot can reach. The planned contacts feature
  adds one case (footnote 1).
- **An ntfy push with Allow and Deny buttons now.** It needs the phone on
  the tailnet and a new authenticated endpoint. It is added later only if
  WhatsApp is too slow (footnote 2).

## Open questions / footnotes

1. The future contacts feature (the bot talking with selected people) asks
   before a first message that the bot starts by itself to a contact.
2. ntfy push with Allow and Deny buttons (ADR-037): later, only if WhatsApp
   is too slow. It needs the phone on the tailnet and a new authenticated
   endpoint.
3. How the hook recognises a browser form submission (which browser tool
   call, which page) for the two portals in the scope, and how it reads the
   page origin and the submitted field values for the action hash, is not
   yet decided.
4. Where the request store lives (a database under `memory/`) and its
   owning module are decided at build time, under ADR-020's single-writer
   rule.
5. Where the passkey registration and the TOTP secret are stored, so that
   no session or worker can read them, and how they are registered, is
   decided at build time.

## Verification

- Hook, unit: each action in the scope is refused and records one request
  with its hash; actions outside the scope pass; the same action after an
  approval passes once within 10 minutes, a second attempt and an attempt
  after 10 minutes are refused, and an action with a different hash is a
  new request.
- Resolved action, unit: after an approval of an email command that reads
  its body or recipients from a file, a change to that file's recipients,
  body or an attachment's bytes, with the command text unchanged, changes
  the action hash and the attempt is refused; a browser submission whose
  field values or page origin changed after the approval is refused; a
  script-backed send whose payload the hook cannot resolve is refused with
  the rewrite request.
- Shared record, unit: an approved delete whose command is unchanged but
  whose dry run now gives a different manifest is refused, and a different
  command with the same manifest is refused.
- Saved rules: an allowed agent on a saved site passes; another agent on
  the same site and the same agent on another site are refused and asked.
- Routing: a request from an interactive session goes to its chat, and to
  WhatsApp after 10 minutes without an answer; a request from background
  work goes to WhatsApp and the dashboard; every request appears in the
  ADR-046 view.
- Expiry: a background request expires after 30 minutes; a chat request
  ends on a new message in that chat; both values are read from
  `nucleus.toml`.
- Dashboard confirmation: a request to a covered route from a worker
  machine, from a session, or from a browser without the cookie is
  refused; after a passkey or TOTP confirmation the same browser's request
  passes; 15 minutes later (or after the changed `nucleus.toml` value) the
  cookie no longer passes and a new confirmation is asked; the cookie is
  HttpOnly and same-site; a route outside the covered list still works
  without a confirmation from a tailnet device.
- Interpreter: shared vectors for plain-word answers bound to a request
  number; a request not in the list is refused; voice notes, forwards and
  an inferred request with several waiting get a confirmation question.
- Real session (opt-in, `nucleus-test-*` tmux session): a session runs an
  in-scope action, is refused, ends its turn with "waiting for approval
  #N", receives the typed approval message, runs the action once and is
  refused on a repeat. The same test runs in a bot session, because global
  hooks run in bot sessions too.
