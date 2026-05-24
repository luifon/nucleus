# ADR-004 — Diary tier and distillation pipeline

**Status:** Accepted (2026-05-12) — partially superseded by [[ADR-016]] (2026-05-24)

> **ADR-016 amendment.** The two-stage distiller below (hourly metabolism +
> weekly contemplation) is consolidated into **one daily pass**; the diary
> tier, tags, and PROMOTE/MERGE/ARCHIVE/DROP vocabulary are unchanged. The
> "Persona evolution (SOUL slot)" section is **deferred** out of the distiller
> to ADR-016's future skill-gap learner (reviewable suggestions, not silent
> writes to the now-gitignored `personas/<slug>.md`). Raw transcripts are now
> indexed per run (ADR-016 run-log) — the "no full-transcript logging" footgun
> below still holds: we index pointers, we don't copy transcripts.

## Context

Tiers 1 (session DBs) and 2 (shared facts) leave a gap: things that happened during a session that aren't important enough to be a "permanent fact" but might matter later. With only Tier 1+2, those observations either get prematurely promoted to shared memory (noise), or lost when the session DB is pruned (regret).

We add **Tier 1.5 — Diaries** between them. Each agent journals as it works; a periodic distiller decides what's worth keeping permanently.

OpenClaw research (`coolmanns/openclaw-memory-architecture`, `liuhao6741/openclaw-memory`, `yoloshii/ClawMem`) confirmed the pattern works and surfaced specific footguns we avoid.

## Layout

```
nucleus/memory/diaries/
├── discord/
│   ├── 2026-05-12.md
│   ├── 2026-05-13.md
│   ├── _pending.md           # candidates queued by hourly metabolism
│   └── _retention.json       # read-counts for Hebbian reinforcement
├── whatsapp/
├── obsidian-chat/
├── news-fetcher/
└── distiller/                # the distiller journals about itself
```

Per-agent directories prevent cross-agent collisions. Single bot process per agent → no intra-agent contention.

## Entry format

Decisions and observations only. **Never full transcripts** (OpenClaw footgun: disk + distill cost explodes, transcripts are rarely re-read).

```markdown
---
agent: discord
date: 2026-05-12
turns: 14
---

## 22:31 — #daily
User asked when the news job last ran. Surfaced from dashboard.
- OBSERVATION: linking to /dashboard from Discord replies would shortcut this.

## 23:05 — DM
"stop, just give me the answer." Reverted to lead-with-answer mode.
- NOTABLE: reinforces existing communication-style memory; bumping read_count.
```

Tags: `FACT`, `FEEDBACK`, `OBSERVATION`, `NOTABLE`. The distiller treats them as classification signals.

## Who writes

- **v0:** the calling binary auto-appends a one-line "what just happened" entry after every Session ask (no agent involvement). Cheap, captures basic activity. This is what's wired today via `core::diary::record_observation`.
- **v1:** the spawned `claude` session gets a `diary_record(tag, body)` tool — agent self-tags observations as it works. Richer entries get richer treatment from the distiller.

## Distillation — two stages

### Hourly metabolism (cheap)
- Cron: top of every hour
- Model: Haiku (small, fast)
- Input: each agent's diary entries from the past hour
- Output: candidate facts/feedbacks staged in `_pending.md`
- Time budget: ~30 seconds total
- Goal: extract candidates while context is fresh, defer judgment

### Weekly contemplation (heavy)
- Cron: Sunday 04:00 (nightshift, doesn't fight foreground latency)
- Model: Sonnet (better judgment)
- Input: each agent's `_pending.md` + the week's diary files
- Output: per candidate, one of (Mem0 operation vocabulary):
  - **PROMOTE** → write a new file in Tier 2 (`$NUCLEUS_TIER2_DIR/`)
  - **MERGE** → update an existing Tier 2 file
  - **ARCHIVE** → write a weekly digest to Obsidian (`~/Documents/Obsidian/Diaries/<agent>/YYYY-Www.md`)
  - **DROP** → no action
- Cleanup: delete diary files older than `retain_days` (default 7) unless explicitly retained

High-confidence single observations (e.g., user explicitly states a preference) **promote immediately** — they don't wait for the weekly cycle. The slow path is for behavioral inferences only.

## Hebbian reinforcement

When an agent reads a diary entry mid-session ("what did I do last Tuesday?"), bump `read_count` in `_retention.json`. Weekly contemplation auto-archives high-read entries to Obsidian even if the LLM judge didn't flag them — frequent reference is itself signal.

## Persona evolution (SOUL slot)

Each persona file (e.g., `messaging/discord/persona.md`) is git-tracked. The weekly contemplation pass has explicit license to edit these when it sees recurring style feedback in diaries ("user keeps asking the bot to be less formal"). Changes are reviewable via `git log persona.md` and revertable.

Slow, deliberate, auditable — opposite of ChatGPT's opaque memory.

## Concurrency

- Per-agent dirs → no inter-agent collision.
- Single process per agent → no intra-agent contention.
- If we ever fork an agent into multiple processes, copy Hermes' `.lock` file pattern.

## Footguns we avoid (from OpenClaw research)

- **No per-turn full-transcript logging.** Disk + distill cost explodes; raw turns are rarely re-read.
- **No 30-day promotion gate.** High-confidence observations promote immediately.
- **Every agent gets distilled.** OpenClaw's metabolism was main-agent-only; sub-agent diaries became write-only landfill.
- **Markdown is canonical.** No SQLite "facts.db" duplicate. The DB (if any) is a derived index, rebuildable from the markdown.
- **No silent forgetting.** DROPs are logged in distiller's own diary so we can audit what got tossed.

## Cross-references

- Letta/MemGPT: `core_memory` / `archival_memory` split validates Tier 2 / Tier 3 contract.
- Mem0: PROMOTE / MERGE / ARCHIVE / DROP operation vocabulary borrowed from their fact-graph pipeline.
- Cursor / Cline `memory-bank/`: same shape as Tier 2.
- Obsidian Periodic Notes: weekly archive filenames mirror its convention so distilled digests drop straight into the user's vault.
