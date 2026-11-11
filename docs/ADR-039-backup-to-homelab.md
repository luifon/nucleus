# ADR-039 — Daily encrypted backup to the homelab

**Status:** Accepted (2026-11-03) — Implemented (2026-11-03)

**Builds on:**
- [[ADR-016]] — the agent registry; the backup is a `maintenance` agent.
- [[ADR-020]] — DB ownership; launchd templates and the install script.
- [[ADR-026]] — the heartbeat, which detects a backup that stopped running.
- [[ADR-029]] — chore state; the backup's watermarks.
- [[ADR-035]] — vault-check, whose schedule gate this job copies.

## Context

Everything Nucleus needs to be rebuilt existed only on the Mac: the SQLite
databases in `memory/` (among them `session_index.db`, the only remaining
text of deleted transcripts), the private skills tree, `.env`, the WhatsApp
pairing state, the Claude Code transcripts and Tier-2 memory, and the
Obsidian vault. The repository holds the code, not this state. A lost or
broken disk meant losing all of it.

The operator has a homelab on the same network with an HDD for bulk data.
The homelab already backs itself up with restic to a repository on that HDD.

## Decision

A `nucleus backup` command and a `backup` launchd job. restic runs on the
Mac and writes to an `sftp:` repository on the homelab's HDD. The data is
encrypted on the Mac; the homelab stores only ciphertext.

### What is backed up

`[backup] paths` in `nucleus.toml` (the default list is in
`nucleus.toml.example`). Entries are workspace-relative, `~/`-relative, or
one of the placeholders `{claude_project}` (this workspace's directory under
`~/.claude/projects/`), `{vault}` (`[obsidian] vault_path`) and
`{launchd_prefix}`. A wildcard is allowed in the last component only. The
default list:

- workspace: `memory/`, `.nucleus/`, `.env`, `nucleus.toml`,
  `.claude/settings.local.json`,
  `messaging/whatsapp/auth/`, the gitignored planning docs, the rendered
  cloudflared yaml;
- Claude Code: the project directory (transcripts and Tier-2 memory),
  `settings.json`, `CLAUDE.md`, `skills/`, `output-styles/`, `hooks/` and the
  status line script that `settings.json` refers to;
- the Playwright storage-state snapshot (ADR-022), the cloudflared directory,
  the commit-guard directory (`~/.config/commit-guard/`: denylist, scanner,
  hook dispatcher) and the global git hooks (`~/.config/git/hooks/`), the
  installed Nucleus launchd plists;
- the Obsidian vault.

Not backed up: tracked source (it is in git), `target/`, `node_modules/`,
caches, the Playwright browser profile, the Caddy certificate store (Caddy
requests new certificates). `[backup] exclude` holds file and directory names
excluded at any depth. A configured path that does not exist is skipped and
logged.

### SQLite

Every file under the backed-up paths that starts with SQLite's 16-byte
header is a database, whatever its name (this catches `*.db.bak-*` copies).
Each one is copied with SQLite's online backup API (rusqlite 0.32, which links
the same `libsqlite3-sys` as sqlx) in a single step, which is one read
transaction, so the copy is consistent while the bots write. The copy is
switched to `journal_mode=DELETE` and must pass `PRAGMA quick_check`. Copies
go to `<staging_dir>/work/sqlite/<absolute path>` (the same layout as the
homelab's own backup), and the live file with its `-wal`, `-shm` and
`-journal` files is excluded from restic. `work` is the only directory the
job creates or deletes: it holds a marker file, and a `work` without the
marker is never removed. A `staging_dir` that is `/`, the home directory or
one of its parents, or that contains or lies inside a backed-up path is a
configuration error. Snapshots made on the first day used
`<staging_dir>/sqlite`; restore handles both layouts. restic expands
environment variables in exclude files, so every `$` in an exclude line is
written as `$$`. A database whose copy fails is also excluded (a raw
copy of a live database is what this step avoids); the run then fails.

The walk that finds the databases and restic's own walk must exclude the
same entries. Exclusions are therefore names only, matched the same way on
both sides.

### The HDD guard

When the HDD is not mounted, its mount point is an empty directory on the
homelab's system disk. `restic backup` refuses a path without an initialised
repository, so a run that starts with the HDD missing fails. The risk is
during a run: if the HDD disappears after restic has opened the repository,
restic's SFTP backend creates missing directories, and data could go to the
system disk. The job runs
`findmnt -n -P -o UUID,SOURCE,TARGET --target <repository path>` and the
same for `/` over SSH before the backup, again after the backup (before
`forget --prune`), and again before `restic check`. It refuses when:

- the path does not exist;
- the path's filesystem is the root filesystem (same UUID, same block device
  without the btrfs subvolume suffix, or mount target `/`);
- the path's filesystem UUID is not `NUCLEUS_BACKUP_HDD_UUID`.

A refusal at any of the three points is a failed run with an alert; after a
refusal nothing more is written to the repository. `nucleus backup guard
[--path <p>]` runs only the guard. The repository string must be the
`sftp:host:/absolute/path` form so the guard knows the absolute path.

Optional manual hardening on the homelab, not done by Nucleus: with the HDD
unmounted, `chattr +i` on the empty mount-point directory makes it
read-only on the system disk, so nothing can create files there while the
HDD is missing. It must be run while the HDD is unmounted.

### Password

A random 256-bit password, stored in two places: the Mac's login Keychain
(restic reads it through `--password-command`, which runs `security
find-generic-password -w`), and a root-only file on the homelab, so the
backup can be read when the Mac is gone (the homelab repository uses the same
model). The password is never in an argument list, log, tracked file or
alert. The operator keeps a third copy outside both machines.

### Schedule, retention, check

launchd wakes `nucleus backup run --scheduled` at :00, :15, :30 and :45.
The run proceeds when `[backup] cron` (05:30 daily) has a match since the
last successful scheduled run, so a Mac that was asleep runs it at the next
wake. Without a watermark the first wake runs. After a backup with no
failure, `restic forget --prune` keeps 7 daily, 4 weekly and 12 monthly snapshots,
grouped by host and tag (the root list changes when a configured path
appears, and grouping by paths would then keep every snapshot). Snapshots
carry a fixed host name and the tag `nucleus`. A run with any failure
(a database copy, an unreadable file) does not prune: an incomplete snapshot
must not push a complete one out of the retention window. When `[backup]
check_cron` (Sunday 05:30) is due and nothing failed, the run ends with
`restic check --read-data-subset 10%`.

A run holds `memory/backup.lock`; a second run at the same time exits.

### Failure handling

A failed scheduled run (guard refusal, SSH failure, SQLite copy failure,
restic error, files restic could not read, forget or check failure) exits
non-zero, which the dashboard's `/agents` shows as an error, and enqueues one
WhatsApp alert to the operator's DM through the outbound queue, keyed by the
cron occurrence, so it is sent once per day. The alert text is code-owned and
holds no paths, host names or command output; details go to
`memory/backup.log`. The run is retried every `retry_minutes` (60) until it
succeeds; the first success after any outstanding alert, from the same day or
an earlier one, sends one short recovery note. restic 0.19 writes the
per-file errors and the `exit_error` message of `backup --json` to stderr;
both streams are parsed and every error is logged. Exit code 3 (snapshot
written, some files not read) is a failure unless every reported error is a
file that disappeared between restic's listing and its read ("no such file
or directory" during scan or archival). A successful run sends nothing.

Configuration: both `.env` values unset means the backup is not set up, and
the scheduled job does nothing. Only one of them set, or an invalid value
(repository form, Keychain names, `staging_dir`, excludes), is a failed run
with an alert.

### A backup that stops running

The alert covers runs that start. A job that is unloaded, a binary that
crashes before it can alert, or a Mac that is off produce no run. Every
successful backup writes the chore-state watermark `backup.last_success`
(RFC 3339). The heartbeat (ADR-026) already checks chore watermarks from its
watchlist note; the backup is one more item there: a `backup.last_success`
older than 30 hours is reported.

### Restore

`nucleus backup restore --to <empty dir> [--snapshot <id>] [--path <entry>]...`
restores into a directory where files land at their absolute paths, then
moves the SQLite copies from the staging layout back to their original paths
under that directory. `--path` takes the same syntax as `[backup] paths` and
includes both the original and the staged path. Without the Mac, on the
homelab: `restic -r <path> --password-file <root-only file> restore latest
--target <dir>`; the SQLite copies are then under
`<dir>/<staging dir>/sqlite/<original path>`.

### Configuration split

- `.env`: `NUCLEUS_BACKUP_REPOSITORY`, `NUCLEUS_BACKUP_HDD_UUID` (both
  identify one operator's setup). Unset: the scheduled job does nothing.
- `nucleus.toml` `[backup]`: schedule, check schedule and subset, retention,
  paths, excludes, Keychain item names, staging directory, snapshot host
  name, retry interval, SSH timeout, restic binary, alert switch.

## Consequences

- First backup about 1.2 GB; daily runs add what changed. `usage.db` is
  rewritten by each SQLite copy, but restic's content-defined chunking stores
  only the changed pages.
- The staging directory needs free space equal to the databases (~460 MB)
  during a run; it is removed at the end.
- restic reads `~/Documents`, which macOS protects. It runs as a child of the
  signed `nucleus` binary, which holds the Full Disk Access grant (ADR-030).
- The homelab's HDD holds both the homelab's repository and this one. A
  failure of that disk loses the backups, not the Mac's data. A second,
  off-site copy is not part of this decision.

## Rejected

- **Time Machine to the homelab.** Copies live SQLite files as files, has
  no HDD guard, and does not cover the homelab-side restore without the Mac.
- **The homelab pulling from the Mac.** Needs SSH access into the Mac and
  moves the password away from where the data is encrypted.
- **`sqlite3 .backup` as a subprocess.** Works, but the in-process API with
  `quick_check` gives one error path per database and no shell quoting of
  database paths.
- **`--files-from` with a full file list.** restic records every listed path
  as a snapshot path, so snapshots would never share a parent and retention
  groups would grow without limit.
- **A separate dead-man's switch** (an external monitor pinged on success).
  The heartbeat already reads chore watermarks; a second mechanism would
  duplicate it.

## Amendment: off-site copy (2026-11-11)

**Status:** proposed. Decided by the operator; not set up.

**Relates to:** [[ADR-045]] (the storage key is kept in the secret store),
[[ADR-047]] (stage 3 moves the control plane to the homelab).

### Context

A check on 2026-11-11 found that both restic repositories, the homelab's
own and this one (712 MiB), are on the same HDD as the media library.
Losing that disk loses both backups. The Consequences section above records
that limit and leaves the off-site copy out of the original decision. The
limit matters more once the control plane moves to the homelab (ADR-047,
stage 3), because the homelab then holds the live data as well.

### Decision

1. **Mirroring on the 8 TB HDD.** The backup disk is mirrored, so the
   failure of one disk does not lose the repositories. To be set up soon.
2. **An off-site copy of the internal backups** at an off-site object
   storage provider, within that provider's free tier (10 GB).
3. **Encrypted before upload.** restic encrypts the data before it leaves
   the homelab, so the provider stores only ciphertext.
4. **The provider's application key is kept in the secret store**, in the
   operational account (ADR-045), not in `.env`. The backup job is a
   registered code path that gets it through the broker's value verb
   (ADR-045 §3).
5. **The restic password does not move.** It stays in the places of the
   Password section above (the Mac's login Keychain, a root-only file on
   the homelab, the operator's copy outside both machines). It is recovery
   material: a restore may be needed when the secret store is unavailable,
   so it does not depend on the broker (ADR-045 §1).

### Consequences

- The internal backups survive the loss of the homelab's backup disk and of
  the homelab itself.
- The off-site copy must stay within the free tier's 10 GB.
- After a homelab restart the off-site upload fails until the operator
  unlocks the broker; the locked broker is reported in the "what needs you"
  view and by a WhatsApp alert (ADR-045 §4, ADR-046).

### Rejected alternatives

- **The key in `.env`.** Rejected by the operator: the key goes into the
  operational secret store account.

### Open questions / footnotes

1. The mirroring method (filesystem RAID or a second disk with a copy job)
   and its date.
2. How the off-site copy is made (restic `copy` to a second repository, or
   a sync of the repository directory) and on what schedule.
3. Which repositories go off-site when the two together pass 10 GB.

### Verification

- With the mirror built, removing one disk leaves both repositories
  readable and `restic check` passes.
- After an upload, the off-site objects hold no plaintext (a search for a
  known file's content in the uploaded objects finds nothing).
- A restore from the off-site copy to an empty directory on another machine
  succeeds with the restic password.
- The application key is absent from `.env`, from tracked files and from
  logs; a locked broker makes the upload fail with an alert.
- The off-site usage reported by the provider stays under 10 GB.
