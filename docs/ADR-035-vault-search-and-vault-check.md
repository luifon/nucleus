# ADR-035 — Vault search and the weekly vault check

**Status:** Accepted + built (2026-09-24)

**Builds on:**
- [[ADR-005]] — the PARA vault (T3) and the brain-dump multi-op pipeline.
- [[ADR-020]] — DB ownership, versioned migrations, typegen.
- [[ADR-023]] — the session-search FTS5 design this mirrors.
- [[ADR-029]] — watermarks (the weekly schedule gate).

## Context

The vault has several hundred notes (845 in the first real run). No process
could search it. The brain-dump planner and the distiller received a folder
tree truncated to 20 sub-folders and 10 notes per bucket and read files one
at a time; the dashboard's vault page showed only the most recently changed
files. Two consequences followed:

1. **Duplicates.** CLAUDE.md Rule 9.4 says to append to an existing note
   rather than create a new one, but a writer that cannot find the existing
   note creates a new one. The first check found two pairs of dated notes on
   one theme in one folder, one set of three near-identical notes, and
   fourteen file names used by more than one note.
2. **Decay nobody sees.** Broken `[[links]]`, notes nothing links to,
   captures left in `0-Inbox`, missing frontmatter, and empty files built up
   with no report.

## Decision

### 1. Vault search

- **Index.** `nucleus_core::vault::index` owns `memory/vault_index.db`
  (ADR-020: only this module writes it; the `vault-search` CLI, the
  dashboard search endpoint and future callers all go through it). SQLite
  FTS5 with columns title, headings, tags, path, frontmatter and body, bm25
  weights 12 / 4 / 4 / 3 / 1.5 / 1, and the `porter unicode61
  remove_diacritics 2` tokenizer, so `orcamento` finds `orçamento` and
  `deciding` finds `decided`.
- **Incremental.** Each call walks the vault and re-reads only notes whose
  (mtime, size) changed; deleted or newly excluded notes are removed. The
  whole update runs in one `BEGIN IMMEDIATE` transaction, so a CLI call and
  the dashboard updating at the same time serialize on SQLite's write lock.
  A change to the exclusion rules (or to the credential detector version)
  rebuilds the index. The index is derived data: deleting the file is a
  valid repair.
- **Query.** Plain words must all match (each word is quoted, so
  punctuation cannot break the FTS5 syntax). When no note has every word,
  the search falls back to notes with any word and reports `mode: any`. A
  query that uses FTS5 syntax (`OR`, `NOT`, `"phrase"`, `prefix*`) is passed
  through. Optional path-prefix filter (`--bucket 3-Projects` or a folder).
- **Results.** path, title, display name, bucket, `created`, `source`,
  snippet, score. A generic file name (`index.md`, `README.md`) is displayed
  with its parent folder (`Alpha/index.md`); 67 notes in the first vault are
  named `index.md`.
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
returned. Excluded files are also never listed in a vault-check report and
never modified by a fix. Two layers, both shared by search and check:

- **Path globs.** A floor in code that configuration cannot remove: dot
  folders (`.obsidian/`, `.trash/`), names containing `credential`,
  `credencia`, `password`, `passwd`, `senha`, `secret`, `api-key`, `apikey`,
  `*.pem`, `*.key`, and any folder named `homelab`, where operators keep
  service logins. `[vault_search] exclude` adds globs (default:
  attachment and asset folders). Non-markdown files are never indexed.
- **Content.** A built-in detector, also not removable: well-known key
  formats and PEM private keys anywhere; a label that is exactly a secret
  word with a one-token value (`password: x`, `- **Senha:** x`,
  `API_KEY=x`); or a label of up to five words naming a secret whose value,
  on the same line or the next non-empty line, is a token of six or more
  characters with letters and digits (`API key on file:` followed by the
  key). `[vault_search] credential_content_regex` adds patterns.

The name floor is deliberately broad. A false positive costs one note
missing from search; a false negative puts a credential in a search result.
In the first real run, 5 files were excluded by path and 3 notes by content,
including two notes that had only a generic name and one note that held an
API key the first per-line regex did not catch.

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
| `similar_title` | Titles with the date removed whose word sets have Jaccard similarity ≥ 0.8. |
| `duplicate_content` | Bodies of 60+ words whose 5-word shingle sets have estimated Jaccard ≥ 0.6 (64-hash MinHash). |
| `broken_link` | A `[[target]]` that resolves to no file. Fenced and inline code are skipped; an alias after a pipe (including the backslash-escaped pipe used inside tables), `#heading` and `#^block` are handled; resolution follows Obsidian (case-insensitive, by file name anywhere or by path suffix; a link to an excluded file counts as resolved). When a folder has the target's name, the finding says so. |
| `orphan` | A note no other note links to (wiki-links and relative markdown links count). Exempt by default: `README.md`, `index.md`, `Home.md`, `_`-prefixed pipeline files, `0-Inbox`, `1-Main-Notes`, `2-Daily-Notes`, `7-Archives`. |
| `stale_inbox` | A `0-Inbox` note older than `inbox_max_age_days` (14), by `created`, else file birth time, else mtime. |
| `frontmatter` | No frontmatter block, invalid YAML, or a key from `required_frontmatter` (`created`, `source`; Rule 9.7) missing. |
| `unknown_source` | A `source:` value outside `source_vocabulary`, grouped by value. An entry ending in `*` is a prefix; `a+b` passes when each part does. The shipped vocabulary names only the writers Nucleus ships plus generic origins; operators add their own. |
| `empty_file` | A 0-byte file, a note with no text after its frontmatter, or a canvas with no nodes. |

Safe fixes, only with `--apply` (or `scheduled_apply = true`), each recorded
on its finding and logged:

- delete an empty file whose name starts with `Untitled` (Obsidian's
  default name) and that nothing links to;
- add `created: <date>` from the file's birth time to a note that lacks it,
  when its frontmatter is valid or absent and the note is not empty. The
  file is rewritten in place (same inode, so the birth time survives) and
  its mtime is restored. A note that changed since it was read is skipped.

Notes are never moved or renamed, and invalid YAML is never rewritten.

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

**Schedule.** `tools/launchd/vault-check.plist.example` starts
`nucleus vault-check --scheduled` every hour. The command runs only when
`[vault_check] cron` (default `0 20 * * 0`, Sunday 20:00 in `NUCLEUS_TZ`)
has had a match since the last scheduled run, recorded as the ADR-029
watermark `vault-check.scheduled`. The first wake after install only
records the watermark, so installing the job does not send a report. A
machine that was asleep or off at the scheduled time runs the check at the
next wake (the reminders fire-late policy). A failed run does not advance
the watermark and is retried the next hour.

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

- Sessions find notes by content in about 0.02 s after the first index
  build; the planner and the distiller have the tool and the instruction to
  append instead of duplicating.
- A credential written into an ordinary note stays out of search, the
  dashboard, and the check report. The detector can miss a secret written
  in prose; the floor and the extra regex are the places to tighten it.
- Two new DBs under `memory/`, both owned by core. `vault_index.db` has many
  writers through one module (like `session_index.db`); `vault_check.db` has
  one writer, the `vault-check` command; the dashboard reads it read-only.
- `/vault/api/file` accepts vault-relative paths so search hits open inline.
  It still serves any markdown file under the vault root when given its
  path; the exclusion rules apply to search and to the check, not to that
  endpoint.

## First real run (2026-09-24, release build, read-only)

| | |
|---|---|
| Markdown notes | 845 |
| Excluded by path / as credentials | 5 / 3 |
| Indexed | 837 |
| Index build (cold / no change) | 0.48 s / 0.01 s |
| Search (incl. the incremental update) | 0.02 s |
| Check | 0.23 s |
| Findings | 18 duplicate groups, 27 broken links, 71 orphans, 0 stale inbox, 5 frontmatter, 605 notes with a source outside the default vocabulary (24 values), 1 empty file |
