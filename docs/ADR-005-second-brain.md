# ADR-005 — T3 = PARA-Organized Second Brain in Obsidian

**Status:** Accepted (2026-05-14)

## Context

T3 was originally a flat `~/Documents/Obsidian/Diaries/<agent>/YYYY-Www.md` archive — weekly distilled diary digests in per-agent silos. Useful as an audit trail; useless as a knowledge base. Browsing it felt like reading server logs, not consulting your own notes.

Meanwhile T4 (mem0 vector recall) was the planned long-tail-recall layer, but mem0 needs an embedding model AND a separate LLM provider — neither is covered by the Claude Max subscription that powers Nucleus. Wiring it would re-introduce the per-call API billing we removed when we ditched DeepSeek. T4 is now [[ADR-001|deferred indefinitely]].

The role T4 was supposed to play (long-tail recall over months of accumulated facts) needs a home. T3, restructured, is that home.

## Decision

T3 is a PARA-organized Obsidian vault at `~/Documents/Obsidian/` with three extension buckets (renumbered 2026-05-21):

```
0-Inbox/         capture-now-organize-later landing pad
1-Main-Notes/    hubs / MOCs / recurring-question answers (curated by user)
2-Daily-Notes/   time-anchored journal entries (YYYY-MM-DD.md)
3-Projects/      short-term efforts with a deadline + defined outcome
4-Areas/         ongoing responsibilities with a standard to maintain
5-Resources/     reference material on topics of interest
6-Slipbox/       atomic evergreen ideas (Zettelkasten, flat, no sub-folders)
7-Archives/      inactive items from any of the other buckets
```

The PARA core (Projects/Areas/Resources/Archives) is Tiago Forte's *Building a Second Brain* scheme — buckets ordered by **actionability**, not topic. A note moves between buckets as its actionability changes — e.g., a note about deploy strategy starts in Projects (active rollout) → Areas (ongoing devops practice) → Archives (next platform).

The three additions are research-validated extensions to plain PARA:
- `1-Main-Notes` ≈ LYT's "Atlas" / Forte's MOC pattern — index/hub notes that link clusters of related material
- `2-Daily-Notes` ≈ LYT's "Calendar" — time-anchored reflection
- `6-Slipbox` ≈ Zettelkasten — atomic evergreen ideas that PARA's actionability axis doesn't cover (resolves the LYT-author's PARA critique: "Archive eats knowledge")

## How Claude reads + writes the vault

The vault is exposed to Claude sessions via `--add-dir ~/Documents/Obsidian/` (already wired for the Obsidian chat in `dashboard/src/obsidian.rs`). For the bots that don't currently have it as an add-dir (Discord/Alfred), we extend the same when they need vault access.

**Reading.** Claude navigates the vault semantically through filenames + frontmatter + section headers + the per-bucket README files. No embeddings — context-driven traversal. The README in each bucket tells Claude what belongs there.

**Writing.** When a bot writes a note, it follows three rules (codified in CLAUDE.md Rule 9):
1. **Pick the right bucket.** Use the bucket READMEs as ground truth. If unsure between two, prefer 0-Inbox.
2. **Link siblings.** Read the immediate siblings (other notes in the same Project/Area) and add `[[wiki-links]]` to anything thematically related. This is how the graph emerges.
3. **Frontmatter.** Every written note gets a YAML frontmatter block with `created`, `source` (which bot/agent wrote it), and `tags` (free-form, optional).

## T2 vs T3 — when is something Tier 2 vs Tier 3?

Both auto-load into Claude sessions in their own way. The split is about **purpose**, not size:

| | T2 — shared facts | T3 — second brain |
|---|---|---|
| Where | `$NUCLEUS_TIER2_DIR/*.md` (Claude auto-memory) | `~/Documents/Obsidian/` (PARA vault) |
| Loaded | Auto, every claude session in the workspace | On-demand via `--add-dir`, when relevant |
| Style | Short, structured, one fact per file | Long-form, prose, decisions, narrative |
| Examples | "user prefers terse replies", "timezone = `$NUCLEUS_TZ`", "Discord home channel = X" | "Notes from class on 2026-04-12", "Why we chose backend X for project Y", "Recipe for the lasagna mom makes" |
| Question | "Does the bot need this in every spawn?" | "Might I want to browse/reference this later?" |

**Promote rule for the distiller.** When a candidate is promoted out of T1.5:
- Short, recurring, behaviorally-binding → **T2** (`promote(kind, name, body)` writes to the auto-memory dir)
- Longer, narrative, browseable, project/area-tied → **T3** (write to the matching PARA bucket)

Both can happen for the same candidate if it's both a fact AND a context-rich note.

## How the brain-dump pipeline writes (multi-op)

A capture isn't necessarily one file. The brain-dump pipeline asks Claude (with the vault as `--add-dir`) to return a **plan** — a list of OPERATIONS to apply:

```json
{
  "ops": [
    { "op": "create", "bucket": "3-Projects/Example-Project", "filename": "contract.md", "body": "...", "createsSubfolder": false, "reason": "..." },
    { "op": "append", "targetPath": "3-Projects/Example-Project/team.md", "body": "...", "reason": "..." },
    { "op": "move",   "fromPath": "0-Inbox/old-note.md", "toBucket": "3-Projects/Example-Project", "toFilename": "", "createsSubfolder": false, "reason": "..." }
  ],
  "summary": "created 2 docs in Projects/Example-Project, moved 1 from inbox",
  "confidence": 0.85
}
```

The TS code validates each op (path-escape, vault containment, sub-folder gating) and applies them. Rejections don't void the plan — surviving ops still apply. If every op gets rejected, a fallback `create` writes the raw capture to `0-Inbox/` so we never silently lose data.

### Decomposition

A 6-minute audio about a work contract should NOT become one 4KB markdown. It should decompose into themed siblings under a project folder (contract terms / company info / your role / tooling matrix). Coarse, not atomic — typically **1-3 files for a long capture**, sub-headings inside for sub-themes. Zettelkasten-style atomic notes is explicitly NOT the model.

### Sub-folder creation

New sub-folders under `3-Projects/`, `4-Areas/`, or `5-Resources/` may be created **only when the capture explicitly directs it** ("create a folder for X", "Y is one of my projects, put it there"). The op carries a `createsSubfolder: true` flag that the validator gates on — lying about the flag means your op gets rejected. Speculative creation by the bot is forbidden. `6-Slipbox` is flat — never create sub-folders there.

If nothing fits and no directive: file in `0-Inbox/`. The user can correct via a follow-up capture.

### Append-over-duplicate

Strongly prefer `append` over `create` when an existing file already covers a theme. The bot prepends a dated separator (`<!-- appended YYYY-MM-DD via alfred-braindump -->`) so history is preserved and you can manually refactor later.

### Meta-corrections via `move`

When a capture is correcting a prior misfile ("that note from earlier should be in Projects/X, not Inbox"), Claude detects this and emits `move` ops to actually relocate the prior file. The correction does the work — no new "describing what should happen" note is created.

### Why no escalation in v2

V1 had an escalation flow (Claude returns alternatives, bot asks "where does this go?", user replies with a number). V2 dropped it because the multi-op pipeline + meta-corrections cover the same ground more naturally: if Claude misfiles, the user sends a follow-up capture saying so, and the next plan emits `move` ops. Iterative correction is simpler than two-shot escalation, and it scales to plans of any complexity (you can correct multiple ops at once via natural language).

The `pending_classifications` SQLite table is kept for forward-compatibility but isn't currently used.

## What about T1.5 (working diaries)?

Unchanged. T1.5 (`nucleus/memory/diaries/<agent>/YYYY-MM-DD.md`, 7-day rolling) is the bot's operational scratch pad — completely internal. The distiller still reads T1.5 and produces T3 digests; the digests just land in PARA buckets now, not in flat per-agent silos.

## Why PARA core + Zettelkasten side-car

Considered:
- **PARA** — folders by lifecycle, refuses to organize by topic. Optimized for getting things done.
- **Zettelkasten** — atomic notes densely interlinked, organized by emergent graph. Optimized for generating new ideas.
- **Flat + tags** — single dir, organize via tags + search. Cheap; doesn't help with "what's actionable now."

PARA is the spine because:
- The folder name *is* the classification — Claude can pick from a small set of known buckets without ambiguity
- Lifecycle progression is explicit (Project → Area → Archive); pure Zettelkasten has no notion of "this note is now stale"
- Forte explicitly designs PARA to be tool-agnostic; works the same in plain folders

But pure PARA leaks knowledge — atomic ideas that aren't tied to a Project/Area/Resource end up in 0-Inbox forever, or worse, get archived with a completed project (the LYT-author's critique of PARA). The 2026-05-21 renumber added `6-Slipbox` as a Zettelkasten side-car for that case: atomic evergreen notes live there, flat, linked to PARA siblings via `[[wiki-links]]`. We also added `1-Main-Notes` (MOCs/hubs) and `2-Daily-Notes` (Calendar/journal) to round out LYT's three-space model.

Net: PARA for actionable lifecycle, Slipbox for evergreen ideas, Main-Notes for hubs, Daily-Notes for time-anchored reflection. `[[wiki-links]]` between everything gives us emergent graph topology *on top of* the structural folders — best of both with no embedding dependency.

## Why no Graphify (or similar plugins)

Researched [Graphify](https://github.com/safishamsi/graphify) — turned out to be a code-AST tool, not a notes-linking tool. It can graph our codebases for cheaper bot context (possible future S-slice add-on, see [[ADR-001]]) but it doesn't help with notes.

For notes, we get the same effect via Rule 9.2: when Claude writes a note, it reads siblings and links them. Free, no embedding dep, fits our stack.

## What the vault looks like at steady state

```
~/Documents/Obsidian/
├── 0-Inbox/
│   ├── README.md
│   └── 2026-05-14-thought-about-routing.md      ← brain-dump unsorted
├── 1-Main-Notes/
│   ├── README.md
│   ├── Nucleus-index.md                         ← MOC linking all Nucleus material
│   └── How-to-deploy-northmark.md               ← recurring question
├── 2-Daily-Notes/
│   ├── README.md
│   ├── 2026-05-21.md                            ← today's journal
│   └── 2026-W20.md                              ← weekly review
├── 3-Projects/
│   ├── README.md
│   └── SomeProject-Q3-redesign/
│       ├── decisions.md
│       └── 2026-W19-status.md
├── 4-Areas/
│   ├── README.md
│   ├── Nucleus/
│   │   ├── 2026-W19-discord.md                  ← distiller digest
│   │   └── 2026-W19-alfred.md
│   └── Health/
│       └── 2026-04-physiotherapy-notes.md
├── 5-Resources/
│   ├── README.md
│   └── Rust-async-patterns/
│       └── pinning.md
├── 6-Slipbox/
│   ├── README.md
│   ├── why-cleanup-over-parallel-migrations.md  ← atomic idea
│   └── has-css-depth-aware-selectors.md
└── 7-Archives/
    ├── README.md
    └── Projects/
        └── Old-thing/
```

## Cross-references

- [[ADR-001|ADR-001 — slice S4 deferred]] for the mem0/T4 deferral rationale
- [[ADR-002|ADR-002 — tier model]] for the full T1/T1.5/T2/T3/T4 picture
- [[ADR-004|ADR-004 — diary + distillation]] for what feeds T3 from T1.5
- CLAUDE.md Rule 9 — the writing convention bots must follow
