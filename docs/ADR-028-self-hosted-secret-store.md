# ADR-028 — Self-hosted secret store: a broker Nucleus can ask, a vault it cannot open

Date: 2026-08-22
Status: proposed (revised after adversarial review)

## Context

Nucleus has no secret store. Live credentials sit in `.env` as plaintext,
and the operator's own credentials have accumulated in Obsidian vault
notes — also plaintext — because the bot was asked to remember them and
had nowhere better to put them.

The vault already carries a decision (`Ideas/credential-handling.md`)
that the agent should not hold reusable passwords: anything inside an
agent's read scope is readable by an injected instruction. That decision
is aspirational — nothing enforces it, and it has been overridden case by
case. Enforcement is also *inconsistent* rather than absent: the harness
classifier blocks some credential writes and permits others, so the
operator's fallback is leaving the secret in the chat thread, the least
protected place available.

Current blast radius — three stores, none designed for secrets:

1. **Messaging transcripts** — the value as pasted.
2. **The session index** (ADR-023) — full-text searchable, callable by
   any session. A plain FTS query returns credentials today.
3. **Vault notes** — durable, and synced if vault sync is ever enabled.

Upstream (hermes v0.19) resolves secrets from a password manager at load
instead of reading plaintext config. The shape is right; the vendor choice
is ours, and the operator's constraint is explicit: self-hosted, no
third-party custody.

### Threat model

Distinct attackers, because they have distinct reach:

| # | Attacker | Reach today |
|---|---|---|
| T1 | **Prompt injection** into an unattended session (malicious page, email, injected `agent-msg`) | Everything the workspace user can do. Does **not** require host compromise. |
| T2 | **Stolen messaging account** (Discord/WhatsApp token) | Same as T1 via ADR-003 — broad shell as the workspace user. |
| T3 | **Same-user local code execution** | Reads `.env`, process memory, caches, sockets. |
| T4 | **Full host compromise** | Everything, including anything unlocked. |
| T5 | **Network / tailnet peer** — a compromised or mistakenly enrolled device | Reaches any service on the tailnet. |

T1 and T2 are the attackers this ADR exists to stop, and the ones the
previous draft under-modelled by comparing only against T4. Integrity and
availability attacks (deleting vault state, corrupting backups to force
lockout) are **in scope**; see Consequences.

## Decision

### 1. Two vaults, and Nucleus only ever gets one of them

The load-bearing decision is *subtraction*: **personal credentials never
enter Nucleus at all.**

| Vault | Holds | Who opens it |
|---|---|---|
| **Operational** | Only credentials bots need to run unattended | The resolver daemon, automatically |
| **Personal** | The operator's own credentials | **Only the operator's own Bitwarden client** (phone/desktop). No Nucleus code path exists. |

The personal vault is a separate Vaultwarden **account** — therefore a
separate encryption key, not a collection or a naming convention. Nucleus
holds no credential for it, no session token, no unlock path, and exposes
no retrieval command. The previous draft's "deliver a personal secret to
the operator's DM" feature is **removed**: under ADR-021 an injected
message can request a DM, so that path was an exfiltration channel. The
operator opens his own vault on his own device.

This makes the separation structural rather than procedural: T1–T3 cannot
reach personal secrets because no code that they can reach knows how.

**With one honest caveat about timing.** That property holds for values
stored in the personal vault. It does *not* retroactively cover the
personal credentials already sitting in transcripts, the FTS index, and
vault notes — a new account neither removes nor revokes those. Until
Gate −1 closes, "out of Nucleus's reach" is a statement about the target
state, not the current one. This is why remediation is the first gate
rather than adjacent cleanup.

### 2. Operational access is a brokered socket, not a CLI the bots run

A resolver daemon runs as a **dedicated OS user** (`_nucleus-secrets`),
distinct from the workspace user the bots run as.

- It holds the operational vault's session key in its own memory. The
  workspace user cannot read that process, its env, or its files.
- Bots reach it over a unix socket, authorized by **peer credentials**
  (`SO_PEERCRED`/`LOCAL_PEERCRED`), not by a shared token an injected
  session could steal.
- It answers only from a **static allowlist** of references, in a
  root-owned config the workspace user cannot edit. There is no "fetch
  arbitrary item" verb, no list verb, no export verb.
- The allowlist is **scoped per consumer, not global.** Each entry binds a
  reference to the specific service allowed to resolve it, keyed on the
  caller's executable path and launchd service label (verified from the
  peer pid, re-checked after read to avoid pid reuse). The WhatsApp bot
  cannot resolve the Gmail token; a session that is not a registered
  consumer resolves nothing at all.
- Every resolution is audit-logged with the requesting pid, resolved
  executable, and reference.

**What T1/T2 still get, stated plainly.** `SO_PEERCRED` proves only that
the caller runs as the workspace user — it cannot distinguish a real bot
from injected code at the same UID. Injected code that execs from a
registered consumer's path, or injects into a live consumer process,
therefore resolves that consumer's references. The boundary is real but
partial, and the honest claim is narrower than "unattended sessions can't
reach operational secrets":

- **Bounded per resolution**, not per incident. A single injected session
  gets one consumer's slice; an attacker with *sustained* same-user
  execution can work through each registered path in turn and accumulate
  the union of all consumers' references. The allowlist raises cost and
  narrows a smash-and-grab — it does not cap a patient attacker. Registered
  executables must therefore be root-owned and non-writable by the
  workspace user, or the binding is decorative.
- **Never** the session key, unlisted items, vault enumeration, the
  ability to add references, or anything in the personal vault.
- Still **strictly better than `.env` today**, which yields every value at
  once, is editable, and is readable offline.

Closing the remaining gap requires per-consumer OS users — worth doing,
but it is a larger change to how bots are spawned and is deliberately not
in this ADR's scope. Recorded here as the known ceiling of this design
rather than left implied.

`bw` is therefore **not** the bots' interface. It is an implementation
detail inside the daemon, which is what finding 3.6 asks for: the
password-manager CLI is not being used as a machine-secret broker; a
purpose-built broker wraps it.

### 3. `.env` holds references

A secret-bearing key becomes `bw://<vault-id>/<item>/<field>`, where
`<vault-id>` is the immutable operational account identifier — so a
resolver logged into the wrong vault fails closed rather than silently
resolving a same-named item. `nucleus_core::config` resolves references
through the broker at load into process memory. Templates ship references,
which are safe to commit — Rule 3 stops conflicting with Rule 1.

### 4. Where plaintext actually lives

Bitwarden clients encrypt and decrypt locally; the server stores
ciphertext. So the claim is **not** "nothing on disk holds the value".
Precisely:

| Component | Holds | Mitigation |
|---|---|---|
| Vaultwarden DB + backups | Ciphertext | Encrypted at rest; backup keys offline (§6) |
| Broker process memory | **Plaintext** | Dedicated OS user; no core dumps (`RLIMIT_CORE=0`) |
| `bw` local cache (broker-owned) | Ciphertext | Mode 0600, `_nucleus-secrets` only |
| Bot process memory | **Plaintext**, only allowlisted values | Bounded by the allowlist |
| Logs / shell output / swap | Must hold **nothing** | Broker never logs values; resolved values never passed as argv |

Server-side compromise (T5) yields ciphertext. Broker compromise (T3/T4)
yields the operational tier only. Those are different incidents with
different responses.

### 5. Vendor choice

| Candidate | Rejected because |
|---|---|
| **Vaultwarden** | **Chosen** — self-hosted, low resource, Bitwarden-protocol so the operator's existing clients work for the personal vault |
| Official Bitwarden self-host | Same clients, but heavyweight for a home box; revisit if Vaultwarden compatibility lag bites |
| SOPS / age | Excellent for *config* secrets, no interactive client for the personal vault — would need a second tool |
| `pass` / GPG | GPG agent under launchd is the known-bad path; no phone client worth using |
| KeePassXC | File-based; sync story is manual and conflict-prone |
| OpenBao / Infisical | Correct shape for machine secrets, but heavy ops burden and still no personal-vault client |

Vaultwarden tracks a protocol Bitwarden controls, so: **pin the server
image by digest, pin the `bw` version, test the pair in staging before
upgrade, and keep the previous digest for rollback.** Patch SLA: security
releases applied within 7 days; the project has shipped fixes for account
enumeration, CSRF, and SSRF, so "actively maintained" is not a substitute
for a patch policy.

### 6. Deployment requirements (not deployment trivia)

Tailnet membership is **not** the only gate — ADR-011 leaves ACLs as
future work, so T5 is real:

- Registration **disabled**; invitations off; password hints off.
- Admin interface disabled (no admin token) except during maintenance.
- 2FA on both accounts. Rate limiting on the login surface.
- TLS terminated by the existing reverse proxy; no direct exposure.
- Container runs non-root, read-only rootfs, no host network.
- Icon fetching and SMTP **off** (SSRF and egress surface).
- Tailnet ACL restricting who can reach the port, once ADR-011 formalizes.

Recovery material — master passwords, 2FA recovery codes, backup
decryption keys — lives **offline and off-host** (printed / hardware
token). It is not reachable by the broker, by the workspace user, or by
anything inside the failed vault. Restore is tested per vault
independently, or the break-glass path is fiction.

## Migration

Ordered, one credential at a time, with explicit states. Nothing is
deleted before its replacement is proven, and nothing is considered done
before rotation.

**Gate −1 — remediate the existing leak first. Nothing else starts until
this closes.** The separate personal account protects *new* values only;
credentials already sitting in transcripts, the FTS index, and vault notes
stay reachable until they are removed and rotated. Running migration first
would leave "personal secrets are out of Nucleus's reach" false for the
whole rollout.

1. **Inventory** every credential ever pasted into a thread, a vault note,
   or a captured transcript, by scanning those stores for the known
   secret shapes and reviewing every hit. Completeness cannot be proven —
   an unrecorded paste is unknowable — so the gate's exit criterion is the
   weaker but checkable one: *every hit found by the scan is resolved.*
   Residual risk is rotation's job, which is why every inventoried
   credential is rotated regardless of where it was found.
2. **Stop the bots for the duration.** Gate −1 runs with automation
   halted and the operator present — not alongside live sessions. This is
   the only enforcement that actually holds: `session-search` is callable
   by any session and **re-indexes on every invocation**, so a running
   session could repopulate the index from unsanitized transcripts
   mid-clean. Revoking the binary's execute bit does not stop the
   workspace user restoring it, copying it, or reading the FTS database
   directly — with no session running, there is nothing to do any of that.
   Downtime is the cost, and it is acceptable for a one-time gate —
   **scheduled by the operator**, not chosen by whoever runs the
   migration. Gate −1 does not start until he names the window.
3. **Sanitize or quarantine transcripts** with a format-aware JSONL
   rewriter — resume, tail-parsing, and agent messaging all read those
   files, and a naive edit corrupts them.
4. **Purge the FTS database, then rebuild** from the sanitized
   transcripts. Rebuilding before step 3 simply reinserts the credential;
   ADR-023 makes transcripts the source of truth.
5. **Rotate every inventoried credential** and verify per the ledger
   below. Where sanitizing is unsafe, quarantine plus rotation is the
   fallback — rotation is what actually ends the exposure.
6. **Verify** with exact-value and fragment searches, then restore
   execute access.

**Gate 0 — prototype before any production reference moves.** Prove:
broker auto-unlock across a cold boot; behavior during a vault outage;
restore from backup. If a bot cannot start after a reboot with the vault
down, the design is not ready. The broker keeps a local ciphertext cache
so a vault outage degrades rather than bricks startup.

**Per credential:** `captured → referenced → resolved → consumer restarted
→ old copy purged → rotated → verified`. A per-secret log records the
state; a partial migration is therefore visible rather than ambiguous.

**Rotation is an acceptance criterion, and it is mechanically checked.**
Deleting old copies revokes nothing — anyone who already read one still
holds a working value. So each credential carries a ledger row, and
`verified` is set *by the verifier*, never by hand:

| Field | Meaning |
|---|---|
| `secret_id` | The reference it migrated to |
| `old_digest` / `new_digest` | Salted hashes — proves the value actually changed, without storing either |
| `authority` | The endpoint that decides validity, **and the named person accountable for that credential's rotation** — both fixed at inventory time, not chosen at test time |
| `probe_result` | The authority's response to the **old** value — must be an explicit auth rejection |
| `probe_at`, `probe_operator` | When, and by whom |

`rotated → verified` advances only on a recorded rejection from the named
authority. A transient failure (timeout, 5xx, DNS) is **not** a rejection
and leaves the row at `rotated`. A credential whose authority cannot be probed
programmatically is marked `manual-attest`, which requires the accountable
person to record what they did and what response they saw, and is listed
in the ADR's sign-off. `manual-attest` is a **weaker** state than
`verified`, never an equivalent one: the count of `manual-attest` rows is
reported at sign-off, and a migration that is mostly manual attestation
has not met the acceptance criterion.

The ledger is append-only and lives outside the workspace user's write
scope — a verification record an attacker can forge verifies nothing.

**Backup timing:** first restore-tested backup happens *after* import and
*before* any plaintext deletion. Because that backup then contains
pre-rotation values, rotation follows it — so every credential in any
earlier backup is already revoked.

**Rollback:** if the broker fails after old copies are gone, recovery is
restore-the-vault or issue replacement credentials — never re-materialize
plaintext `.env`. "One-way" describes the plaintext, not the ability to
recover.

## Consequences

- **One rotation point** for operational secrets; `.env` stops holding
  live values.
- **The personal tier is out of reach by construction, once Gate −1
  closes** — the strongest property here, and it comes from removing a
  feature rather than adding controls. Before that gate closes the older
  copies are still reachable; the property describes the target state, not
  the day this ADR is accepted.
- **Two vaults and a daemon to run.** More moving parts than `.env`,
  including a dedicated OS user, socket permissions, and a patch SLA.
- **Availability is a real risk.** Vault loss takes operational
  credentials with it; the ciphertext cache, tested restore, and offline
  recovery material are what keep that from being terminal.
- **Rule 5 applies** — launchd needs explicit `PATH`/env for the broker.
- **Not solved here:** ADR-011's missing tailnet ACLs, and the harness
  classifier's inconsistent behavior around credential writes.

## Open questions

1. **Can the broker unlock non-interactively at boot without a reusable
   master password on disk?** If the only mechanism is a stored master
   password, the operational vault's protection against T3/T4 reduces to
   filesystem permissions — acceptable, but it must be *stated* rather
   than assumed. Resolve in Gate 0.
2. **Which host** — the current home server or the planned mini-PC. If the
   latter, this waits rather than being built twice.

## References

- ADR-003 — permissions; why an injected session has broad shell reach
- ADR-011 — tailnet perimeter, and its unfinished ACL work
- ADR-021 — agent session messaging; why injected input must not trigger
  secret delivery
- ADR-023 — session search; transcripts are the source of truth
- `docs/SECRETS.md` — env routing policy this supersedes for secrets
- CLAUDE.md Rule 3 (templates ship), Rule 5 (launchd env)
