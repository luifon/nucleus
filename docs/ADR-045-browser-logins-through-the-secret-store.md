# ADR-045 — Browser logins through the self-hosted secret store

Date: 2026-11-11
Status: proposed

**Relates to:**
- [[ADR-028]] — amended. What stays: the threat model; the personal account
  that only the operator's own Bitwarden clients open (§1); the broker as a
  separate process with a peer-credential check and a static per-consumer
  allowlist (§2); the vendor choice (§5); the deployment requirements (§6);
  Gate −1 as the first step; rotation as an acceptance criterion. What this
  ADR narrows or replaces:
  - §1, the single operational vault, is replaced by **one operational
    account per machine**, each seeing only its own collection inside one
    Vaultwarden organization (§2 below).
  - §2, the broker, is narrowed: one broker per machine; it holds the
    unlocked key only in memory, caches nothing and writes nothing to disk.
    It has two verbs. Sessions get only the login verb, which logs in
    inside the broker's own browser context and hands over only the
    resulting session state; the verb that returns a value serves only
    registered code paths and refuses callers whose process ancestry
    includes a session (§3 below).
  - §3, `.env` references resolved at load, stays only for registered code
    paths through the value verb.
  - §4, the `bw` local ciphertext cache owned by the broker, is removed.
    No machine keeps a copy, cache or file of the credentials that
    Nucleus's agents and automation use (§1 below states the exceptions).
  - Gate 0's requirements "broker auto-unlock across a cold boot" and "a
    local ciphertext cache so a vault outage degrades rather than bricks
    startup" are replaced: the broker starts locked after a restart, the
    operator unlocks it once, and it refuses every request while
    Vaultwarden is unreachable. This answers ADR-028 open question 1: there
    is no unattended unlock and no master password on disk.
  - Gate −1's disposition is narrowed: the operator marks each credential
    found as important or unimportant (§5 below).
  - Open question 2 (which host) is answered: Vaultwarden runs on the
    Vaultwarden host that already serves it; every machine reaches it over
    the tailnet.
- [[ADR-022]] — the isolated Playwright browsers and the shared storage
  state stay. The broker logs in inside its own browser context and hands
  the session's browser only the resulting storage state (cookies and
  storage), the same mechanism ADR-022 uses for a manual login.
  `tools/playwright-auth login` stays the manual path when the operator
  logs in himself.
- [[ADR-023]] — the session index is one of the stores the cleanup scans
  and purges.
- [[ADR-035]] — the credential exclusions of vault search apply to the
  Obsidian credential notes that stay in the vault.
- [[ADR-015]] — amended through ADR-040 §6 for two dashboard routes: the
  broker unlock and the input of the live login view need a fresh passkey
  or TOTP confirmation.
- [[ADR-033]] — browser tasks run as tasks; a login step that needs the
  operator is reported to the task's origin.
- [[ADR-040]] — a login step follows the same routing rules as an approval
  request: the working session's chat first, WhatsApp after 10 minutes.
- [[ADR-046]] — every login step that needs the operator, and every locked
  broker with a waiting consumer, is listed in the "what needs you" view.
- [[ADR-047]] — each node has its own broker and its own operational
  account.

## Context

Expired browser logins are the most frequent reason a browser task stops,
above all for the job-application tasks. Today the operator logs in again
by hand with `tools/playwright-auth login` (ADR-022) each time a session
expires.

ADR-028 proposed a broker for the self-hosted secret store; it is not
built. Bitwarden clients decrypt on the machine. Without a broker a session
would run `bw` itself and the decrypted passwords would enter its context,
where an injected instruction can read them.

One of the surveyed assistant products has a vault with a secure form: its
code, not the model, fetches the site's login and types it into the page,
and the password never enters the model's context. This ADR adopts the
part of that model where code, not the model, handles the password. It
does not type into a page the session can script: a session that scripts
a browser can read a filled field's value, add an input listener, or
change the page before the login is typed, so the broker logs in inside a
browser context of its own (§3).

## Decision

### 1. Vaultwarden is the only store for agent and automation credentials

Vaultwarden is the only store for the credentials that Nucleus's agents
and automation use: browser logins and the secrets that registered code
paths resolve through the value verb.

- No machine keeps a copy, cache or file of those credentials.
- Two exceptions are outside this rule:
  - **The restic repository password** stays where ADR-039 keeps it: the
    Mac's login Keychain and a root-only file on the homelab, plus the
    operator's copy outside both machines. It is recovery material for the
    backups, and a restore may be needed when the secret store itself is
    unavailable or lost, so it must not depend on that store.
  - **Unimportant credentials** the operator chooses to leave in their
    Obsidian notes (§5).
- Credentials that tools store in plain text on worker machines are
  migration candidates for the broker.
- When Vaultwarden is down, automated logins fail and the operator logs in
  himself.
- The personal account of ADR-028 §1 is unchanged: no Nucleus code path
  opens it.

### 2. One operational account per machine

- Every machine that runs browser tasks has its own operational account.
- The accounts are members of one Vaultwarden organization. Each account
  sees only its own machine's collection.
- A login is stored once in the organization and shared to the collections
  of the machines that need it.
- A compromised machine exposes only the logins shared to its collection.
- The job-application logins move to the operational account (approved by
  the operator). They are shared to the Mac's collection, unless those
  browser tasks later run on the homelab.

### 3. The broker, one per machine

A small process on each machine:

- unlocks with that machine's operational account;
- holds the key **only in memory**;
- writes nothing to disk and caches nothing;
- refuses every request while Vaultwarden is unreachable;
- starts **locked** after a restart; the operator unlocks it once.

It has two verbs:

- **Log in.** When a browser task needs a login, the caller names a
  credential reference. The broker fetches the login and performs the
  login in a browser context of its own that the session cannot script:
  its own browser process, or its own CDP connection, owned by the
  broker's OS user. It then forgets the value and hands only the resulting
  authenticated session state (cookies and storage) to the session's
  browser, through the storage-state mechanism of ADR-022. The password is
  never typed into a page the session can script, and the value is never
  returned to the caller. This is the only verb a Claude session can use.
- **Return a value.** Serves only registered code paths (for example the
  backup job's executable, ADR-039 amendment), identified as in ADR-028 §2
  by peer credentials and the per-consumer allowlist. It refuses every
  caller whose process ancestry includes a Claude session. A process that
  runs as the same OS user and impersonates a registered executable is not
  stopped by that check; this is the residual recorded in ADR-028 §2 (see
  Consequences).

**Where a login may go.** Each credential reference is bound, in the
broker's allowlist, to its permitted HTTPS origin or origins and its login
URL. The caller names only the reference; it cannot choose the destination.
The broker navigates to the login URL itself. At the moment it enters the
login, and again after every redirect, it checks that the page's origin
and the frame that receives the input are in the reference's permitted
origins. It refuses any other destination and reports a failed login.
The session state it hands over is limited to the permitted origins.

### 4. Locked broker

After a machine restart every consumer that needs a secret fails until the
operator unlocks the broker, the backup job included. A locked broker with
a waiting consumer raises an item in the "what needs you" view (ADR-046)
and sends a WhatsApp alert, so the wait is visible. The operator chose the
locked start; this is an accepted trade-off. An unlock from the dashboard
requires the passkey or TOTP confirmation of ADR-040 §6.

### 5. Cleanup first (ADR-028 Gate −1)

Before any login moves:

1. Scan the transcripts, the session search index and the vault notes for
   credentials.
2. The operator marks each credential found:
   - **important**: moved into Vaultwarden and rotated. It goes into the
     personal account, or into the operational organization's collection
     only when a browser task needs it.
   - **unimportant**: may stay in its Obsidian note. Some accounts there do
     not matter.
3. Either way, the credential is removed from the transcripts and the
   search index.
4. Obsidian credential notes stay out of vault sync (ADR-047) and out of
   vault search (ADR-035).

### 6. Login steps that need the operator

Some logins need the operator: 2FA, a CAPTCHA, "approve on phone".

- The dashboard shows a live view of that browser page (Chrome
  screencast); during a login through the broker, that is the page in the
  broker's own browser context. The operator clicks and types in it. The
  live view's input route requires the passkey or TOTP confirmation of
  ADR-040 §6 (one confirmation unlocks that browser for 15 minutes). The
  task continues when the page moves past the login.
- When the task comes from a session he is working in, the request goes to
  that session's chat instead of WhatsApp or the dashboard. It goes to
  WhatsApp after 10 minutes without an answer.
- Every request also appears in the "what needs you" view (ADR-046).
- Device-bound passkeys cannot work in a remote browser. The fallback is a
  visible browser window on the machine that runs the task.

## Consequences

- The password of a site never enters a session's context or a page the
  session can script. An injected session can request a login for an
  allowed reference; the login happens in the broker's own browser
  context, at the reference's own origin, and the session receives only
  the resulting session state.
- The session state handed over still lets the session act on the site
  as the operator (see below); the binding limits which sites, not what is
  done there.
- A login stored once can serve several machines; revoking a machine is a
  change to its collection.
- An outage of the Vaultwarden host stops every automated login and every
  value consumer. There is no cache to fall back to.
- The broker unlock and the live login view on the dashboard need a
  passkey or TOTP confirmation at most every 15 minutes per browser
  (ADR-040 §6).
- After every machine restart, consumers wait until the operator unlocks
  the broker. The "what needs you" item and the WhatsApp alert report the
  wait (§4).
- The residual of ADR-028 §2 stays for the value verb: the ancestry check
  refuses callers started by a session, and injected code at the same OS
  user that runs or impersonates a registered executable resolves that
  consumer's references.
- Once logged in, a session can act on the site as the operator for the
  life of the session. This ADR protects the password, not the actions
  taken after login; approvals for those actions are ADR-040.

## Rejected alternatives

- **Sessions run `bw` themselves.** Bitwarden clients decrypt on the
  machine, so the passwords would enter the session's context.
- **A credential cache on each machine** (ADR-028 §4 and Gate 0). Rejected
  by the operator: no machine keeps a copy, cache or file of credentials.
- **Automatic unlock at boot** (ADR-028 Gate 0). Rejected by the operator,
  who chose the locked start; it needs a reusable unlock secret on the
  machine.
- **One operational account shared by every machine.** A compromised
  machine would expose every operational login.
- **The value verb for sessions.** A session could read the value it
  receives.
- **Typing the login into the session's browser page.** The session
  scripts that browser: it can read the filled field, add an input
  listener, or prepare the page before the broker types, and a page the
  session navigated can send the typed value over its own network.
- **A destination chosen by the caller.** An injected session could direct
  an allowed credential to a page or frame it controls.
- **The restic password in the secret store.** The backups must be
  restorable when the secret store is down or lost (ADR-039).

## Open questions / footnotes

1. The machine that runs the job-application browser tasks: the Mac now;
   the logins are shared to the homelab's collection if those tasks move
   there.
2. How the operator unlocks a broker (dashboard, terminal on the machine)
   is not decided. On the dashboard, the unlock needs the confirmation of
   ADR-040 §6.
3. Origin: this design follows the operator's earlier decision that an
   agent must not hold reusable passwords, and the persistent authenticated
   sessions of ADR-022.

## Verification

- **No value in context:** a browser task logs in to a test site through
  the login verb; the session's transcript, the session index and the tool
  results contain no part of the password, and the session's browser
  receives only cookies and storage for the test site's origin.
- **Hostile page scripts:** a test session that runs DOM evaluation,
  installs input and keyboard listeners and adds page instrumentation in
  its own browser before, during and after the login, and tries to attach
  to the broker's browser process or CDP endpoint, gets no part of the
  password and cannot attach.
- **Destination binding:** a login request for a reference whose login URL
  redirects to an origin outside its permitted origins, a login form inside
  a frame from another origin, and a caller that names a different URL are
  each refused with a failed login and no value entered.
- **Value verb refusal:** a Claude session, and a process whose ancestry
  includes a session, calling the value verb are refused; the registered
  backup executable gets its value. The same-OS-user impersonation residual
  is recorded, not tested as refused.
- **Collections:** a machine's account cannot fetch a login that is not
  shared to its collection.
- **Nothing on disk:** after a fetch, no file written by the broker holds
  the value or ciphertext of it; the broker's memory is not dumped on crash
  (`RLIMIT_CORE=0`).
- **Outage:** with Vaultwarden unreachable, every request is refused and the
  task reports a failed login.
- **Dashboard confirmation:** a broker unlock or a live-view input from a
  worker machine, a session, or a browser without the ADR-040 §6 cookie is
  refused; after a passkey or TOTP confirmation it passes; after 15
  minutes it is refused again.
- **Locked start:** after a restart the broker refuses until unlocked; a
  waiting consumer creates one "what needs you" item and one WhatsApp
  alert.
- **Cleanup:** a scan of a fixture transcript, session index and vault note
  with synthetic credentials finds each one; after the operator's marking,
  none remains in transcripts or the index, and an unimportant one remains
  only in its note, absent from vault search.
- **Operator step:** a test page that requires a second step shows in the
  dashboard's live view; input there moves the page past the login and the
  task continues; a request from a working session reaches its chat and
  reaches WhatsApp after 10 minutes without an answer.
- **Passkey fallback:** a site that requires a device-bound passkey opens a
  visible window on the machine that runs the task.
