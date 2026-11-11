# ADR-043 — Update generations, database snapshots and rollback

Date: 2026-11-11
Status: proposed

**Relates to:**
- [[ADR-030]] — amended. `tools/build.sh` still signs a copy and installs it
  with `mv` over `target/release/nucleus`, and the path stays fixed. What
  changes: the script keeps the last 3 signed builds as numbered
  generations instead of keeping only the installed file; it snapshots the
  databases whose schema the new build changes before the install; and every
  install starts a check window that can end in an automatic rollback.
  ADR-030's cleanup of `target/release/` stays; the generations are stored
  outside that directory.
- [[ADR-020]] — the versioned migrations (`core/src/migrate.rs`, the
  `schema_migrations` ledger) are how an install knows whether a build
  changes a database schema.
- [[ADR-039]] — the SQLite online backup API and `PRAGMA quick_check` used
  for the backup copies are reused for the pre-install snapshots.
- [[ADR-026]] — the heartbeat's service checks are one of the checks run
  inside the window.
- [[ADR-040]], [[ADR-036]] — approval grants, requests and confirmations
  are invalidated by a restore of snapshots (§4).
- [[ADR-016]] — the services that run the binary are the launchd jobs of
  the agent registry; they restart after an install and after a rollback.

## Context

The chores, the Discord bot and the dashboard run the one Rust `nucleus`
binary that ADR-030 consolidated. The WhatsApp bot does not: it is
TypeScript, started through npm (`tools/launchd/whatsapp.plist.example`
runs `npm start`, which `messaging/whatsapp/package.json` defines as
`tsx src/index.ts`), and it is deployed separately from the binary.
`tools/build.sh` builds with the `install-build` profile, signs a copy and
moves it over `target/release/nucleus`. Nothing keeps the build that was
installed before. A bad build therefore stops every chore until a fix is
built and installed. The path cannot change between builds, because the
Full Disk Access grant is tied to it.

The monthly feature radar of October 2026 reported that an upstream
assistant stack keeps sealed install generations, and a second surveyed
product does the same. That product also states the limit that applies to
any rollback with a database: a database that has run a release does not go
back without a snapshot taken before that release.

## Decision

### 1. Generations

Generations cover the Rust `nucleus` binary of ADR-030 and the processes
that run it. The WhatsApp bot is outside them: an install or a rollback of
a generation does not change the bot's code, and a restore of §2 does not
restore the database the bot owns (`memory/whatsapp.db`). Rollback of the
bot is the operator reverting its source and restarting its launchd job,
as today. How a schema change of the bot's database is detected stays an
open question (footnote 3).

- `tools/build.sh` keeps the last 3 signed builds as numbered generations.
  A generation is the signed, verified binary plus a record of its version
  (`nucleus --version`), the build time and the migration versions it
  contains.
- Generations live outside `target/release/`, so ADR-030's cleanup of that
  directory does not remove them.
- Install is a move of one generation into the fixed path: the script
  copies the generation to a temporary file in `target/release/` and moves
  that file over `target/release/nucleus`. The installed file is never
  changed in place (ADR-030).
- Rollback is the same move with the previous generation.
- After an install or a rollback, the launchd jobs that run the binary are
  restarted. Running processes of the old file keep the file they have open
  until then.
- When a fourth generation is added, the oldest one is removed, together
  with its database snapshots.

### 2. Database snapshots before a schema change

- Before an install, the script compares the migration list of the new
  build with the `schema_migrations` ledger of each database. A build
  changes a database schema when its migration list holds a version that
  the database's ledger does not.
- When a build changes one or more schemas, those databases are copied
  before the install with SQLite's online backup API, and each copy must
  pass `PRAGMA quick_check` (the method of ADR-039). The snapshots are
  stored with the generation that was installed before.
- A failed snapshot stops the install. The installed binary stays
  unchanged.
- A build that changes no schema takes no snapshot.

### 3. The check window

After every install a check window starts. Its length is a `nucleus.toml`
setting; the default is **30 minutes**.

**Who runs the window.** The install step starts a separate job, the
window supervisor, from the previous (known-good) generation, not from the
candidate that was just installed. The supervisor owns the window's timer
and check state, runs the checks against the candidate, and performs the
restore. A candidate that cannot start, or that stops every chore, cannot
stop its own rollback, because no part of the rollback runs from the
candidate's code. The supervisor reads the operator's "keep it" and "roll
back now" answers and the install of a fix (point 2 below) from the
window's state, which it owns. The operator chose a window over an
immediate rollback because a failure can be a small problem that a quick
fix solves, and an immediate rollback would discard the whole version for
it.

Checks run inside the window:

- `nucleus --version`;
- a self-test of the installed binary;
- the heartbeat's service checks (ADR-026).

Behavior:

1. A failed check sends an alert at once. It does not roll back at once.
2. A fix installed inside the window cancels the pending rollback. The fix
   is a new install and starts its own window.
3. If the checks still fail at the end of the window, Nucleus rolls back
   automatically:
   - when the failed build changed no schema, it restores the binary only.
     No data is lost.
   - when the failed build changed a schema, it restores the binary and the
     snapshots of §2. The alert lists the writes lost: the writes made to
     those databases between the snapshot and the rollback.
4. The operator can answer the alert with "keep it", which cancels the
   rollback and keeps the new build installed, or "roll back now", which
   rolls back without waiting for the end of the window.

### 4. Authorization after a restore

A restore of database snapshots (§3 point 3, §5) starts a new
authorization epoch:

- Every outstanding approval grant and confirmation is invalidated at the
  restore: the ADR-040 approval grants (including delete grants of
  ADR-041), the ADR-036 confirmations and the ADR-040 requests that were
  waiting. The operator is asked again for any action that still needs
  approval.
- A consumed grant can never be restored. A grant consumed after the
  snapshot is not usable again, even when the restored rows show it
  unconsumed, because every grant from an earlier epoch is refused.
- Actions completed outside Nucleus between the snapshot and the restore
  (a sent email, a push, a delete, a WhatsApp message, a calendar invite)
  are not undone by the restore. Before queued work resumes, Nucleus
  reconciles it against those actions: a queued item whose action already
  happened is marked done, not run again.
- When the ADR-040 request store is one of the snapshotted databases, its
  restore is covered by this rule explicitly; the store's location is still
  open (ADR-040 footnote 4).

### 5. Rollback after the window

`nucleus rollback` installs the previous generation.

- When the current build changed no schema, it restores the binary only.
- When the current build changed a schema, it needs the operator's approval
  and first shows what would be lost: the writes made to the affected
  databases since the snapshot.

### 6. Configuration

- `nucleus.toml`: the check window length (default 30 minutes).
- The number of generations kept is 3.

## Consequences

- A bad build of the Rust binary stops the chores for at most the check
  window, or until the operator answers "roll back now". This holds because
  the window supervisor runs from the previous generation (§3); it does
  not hold when the previous generation cannot start either.
- A WhatsApp bot change is not covered: a bad bot change is rolled back by
  reverting its source, outside the generations.
- After a restore of snapshots, every approval and confirmation that was
  outstanding must be given again (§4).
- An automatic rollback after a schema change loses the writes made since
  the snapshot. The alert names them, so the operator knows what to redo.
- Disk use grows by up to two more binaries and the snapshots of the
  databases that the last builds changed.
- A rollback restores only the databases whose schema the failed build
  changed. Databases it did not change keep their writes.
- The window adds a waiting period after every install during which the
  checks run and an alert can arrive.

## Rejected alternatives

- **Immediate rollback on the first failed check.** Rejected by the
  operator: the failure can be a small problem that a fix inside the
  window solves, and an immediate rollback discards the whole version for
  it.
- **Rollback of the binary alone after a schema change.** A database that
  has run the new release does not go back; the old binary would run
  against a schema it does not know. The snapshot before the install is
  required.
- **A different install path per generation.** The Full Disk Access grant is
  tied to the fixed path (ADR-030), so every generation is installed at
  that path.

## Open questions / footnotes

1. The directory that holds the generations: it must survive ADR-030's
   cleanup of `target/release/` and `cargo clean`.
2. What the self-test runs. It must not write to the operator's databases
   or send messages.
3. The databases that `core/src/migrate.rs` does not migrate:
   `memory/whatsapp.db` is owned by the TypeScript WhatsApp bot (ADR-020),
   which is deployed outside the generations (§1). The method that detects
   a schema change for it, and whether its database gets snapshots before
   a bot change, are not decided.
4. The form of the list of lost writes (per database, per table, row
   counts).
5. Where the operator's "keep it" and "roll back now" answers are read
   (WhatsApp DM, dashboard, terminal command) and which mechanism records
   the approval in §5.
6. A fix installed inside the window that also fails: which generation the
   automatic rollback at the end of the fix's window restores, and which
   snapshots apply when both builds changed schemas.
7. Origin: the October 2026 feature radar; the operator's reason for the
   window is quoted in §3.

## Verification

- **Generations:** three consecutive builds leave three numbered
  generations; a fourth removes the oldest and its snapshots;
  `target/release/` holds only the installed binary after each install.
- **Install path:** after install and after rollback, `codesign --verify
  --strict` passes on `target/release/nucleus`, the file was replaced by a
  rename (new inode), and a vault read from a launchd job succeeds (the Full
  Disk Access grant still applies).
- **Schema detection:** a test build with one added migration for one
  database snapshots that database only; a build with no added migration
  takes no snapshot.
- **Snapshot failure:** a snapshot that fails `quick_check` stops the
  install and leaves the installed binary unchanged.
- **Window, no schema change:** a build whose self-test fails sends one
  alert at once, rolls back the binary at the end of the window, and loses
  no data.
- **Window, schema change:** a failing build with a migration, plus writes
  during the window, rolls back the binary and the snapshot; the alert
  lists those writes.
- **Fix inside the window:** a failing build followed by a passing build
  inside the window does not roll back.
- **Operator answers:** "keep it" cancels the rollback; "roll back now"
  rolls back before the window ends.
- **Rollback after the window:** `nucleus rollback` without a schema change
  restores the binary only; with a schema change it shows the writes that
  would be lost and does nothing without approval.
- **Supervisor:** a candidate build that exits at once on start, and one
  that hangs, are rolled back at the end of the window by the supervisor
  running from the previous generation; the supervisor's process runs the
  previous generation's file, not the candidate's.
- **Authorization epoch:** a snapshot taken while an approval grant is
  unconsumed, the grant consumed after the snapshot, then a restore: the
  grant is refused after the restore, and so is every grant and
  confirmation outstanding at the restore. A queued item whose action was
  completed between the snapshot and the restore is not run again.
- **Setting:** a changed window length in `nucleus.toml` changes when the
  automatic rollback runs.
