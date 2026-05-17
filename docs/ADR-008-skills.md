# ADR-008 — Skills: procedural memory via Claude Code's native skill mechanism

**Status:** Proposed (2026-05-16)

## Context

The tiered memory in ADR-002 covers *facts* — T1 per-bot session state, T1.5 per-agent diaries, T2 shared facts ("the user prefers terse replies"), T3 PARA-Obsidian long-form. None of those tiers store *procedures* — "when situation X comes up, do Y."

The cost shows up as repetition. A workflow the user runs every weekday morning (open a tool, fetch specific data, format it, post to a channel) has to be re-derived every conversation. The bot has no continuity of *how it did this last time*. Hermes Agent (NousResearch) and OpenClaw both treat procedures as first-class, with auto-generated "skill" files that the next session reads. Without this, Nucleus is stuck re-explaining.

A concrete trigger: the operator has a recurring pre-meeting prep flow that involves an external SaaS reached via Playwright MCP. The flow is stable enough to memorize but specific enough that it doesn't merit a hand-coded feature in a Rust crate — it's *one user's habit*, not a shared capability.

## Decision

Use **Claude Code's native skill mechanism** (`SKILL.md` files in `skills/` directories, auto-loaded as descriptions at session start, full body on-demand). Do not invent a Nucleus-specific format. Layer Nucleus-specific frontmatter conventions on top.

Authoring is assisted by the **skill-creator plugin** (`https://claude.com/plugins/skill-creator`), enabled per-project via `.claude/settings.json`.

The reminders subsystem (ADR-006) is extended with a `system_prompt` column so reminders can spawn a one-shot Claude session at fire time and orchestrate skill execution, rather than only posting text.

## Storage locations

Two locations, chosen by **audience**, not by sensitivity:

| Path | Audience | Committed? |
|---|---|---|
| `~/.claude/skills/<name>/SKILL.md` | Skills for *using* Nucleus — anything the assistant does on the operator's behalf (recurring fetches, daily preps, weekly reviews). Default for new skills. | No (user-global) |
| `.claude/skills/<name>/SKILL.md` | Skills for *working on* Nucleus — debug helpers, replay tools, dev workflows. | Yes (in-repo) |

The dimension is "who's the audience": operator vs. developer. Sensitivity falls out of that — operator skills are inherently personal (they reveal the operator's routines, tools, contacts), developer skills are inherently generic (any contributor could use them).

### Env-var substitution for partial commit cases

A skill body can use Claude Code's `` !`shell-command` `` syntax to resolve identifiers from environment variables at load time:

```markdown
Open !`echo $EXTERNAL_TOOL_URL` and navigate to the $workspace board.
```

This makes it possible to commit a skill's *structure* while keeping the identifying URL in `.env`. **It does not make it OK to commit a skill whose structure itself reveals routine** (workspace names, meeting times, third-party tool affiliation). When in doubt, default to `~/.claude/skills/`.

### Why not `$NUCLEUS_TIER2_DIR/skills/`?

An earlier design proposed storing skills in T2 (`~/.claude/projects/<encoded-cwd>/memory/skills/`). Dropped: that location isn't recognized by Claude Code's native skill loader. We'd reinvent loading, discovery, slash invocation, the `!`cmd`` substitution, and the auto-load-on-keyword behavior. Using `~/.claude/skills/` cedes one dimension (Nucleus-scoping) to gain all of Claude Code's plumbing for free.

## Skill file format

Claude Code's `SKILL.md` format with Nucleus-specific frontmatter additions:

```markdown
---
description: Pre-meeting prep — fetch recent items from an external tool, format for standup
flavor: recipe                # 'recipe' (hand-written) or 'learned' (distiller-promoted)
trigger: manual | reminder    # how it gets invoked
allowed-tools: [Bash, mcp__plugin_playwright_playwright__browser_*]
arguments: [workspace]        # optional positional args (Claude Code feature)
mcp_needed: [playwright]      # informational — MCPs the body assumes are wired
last_used: <date>
last_failure: null            # RFC3339 of last failure, or null
failure_count_30d: 0          # rolling 30-day count
notify_on_failure:            # channels to ping when this skill errors
  - discord-home
tags: [pre-meeting, daily]
---

# When to invoke

When the user says "run pre-meeting prep", "prep my standup", or a scheduled
reminder fires with this skill referenced.

# Steps

1. Open !`echo $EXTERNAL_TOOL_URL` via playwright MCP.
2. Navigate to the $workspace board.
3. ...

# Failure modes        ← REQUIRED section

- `playwright MCP unreachable`: the MCP isn't loaded in this session.
  Surface: tell the user the MCP needs enabling; don't retry blindly.
- `auth expired on the external tool`: tell the user, exit. Don't try
  to re-auth from a bot session.
- `no items found for the period`: not an error; fall back to listing
  yesterday's diary entries.
```

### Nucleus-specific frontmatter (not part of Claude Code's contract)

- `flavor` — `recipe` (hand-written, Phase 1) or `learned` (distiller-promoted, Phase 2)
- `mcp_needed` — informational list of MCPs the body assumes
- `last_used`, `last_failure`, `failure_count_30d` — bot/system updates these where feasible; manual edit also fine
- `notify_on_failure` — channels to alert when the skill errors out (see [Failure handling](#failure-handling))

Claude Code ignores unknown frontmatter; the bot reads it as part of its system context.

### Required body sections

- `# When to invoke` — natural-language triggers
- `# Steps` — what to do, in order
- `# Failure modes` — at least one. An empty Failure modes section signals the skill hasn't been thought through.

This is a *convention*, not enforced by code. Reviewers (the user, or future you) should bounce skills that omit Failure modes.

## Authoring workflow

The bot should **not** author a skill in one shot. Too many wrong turns get baked in. The recommended flow:

1. **Explore interactively.** Open a Claude Code session, walk the surface together (Playwright MCP, the external tool, whatever's involved). Try the workflow. Hit failures, work around them.
2. **Capture what worked.** When the operator says "this is the flow," summarize: trigger, steps, observed failure modes.
3. **Formalize with skill-creator.** Invoke `/skill-creator create`, give it the summary, let it scaffold `SKILL.md` in the correct location (default: `~/.claude/skills/<name>/`).
4. **Test from a fresh session.** Close the exploratory session. Open a new one. Trigger the skill (manually or via reminder). Verify it executes correctly.
5. **Iterate.** `/skill-creator improve` after observing real fires.

Authoring sensitive flows (operator routines, third-party tools): always end up in `~/.claude/skills/`. The skill-creator plugin doesn't know our policy — the operator is responsible for the choice. ADR-011 (future) may automate this.

## Reminders extension

ADR-006 defined reminders as time-triggered notifications: a `body` posted to one or more `channels`. This ADR extends that with a second action type — spawning a Claude session and orchestrating skill execution.

### Schema change

```sql
ALTER TABLE reminders ADD COLUMN system_prompt TEXT;
-- Constraint (in code, not SQL): exactly one of {body, system_prompt} is set.
```

### CLI

```bash
# Body-based reminder (existing, unchanged):
reminders add --cron "30 18 * * 1-5" \
              --body "⏰ End of day — log your hours" \
              --channels discord-home

# System-prompt-based reminder (new):
reminders add --cron "20 8 * * 1-5" \
              --system-prompt "Run pre-meeting-prep skill, post results to discord-home" \
              --channels discord-home
```

`--system-prompt` and `--body` are mutually exclusive at the CLI level.

### Firing logic

- `body` set → existing behavior: post text to channels via outbound queue. Cheap, no Claude session, sub-second.
- `system_prompt` set → spawn a one-shot `nucleus_core::claude_session::Session` (à la news-fetcher), append the stored `system_prompt` via `--append-system-prompt`. Skill auto-loading (Claude Code's native) makes all skills' descriptions visible to the session; the prompt directs which to invoke. Session executes, posts where it decides, exits.

### `--channels` semantics

- Body-based: required, as today — they're the targets of the post.
- System-prompt-based: **optional, used only when the prompt produces output that needs routing**. If the system_prompt explicitly tells the session where to post ("post to discord-home"), `--channels` is redundant. If the prompt is open-ended, the session reads `--channels` as default output routing context. Falls back to `nucleus.toml`'s `reminders.default_channels` (new config key) when neither the prompt nor the flag specifies.

### Why system_prompt and not --skill

Single-skill invocation (`--skill daily-digest`) is a degenerate case of "shape the spawned session's behavior at fire time." A free-form system-prompt fragment gives:

- Composition: `"Run skill-A, then skill-B, summarize both."`
- Ad-hoc orchestration without writing a skill: `"Check yesterday's diary observations and summarize blockers."`
- Mixing skills with channel-routing instructions in one place.

The trade-off (a Claude session spawn per fire) is fine for the cadence reminders typically run at. Simple "ping me" reminders stay on the body path.

## Failure handling

Three layers, by error origin:

| Origin | Routes to |
|---|---|
| Skill-internal failure (caught per its `# Failure modes` section) | The skill's `notify_on_failure` frontmatter |
| Outer error (session crash, can't spawn `claude`, MCP unavailable) | The reminder's `--channels`, falling back to `reminders.default_channels` in `nucleus.toml` |
| Reminder fire-attempt failure (couldn't even start the session) | Recorded in the existing reminder fire history (ADR-006); next tick retries up to the existing retry budget |

The first layer is the skill author's responsibility — write good Failure modes. The second is the reminder author's responsibility — set `--channels` to where you actually want to be alerted. The third is the reminders binary's existing job.

## Two flavors

| Flavor | Source | When |
|---|---|---|
| **recipe** | Hand-written by the operator (or via skill-creator), edited like any other markdown | **Phase 1 — now** |
| **learned** | Auto-promoted by the distiller from `procedure`-tagged diary entries with recurrence ≥2 over the rolling 7-day window | **Phase 2 — future** |

Phase 1 alone delivers the recurring-flow value. Phase 2 closes the Hermes-style learning loop:

- Add `procedure` to `diary::Tag` (alongside `observation`, `decision`)
- A bot that resolves a non-trivial situation appends a procedure entry: *"When I needed X, Y worked. Z failed first."*
- Hourly metabolism reads by tag (existing infrastructure, ADR-004)
- Weekly contemplation promotes entries with recurrence ≥2 to `~/.claude/skills/<slug>/SKILL.md` with `flavor: learned`
- The distiller is responsible for placing learned skills in the operator's user-global tree (not the project tree) — the safe default

Phase 2 isn't a blocker. The schema and frontmatter are forward-compatible.

## Maintenance

Weekly contemplation gains skill-maintenance routes, reusing the existing PROMOTE / MERGE / ARCHIVE vocabulary from ADR-004:

- **ARCHIVE** — `last_used > 60 days`. Move the directory out of the `skills/` tree entirely:
  - `~/.claude/skills/<name>/` → `~/.claude/archive/skills/<name>/`
  - `.claude/skills/<name>/` → `.claude/archive/skills/<name>/`
  - Claude Code's recursive auto-discovery includes nested subdirectories within `skills/`, so renaming to `skills/_archive/` would still load them. Moving out of `skills/` is the safe pattern. Restore by moving back.
- **MERGE** — two skills with ≥80% body overlap. Distiller proposes; operator confirms.
- **STALE-WARN** — a skill body references a file path / env var / MCP that no longer resolves. The distiller posts a warning to `nucleus.toml`'s configured maintenance channel; doesn't auto-fix.

## What is NOT a skill

- A one-off "remind me about X tomorrow" → that's a body-based reminder
- A general fact ("user prefers terse replies") → that's a T2 fact memory
- A code feature worth shipping → if it's generic and worth wiring into a bot's persona or a Rust crate, do that instead. Skills are for *operator-specific* procedures, not for *capabilities the system needs*.

## Out of scope

- **Sharing / marketplace.** No skill export, no community repo. If a skill ever generalizes enough to ship, reimplement it as code (a CLI subcommand, a persona instruction, a bot feature). The friction is intentional.
- **Skill-calls-skill.** A SKILL.md body shouldn't `/invoke` another skill. Composition happens at the reminder layer (multi-skill `--system-prompt`), not inside a skill.
- **Formal typed parameters.** Claude Code's `arguments:` frontmatter handles positional args; most skills will be parameterless. Don't build a parameter DSL.
- **Skill versioning.** `~/.claude/skills/` lives in the user's home directory and is backed up however the operator backs up `~/.claude/` generally (today: not formalized). If a skill regresses, restore from backup or recreate.

## Migration / rollout

Greenfield. No existing skills to migrate. Steps:

1. Add `skill-creator` plugin to `.claude/settings.json` (committed). Document install in README.
2. Create `~/.claude/skills/` and `.claude/skills/` (the latter empty in the initial commit, with a `.gitkeep`).
3. Apply the `reminders.system_prompt` schema migration on next `reminders` binary startup (`ALTER TABLE ... IF NOT EXISTS` pattern, like ADR-006 did for other columns).
4. Add `--system-prompt` flag to `reminders add`, with mutual-exclusion validation against `--body`.
5. Update `nucleus_core::claude_session::Session` reminders path to spawn with `--append-system-prompt` when the reminder has a `system_prompt`.
6. Add `reminders.default_channels` to `nucleus.toml.example`.
7. Author the first real skill via skill-creator → `~/.claude/skills/<name>/`. Use it for at least a week before formalizing Phase 2.

## Future work

- **ADR-011 (proposed)** — guided setup wizard, à la Hermes. Walks the operator through `.env`, `.claude/settings.local.json`, launchd install, plugin install, and seeds default skills/reminders. Includes a step that asks "personal skill or dev skill?" when scaffolding via skill-creator, automating the policy in [Authoring workflow](#authoring-workflow). Out of scope for ADR-008.
- **Phase 2 — learned skills.** Distiller emits and promotes `procedure`-tagged diary entries. Schema and frontmatter are already forward-compatible.

## References

- ADR-002 — tiered memory model and storage location conventions
- ADR-004 — diary, distillation, PROMOTE/MERGE/ARCHIVE vocabulary
- ADR-006 — reminders schema, ticker, channel infrastructure
- CLAUDE.md Rule 1 — secrets stay in `.env`, applies to skill bodies
- CLAUDE.md Rule 2 — personal state stays uncommitted, applies to `~/.claude/skills/` choice
- CLAUDE.md Rule 10 — reminders CLI usage, extended here
- Claude Code skills docs — https://code.claude.com/docs/en/skills.md
- skill-creator plugin — https://claude.com/plugins/skill-creator
