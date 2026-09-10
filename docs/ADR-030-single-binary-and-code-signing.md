# ADR-030 — One signed binary, one permanent permission grant

**Status:** Accepted (2026-09-09) — Implemented (2026-09-09)

**Builds on:**
- [[ADR-016]] — the agent registry and the per-service launchd jobs this
  consolidates behind one executable.
- [[ADR-020]] — the hard-cut policy applied here to the per-crate binaries.

## Context

On 2026-09-09 the distiller hung for five hours and thirteen minutes, from
04:00:28 to 09:13:32. The machine never slept and the code was not at fault.
macOS asked the `distiller` binary for permission to read `~/Documents`, the
dialog sat unanswered while the operator slept, and `summarize_vault`'s
`read_dir` blocked inside the syscall until he granted it. The privacy database
records the grant at 09:13:27; the distiller's next transcript entry is
09:13:32, five seconds later.

The trigger was an ordinary rebuild the previous evening. Cargo produces
**ad-hoc, linker-signed** binaries — `codesign -dvvv` reported
`Signature=adhoc`, `TeamIdentifier=not set` — and macOS records a privacy grant
as a requirement pinned to that binary's code hash. Every `cargo build
--release` changes the hash, the stored requirement stops matching, and the next
unattended run is treated as a stranger asking for the first time.

Three of the nine binaries read the vault (`chores/distiller`,
`chores/reminders`, `nucleus-dashboard/api`), so the prompt could resurface on a
different service after nearly any rebuild. Granting permission by hand does not
help, because the grant dies with the next build.

A deadline around the vault read was considered and rejected. It converts a
five-hour hang into a degraded run, which is an improvement, but it treats the
symptom: the job still cannot read the vault, and the prompt still returns after
every rebuild.

## Decision

Two changes, which only work together.

### One binary

`target/release/nucleus` is the only executable in the workspace. It dispatches
to nine subcommands; each service crate is a library exposing
`run(args: Vec<OsString>)`.

| Subcommand | Crate |
|---|---|
| `distiller` | `chores/distiller` |
| `reminders` | `chores/reminders` |
| `skill-gap-learner` | `chores/skill-gap-learner` |
| `discord` | `messaging/discord` |
| `gmail-metabolism` | `messaging/gmail` |
| `news-fetcher` | `news/fetcher` |
| `dashboard` | `nucleus-dashboard/api` |
| `session-search` | `nucleus_core::cmd::session_search` |
| `session-send` | `nucleus_core::cmd::session_send` |

This was already the repo's pattern in miniature: `chores/reminders` shipped a
library so the dashboard could import `reminders::store`.

One binary means one signature to maintain and one grant to give. It also
removes a class of silent failure: the skill-gate alert used to be dropped
whenever `target/release/reminders` happened not to exist, and is now a library
call that cannot go missing.

The dispatcher rejects arguments for the four services that accept none.
Without that guard `nucleus distiller --help` **ran a full distillation pass**,
because the crate ignores argv.

### A stable signing identity

`tools/codesign/create-identity.sh` creates a self-signed code-signing
certificate ("Nucleus Code Signing") in the login keychain.
`tools/build.sh` builds and then signs with it under the fixed identifier
`dev.nucleus`. Because the stored requirement keys on the certificate and the
identifier rather than on file contents, the grant survives rebuilds.

`tools/build.sh` replaces `cargo build --release` as the way to build.

## Consequences

- One manual grant: Full Disk Access to `target/release/nucleus`, in System
  Settings. It is only repeated if the certificate is replaced.
- **A build that skips signing loses the grant.** Access is then denied rather
  than prompted, so services fail with a permission error instead of hanging.
  `tools/healthcheck.sh` checks the signature, and `summarize_vault` now logs a
  failed listing instead of silently returning an empty vault.
- The identity reports `CSSMERR_TP_NOT_TRUSTED` and does not appear under
  `security find-identity -v`. That is correct and harmless: nothing verifies
  this certificate against a trust chain, and the grant matches on the leaf
  hash. Both scripts therefore query without `-v` — checking with `-v` made
  `create-identity.sh` mint a duplicate certificate on every run, which then
  broke signing with "ambiguous (matches ... and ...)".
- The first build after creating the identity shows a keychain dialog. Clicking
  **Always Allow** once is required; plain Allow makes it return on every build.
  Setting that from a script needs the login keychain password, so it stays
  manual. If builds ever need to be unattended, move the identity to a dedicated
  keychain whose password lives in `.env` and unlock it in `tools/build.sh`.
- The certificate is machine-local. Another machine building Nucleus needs its
  own identity and its own grant.
- Services still run as separate processes under separate launchd jobs. Only the
  file on disk is shared.

## Rejected

- **A timeout around vault access.** Treats the symptom; the prompt still
  returns after every rebuild and the job still cannot read the vault.
- **Granting each binary separately.** Nine grants that all die at the next
  build.
- **Moving the vault out of `~/Documents`.** This would end the problem for
  every binary with no signing at all, since the protection only covers Desktop,
  Documents and Downloads. Rejected because it relocates the operator's vault
  and every path that references it, to work around a macOS behaviour rather
  than to satisfy it.
