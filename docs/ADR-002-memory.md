# ADR-002 — Tiered Memory

**Status:** Accepted (2026-05-12)

## Context

Hermes used a flat-ish memory: `SOUL.md` (persona slot, empty), `USER.md` (auto-built profile), `MEMORY.md` (free-form notes), plus mem0 (Postgres+pgvector + Neo4j) for vector recall and an Obsidian vault. Two problems: per-chat conversation state mixed with cross-cutting facts, and the LLM had no clean way to promote a session-local insight into shared knowledge.

We split memory by **lifetime and scope**.

## Tiers

```
TIER 1   — Session DBs               per-chat continuity, ephemeral within a chat
            nucleus/memory/discord.db        (channel_id → claude_session_id)
            nucleus/memory/obsidian-chat.db  (chat_id → session, messages)

TIER 1.5 — Diaries                   per-agent daily markdown, 7-day rolling
            nucleus/memory/diaries/<agent>/YYYY-MM-DD.md
            See ADR-004 for the distillation pipeline.

TIER 2   — Shared facts              auto-loaded into EVERY claude session
            $NUCLEUS_TIER2_DIR/*.md  (typically Claude Code's auto-memory
            dir for the workspace's encoded CWD)
            Structured: user / feedback / project / reference

TIER 3   — Second brain               PARA-organized Obsidian vault, written by both
                                      bots and the user; Claude reads via --add-dir
            ~/Documents/Obsidian/
              0-Inbox/        capture-now-organize-later landing pad
              1-Projects/     short-term efforts with a deadline
              2-Areas/        ongoing responsibilities
              3-Resources/    reference material
              4-Archives/     inactive items from the other three

TIER 4   — Vector recall              ❌ deferred indefinitely
            mem0 needs an embedding + LLM provider; neither is covered by
            Claude Max. T3 (PARA-Obsidian + Claude reading via --add-dir)
            handles long-tail recall instead — Claude navigates the vault
            semantically through filenames + frontmatter, no embeddings
            required. The mem0 docker stack is kept idle in
            tools/mem0/docker-compose.yaml in case we ever revisit.
```

## Why Tier 2 lives at that path

Claude Code's auto-memory is keyed by working directory. Every Nucleus binary spawns `claude` from `~/Development/nucleus/...` (via `core::claude_session::Session`), so they all auto-load the same memory dir without us writing a custom loader. Plus `--add-dir` extends file access (e.g., the Obsidian vault) without changing the memory namespace.

Tier 2 is the **canonical** shared memory for everything Nucleus-spawned. The home-scoped Claude Code auto-memory dir (encoded from `~/`) is for the human's own Claude Code sessions started from `~`.

A small subset (user profile + communication style) is also mirrored into `~/.claude/CLAUDE.md` so it loads in **every** Claude Code session regardless of CWD.

## Tier 1 layout

Each bot owns its own SQLite. Per-bot blast radius — if the Discord bot's DB corrupts, the news API keeps working. Schema lives next to its owner (`messaging/discord/migrations/`, etc.).

## Promotion (Tier 1 → Tier 2)

Three layers:

1. **Manual** — Discord slash command `/remember <fact>` writes a memory file directly. DM "remember that I prefer X" → bot interprets and saves.
2. **Per-call** — `core::memory::promote(kind, name, body)` is exposed as a tool to spawned `claude` sessions. The agent decides something is worth keeping, writes inline, replies "saved."
3. **Distiller** — hourly metabolism + weekly contemplation passes (`chores/distiller`) read agent diaries, ask Claude to extract candidates, then promote/merge/archive/drop against Tier 2. Catches what the in-the-moment promotion missed. See ADR-004.

## mem0 (Tier 4)

**Deferred indefinitely.** mem0's architecture assumes an OpenAI-shaped split — an embedding model for vector retrieval AND an LLM for entity/relation extraction. Neither is covered by the Claude Max subscription that powers Nucleus, so wiring mem0 would re-introduce exactly the per-call API billing we removed when we ditched DeepSeek.

The role mem0 was supposed to play (long-tail recall over months of accumulated facts) is filled by **T3 = PARA-Obsidian** instead: Claude navigates the vault semantically through filenames, frontmatter, and section headers — no embeddings needed because Claude reads context, not vectors. The mem0 stack is kept idle in `tools/mem0/docker-compose.yaml` in case the ergonomics ever change.

## File format (Tier 2)

```markdown
---
name: short-kebab-slug
description: one-line summary used to decide relevance
metadata:
  type: user | feedback | project | reference
---

Body. For feedback/project, include:
**Why:** the reason
**How to apply:** when this kicks in

Link related memories with [[other-name]].
```

`MEMORY.md` is the index — one line per memory, `- [Title](file.md) — hook`.
