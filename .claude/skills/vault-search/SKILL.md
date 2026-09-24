---
name: vault-search
description: >
  Find notes in the operator's Obsidian vault by content with the
  `nucleus vault-search` CLI (ADR-035) before browsing folders. Use whenever a
  task needs an existing note — "what did I write about X", "is there already
  a note on Y", deciding APPEND vs CREATE before filing a capture (CLAUDE.md
  Rule 9.4), finding siblings to link, or answering from the second brain —
  even if "search" is not said.
flavor: recipe
trigger: model
allowed-tools:
  - "Bash(./target/release/nucleus vault-search:*)"
mcp_needed: []
last_used: null
last_failure: null
failure_count_30d: 0
notify_on_failure: []
---

# vault-search — find vault notes by content

# When to invoke

Any time you need a note that already exists in the vault: before filing a
capture (to append instead of duplicating), before linking siblings, or to
answer a question from the operator's notes. The vault is several hundred
notes; a folder listing shows names only, and many notes share generic names
(`index.md`, `README.md`). Search first, then Read the few notes that matter.

# Steps

## The command

Run from the workspace root:

```bash
./target/release/nucleus vault-search <words…> [--bucket <prefix>] [--limit N] [--json]
```

- `<words…>`: every word must match (accents and word endings are folded:
  `orcamento` finds `orçamento`, `deciding` finds `decided`). When no note has
  every word, the output says so and lists notes with some of them.
- FTS5 syntax works when you need it: `"exact phrase"`, `a OR b`,
  `a NOT b`, `prefix*`.
- `--bucket 3-Projects` or `--bucket 4-Areas/Health` limits to a path prefix.
- `--limit` defaults to 10.
- `--json` prints `{mode, hits:[{path, title, display, bucket, created,
  source, snippet, score}]}` — use it when you will process the results.

The index updates itself before each query (only changed notes are re-read),
so results reflect the vault as it is now.

## Reading the results

Text output, one block per note, best match first:

```
 1. Alpha/index.md — Alpha  created 2026-01-02
    3-Projects/Alpha/index.md
    … hub for the [rocket] engine project …
```

- Line 1: display name (generic names show their parent folder), title,
  `created` from frontmatter.
- Line 2: the vault-relative path — the value to Read, to append to, or to
  link as `[[...]]`.
- Line 3: the best-matching excerpt; matched words are in `[ ]`.

Ranking weights the title most, then headings and tags, then the path, then
frontmatter, then body text.

## Procedures (Rule 9)

**Before creating a note (Rule 9.4).** Search for the capture's main theme
(two to four distinctive words). If a hit covers the theme, APPEND to that
path instead of creating a new file. Several dated notes on one theme in one
folder mean the theme already has a home: append to the most recent one or
to its hub.

**Linking siblings (Rule 9.6).** Search the destination bucket with
`--bucket` for related notes and link only paths the search returned.

**Answering a question from the vault.** Search, Read the top two or three
hits, answer, and cite the paths.

## Hard rules

- Search results never contain credential notes or excluded folders. Do not
  try to reach them another way (grep, Read by guessed path) to answer a
  question: credentials go to the operator only through the authenticated DM,
  never into search output or a chat.
- The CLI is read-only on the vault. It writes only its own index
  (`memory/vault_index.db`).

# Failure modes

- **`no notes match`** — try fewer or more general words, drop the bucket
  filter, or use `prefix*`. The index covers markdown notes only; images,
  PDFs and canvases are not searched.
- **Error about Settings / `NUCLEUS_WORKSPACE_ROOT`** — the command was run
  outside a Nucleus environment. Run it from the workspace root of a
  configured checkout.
- **`cannot read the vault`** — the binary lost its Full Disk Access grant
  (ADR-030, usually after a rebuild without `tools/build.sh`). Report it to
  the operator; do not fall back to guessing note names.
- **A note you know exists is missing** — it may be excluded as a credential
  note (a line such as `password: x` or an API key) or by a folder rule in
  `[vault_search] exclude`. That is intended; do not work around it.
- **Stale-looking result right after a write** — the index updates at the
  start of each query; run the search again after the write completes.
