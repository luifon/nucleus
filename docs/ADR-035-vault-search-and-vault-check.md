# ADR-035 — Vault search and the weekly vault check

**Status:** Accepted + built (2026-09-24)

**Builds on:**
- [[ADR-005]] — the PARA vault (T3) and the brain-dump multi-op pipeline.
- [[ADR-020]] — DB ownership, versioned migrations, typegen.
- [[ADR-023]] — the session-search FTS5 design this mirrors.
- [[ADR-029]] — watermarks (the weekly schedule gate).

## Context

An operator's vault holds hundreds of notes. No process could search it. The brain-dump planner and the distiller received a folder
tree truncated to 20 sub-folders and 10 notes per bucket and read files one
at a time; the dashboard's vault page showed only the most recently changed
files. Two consequences followed:

1. **Duplicates.** CLAUDE.md Rule 9.4 says to append to an existing note
   rather than create a new one, but a writer that cannot find the existing
   note creates a new one: dated notes on one theme in one folder,
   near-identical notes, and file names used by more than one note.
2. **Decay nobody sees.** Broken `[[links]]`, notes nothing links to,
   captures left in `0-Inbox`, missing frontmatter, and empty files built up
   with no report.

## Decision

### 1. Vault search

- **Index.** `memory/vault_index.db`, written only by the `nucleus
  vault-search` command through `nucleus_core::vault::index::Writer`
  (ADR-020 single writer; see "Index writer" below). SQLite FTS5 with
  columns title, headings, tags, path, frontmatter and body, bm25
  weights 12 / 4 / 4 / 3 / 1.5 / 1, and the `porter unicode61
  remove_diacritics 2` tokenizer, so `orcamento` finds `orçamento` and
  `deciding` finds `decided`.
- **Incremental.** Each call walks the vault and re-reads only notes whose
  (mtime, size) changed; deleted or newly excluded notes are removed. A
  change to the exclusion rules (or to the credential detector version)
  rebuilds the index. A note larger than 2 MiB (`MAX_NOTE_BYTES`) is
  skipped and counted, not read. The index is derived data: deleting the
  file is a valid repair.
- **Index writer.** Only the `vault-search` command writes the index. Two
  invocations at the same time (two sessions) serialize on an advisory lock,
  `memory/vault_index.lock`, and then on one `BEGIN IMMEDIATE` transaction.
  The exclusion rules are read from nucleus.toml after the lock is held, so
  an update always applies the rules that are current when it runs; no
  caller can pass rules it loaded earlier. The dashboard does not write the
  index: before a search it runs `nucleus vault-search --reindex` as a
  subprocess and then opens the file read-only. At most one such update
  runs at a time. The dashboard takes a vault watermark before each search
  (a hash of every walked note's path, size, nanosecond mtime and inode,
  plus the exclusion rules' fingerprint; no content is read). Searches that
  arrive while an update runs for the same watermark wait for that update's
  result instead of starting another; a search whose watermark differs
  waits for it to end and then starts the next one. No update runs when
  the last successful one started less than
  `[vault_search] dashboard_reindex_fresh_secs` (30) ago and the watermark
  is unchanged. A search waits at most `dashboard_reindex_wait_secs` (20)
  and then answers 503; the update keeps running as its own task, and the
  next search uses it.
- **Query.** Plain words must all match (each word is quoted, so
  punctuation cannot break the FTS5 syntax). When no note has every word,
  the search falls back to notes with any word and reports `mode: any`. A
  query that uses FTS5 syntax (`OR`, `NOT`, `"phrase"`, `prefix*`) is passed
  through. Optional path-prefix filter (`--bucket 3-Projects` or a folder);
  `%` and `_` in it match only themselves.
- **Results.** path, title, display name, bucket, `created`, `source`,
  snippet, score. A generic file name (`index.md`, `README.md`) is displayed
  with its parent folder (`Alpha/index.md`), because many folders have one.
- **Surfaces.** `nucleus vault-search <words> [--bucket] [--limit] [--json]
  [--reindex]`; a repo skill, `.claude/skills/vault-search`, that tells
  sessions to search before browsing; `GET /vault/api/search` and a search
  tab on the dashboard vault page.
- **Writers use it.** The brain-dump planner prompt and the distiller's
  contemplation prompt now tell the session to search each theme before a
  CREATE and to append to a note the search returns. The planner session
  pre-approves the read-only CLI. A distiller ARCHIVE that names an existing
  note appends to it, and a frontmatter block at the start of the appended
  body is dropped.

### 2. Exclusions (credentials never appear in search)

Credentials reach the operator only through the authenticated DM. A search
result can be shown by any session, pasted into a chat, or displayed on the
dashboard, so a note that holds a credential must never be indexed or
returned. Excluded files are also never listed in a vault-check report,
never modified by a fix, and never opened, listed or named by the
dashboard. Two layers, shared by search, check and the dashboard:

- **Path globs.** A floor in code that configuration cannot remove: dot
  folders (`.obsidian/`, `.trash/`), names containing `credential`,
  `credencia`, `password`, `passwd`, `senha`, `secret`, `api-key`, `apikey`,
  `*.pem`, `*.key`, and any folder named `homelab`, where operators keep
  service logins. `[vault_search] exclude` adds globs (default:
  attachment and asset folders). Non-markdown files are never indexed.
- **Content.** A built-in detector, also not removable:
  - well-known key formats and PEM private keys anywhere;
  - a **secret label** followed by any value that is not a placeholder.
    A secret label is exactly a secret word or pair (`password`,
    `passphrase`, `senha`, `pin`, `secret`, `token`, `API key`, `chave de
    API`, with markdown removed: `- **Senha:**`, `API_KEY=`), a secret word
    followed by an owner preposition (`senha do roteador`, `password for
    the printer`), or two to four words ending in a secret word that is not
    preceded by a modifier preposition (`wifi password`; not `reset de
    senha`). The value may contain spaces (`password: correct horse
    battery staple`); when the line has no value, the next non-empty line
    that is not a heading is the value. Placeholders are an empty value,
    a value wrapped in `<>`, `[]`, `{}` or `()` (template slots and
    `[[note links]]`), mask characters only (`xxx`, `***`, `...`), a short
    list of words (`none`, `n/a`, `tbd`, `redacted`, `true`, `false`, …),
    and a reference of at most six words that starts with a word such as
    `see`/`in`/`no`/`veja` and names a store (`see vault`, `in 1password`,
    `no cofre`). This rule has false positives (`Token: create one under
    Settings` excludes its note) by design;
  - a label of any other length that mentions a secret word, whose value,
    on the same line or the next non-empty line, is one token of six or
    more characters with letters and digits (`API key on file:` followed
    by the key). Prose such as `Password policy: 12 characters` stays
    searchable.

  A value loses a trailing comment first: YAML (` # prod`), HTML
  (`<!-- -->`) or Obsidian (`%% %%`). `[vault_search]
  credential_content_regex` adds patterns. The detector has a version in
  the index fingerprint; a change to it rebuilds every index.
- **Folding.** Paths and text are compared after Unicode NFKC and the
  removal of every Default_Ignorable_Code_Point, so full-width letters or a
  zero-width character inside a word (`pass<U+200B>word`) match the plain
  spelling in both layers.

The name floor is deliberately broad. A false positive costs one note
missing from search; a false negative puts a credential in a search result.

**Rules are read at the moment of use.** `Exclusions::load` reads
`[vault_search]` from nucleus.toml on each call. The index writer calls it
under its lock; the dashboard calls it on every request. A rule the
operator adds applies at the next request, without a restart. An unreadable
or invalid nucleus.toml fails the request instead of falling back to weaker
rules.

**Dashboard surfaces** (`nucleus_core::vault::access`):

- `/vault/api/file` takes a vault-relative path only. An absolute path, `..`,
  `.` or an empty component is refused (400), then the path rules are
  checked on the path. The note is opened relative to a descriptor of the
  vault root, one component at a time with `O_NOFOLLOW`
  (`nucleus_core::vault::fsx`), so a symlink anywhere below the root is
  refused and answers 404, like a missing note. The opened descriptor is
  checked with `fstat` to be a regular file, and the text is read from that
  descriptor; the path is never opened a second time, so a file or folder
  swapped for a symlink between the check and the read cannot point the
  read outside the vault. Then the size limit, then the content detector on
  the text. An excluded note and a missing note both answer 404. Responses
  carry `Cache-Control: no-store`.
- `/vault/api/recent` lists notes from the exclusion-aware walk and checks
  each returned note's text. Its `bucket` filter is parsed and opened like
  `/file`; an excluded folder returns an empty list. It returns relative
  paths and the vault folder name, never an absolute path.
- `/vault/api/buckets` and the home-page "latest vault write" glance use the
  same walk. `/vault/api/search` drops any hit that `/file` would refuse
  under the current rules.

The walk shared by the index, the check and the dashboard (`vault::scan`)
uses the same descriptor access: each folder is opened from the root
without following symlinks and listed from its descriptor, entries are
read with `fstatat(.., AT_SYMLINK_NOFOLLOW)`, and each note is read through
the root descriptor. A symlink in the vault is never listed, indexed or
followed.

### 3. The weekly vault check

`nucleus vault-check` is deterministic (no Claude session).
`nucleus_core::vault::check` does the analysis and owns
`memory/vault_check.db` (run history); the `chores/vault-check` crate is
the command, the schedule gate and the notification.

Findings:

| Kind | Rule |
|---|---|
| `duplicate_name` | The same file name (normalized) in more than one folder. `[[name]]` is ambiguous. Generic names and daily notes are skipped. |
| `dated_series` | Three or more notes in one folder whose names are a date plus the same theme (`2026-03-01-standup`, `2026-03-08-standup`, …): notes that should have been appends (Rule 9.4). |
| `similar_title` | Parsed titles (frontmatter `title`, else the first H1, else the file stem) with the date removed whose word sets have Jaccard similarity ≥ 0.8. |
| `duplicate_content` | Bodies of 60+ words whose 5-word shingle sets have estimated Jaccard ≥ 0.6 (64-hash MinHash). |
| `broken_link` | A `[[target]]` that resolves to no file. Fenced and inline code and escaped `\[[text]]` are skipped; an alias after a pipe (including the backslash-escaped pipe used inside tables), `#heading` and `#^block` are handled; resolution follows Obsidian (case-insensitive, by file name anywhere or by path suffix; a link to an excluded file counts as resolved). When a folder that is not excluded has the target's name, the finding says so; an excluded folder is never named. |
| `orphan` | A note no other note links to (wiki-links and relative markdown links count, including `[text](<name with spaces.md>)`). Exempt by default: `README.md`, `index.md`, `Home.md`, `_`-prefixed pipeline files, `0-Inbox`, `1-Main-Notes`, `2-Daily-Notes`, `7-Archives`. |
| `stale_inbox` | A `0-Inbox` note older than `inbox_max_age_days` (14), by `created`, else file birth time, else mtime. |
| `frontmatter` | No frontmatter block, invalid YAML, or a key from `required_frontmatter` (`created`, `source`; Rule 9.7) missing or present with no value (reported as `empty:`). |
| `unknown_source` | A `source:` value outside `source_vocabulary`, grouped by value. An entry ending in `*` is a prefix; `a+b` passes when each part does. The shipped vocabulary names only the writers Nucleus ships plus generic origins; operators add their own. |
| `empty_file` | A 0-byte file, a note with no text after its frontmatter, or a canvas with no nodes. |
| `oversized` | A note larger than 2 MiB; it is not read or checked. |

One safe fix, only with `--apply` (or `scheduled_apply = true`), recorded on
its finding (`not applied: <reason>` when a check below fails): move an
empty file whose name starts with `Untitled` (Obsidian's default name) and
that nothing links to into the quarantine. The steps:

1. Re-read every note's links; a note written since the analysis may link
   to the file.
2. Open the file's folder once, relative to the vault root descriptor
   without following symlinks.
3. Check the entry with `fstatat(.., AT_SYMLINK_NOFOLLOW)` against the scan:
   the same device and inode, size and nanosecond mtime. Open it with
   `O_NOFOLLOW` and confirm that its content is still empty. Check the entry
   again.
4. Move it with an exclusive rename from the folder descriptor into the
   quarantine: `renameatx_np(.., RENAME_EXCL)` on macOS,
   `renameat2(.., RENAME_NOREPLACE)` on Linux. An exclusive rename never
   replaces an existing entry.
5. Check the moved entry against the scan. If another process replaced or
   wrote to the file between step 3 and step 4, it does not match, and it
   is moved back with the same exclusive rename. If the path was created
   again before the move back, the move back fails instead of replacing
   that file; the moved file stays in the quarantine run folder and the
   finding reports the error.

Where the platform or the filesystem has no exclusive rename, the fix is
not applied and the file is only reported.

The check never writes into a note. A missing or empty `created:` key is a
`frontmatter` finding that the operator resolves. An earlier version of this
ADR added `created:` automatically; that fix was removed, because replacing a
note's content cannot be made conditional on the note being unchanged: an
editor or a sync client can replace the file between the last check and the
replacing rename, and the rename then discards that write.

**Quarantine.** `memory/vault-quarantine/<UTC time>-<pid>/`, in the
workspace and outside the vault, so Obsidian and its sync do not see it:
`deleted/<path>` holds moved files. Restoring is a move back. Run folders
older than 30 days are removed at the start of the next applying run. The quarantine must be
on the same filesystem as the vault (a rename); otherwise the fix is
reported as not applied.

Notes are otherwise never moved or renamed.

**History.** Each run stores its counts in `check_runs` and its findings in
`check_findings`. Findings are kept for the latest 26 runs; counts are kept
for every run, which is what the trend needs.

**Output.** The command prints a grouped report (or `--json`). The
dashboard vault page has a check tab at `/vault/check` with count tiles
(and the change since the previous run), findings grouped by kind, and the
run history. A scheduled run enqueues one line to the operator's WhatsApp
DM through the reminders outbound-queue path (`store::enqueue_whatsapp`,
target = first entry of `WHATSAPP_ALLOWED_DM_JIDS`, ADR-005b), for example
`🗂️ vault check: 3 duplicates, 5 broken links, 2 fixed — <NUCLEUS_PUBLIC_URL>/vault/check`.
Zero counts are omitted; without `NUCLEUS_PUBLIC_URL` the link is omitted.
A manual run prints findings with their paths. A scheduled run writes to the
launchd log, so it prints counts and fix outcomes only (`--details` prints
the findings), and the check logs fix failures without paths.

**Schedule.** `tools/launchd/vault-check.plist.example` starts
`nucleus vault-check --scheduled` every hour. The command runs only when
`[vault_check] cron` (default `0 20 * * 0`, Sunday 20:00 in `NUCLEUS_TZ`)
has had a match since the last scheduled run, recorded as the ADR-029
watermark `vault-check.scheduled`. The first wake after install only
records the watermark, so installing the job does not send a report. A
machine that was asleep or off at the scheduled time runs the check at the
next wake (the reminders fire-late policy). A failed run does not advance
the watermark and is retried the next hour.

Each cron match is one occurrence. Before the check runs, the process
claims the occurrence in `vault_check.db` (`scheduled_claims`, keyed by the
match time, inside `BEGIN IMMEDIATE`); a second process at the same wake
sees the claim and exits. The WhatsApp summary is enqueued with
`source = vault-check:<occurrence>` and only when no row with that source
exists (`store::enqueue_whatsapp_once`, one statement). The claim is marked
complete after the enqueue, then the watermark advances. A claim left
incomplete by a process that died is taken over after 30 minutes; the
takeover reruns the check but cannot queue a second summary.

## Rejected alternatives

- **A system-seeded reminder.** A reminder either posts a static body or
  spawns a Claude session; the check needs neither. Running it through a
  `--condition` command would misuse the watcher (ADR-024) as a scheduler.
- **`StartCalendarInterval` in the plist.** It works, but the day and time
  would live in a launchd template instead of nucleus.toml, and changing
  them would need a reinstall. The hourly gate keeps the schedule a
  nucleus.toml setting.
- **Embeddings / vector search.** Still rejected (ADR-002): the billing
  model does not cover an embedding provider, and FTS5 with stemming and
  accent folding finds notes by the words they contain, which is what the
  append-before-create decision needs.
- **Automatic moves, renames or merges of duplicates.** Which note should
  survive is a judgment. The check reports; the operator or a capture with a
  `move` op decides.
- **Check history in the index DB.** The index is derived and may be
  deleted to repair it; the run history is not.

## Consequences

- Sessions find notes by content with an incremental index update per
  query; the planner and the distiller have the tool and the instruction to
  append instead of duplicating.
- A credential written into an ordinary note stays out of search, the
  dashboard, and the check report. The detector can miss a secret written
  in prose; the floor and the extra regex are the places to tighten it.
- Two new DBs under `memory/`, both with schemas in core. `vault_index.db`
  has one writer, the `vault-search` command (the dashboard runs it as a
  subprocess and reads the file read-only); `vault_check.db` has one writer,
  the `vault-check` command; the dashboard reads it read-only. See the
  ADR-020 amendment for concurrent invocations of one writer command.
- A dashboard search costs a stat-only walk of the vault (the watermark)
  and, when a note changed or the freshness window has passed, one short
  subprocess (the incremental update) shared with every concurrent
  search.
- The dashboard chat surface runs a Claude session with the vault as an
  added directory; the exclusion rules here do not restrict what that
  session can read.
