# ADR-017 — Skill-gap learner

**Status:** Accepted (2026-05-24) — Implemented (2026-05-24). Arm 1 (the
on-the-fly review) superseded by [[ADR-038]] (2026-11-03): a nightly
transcript sweep files skill suggestions for the operator instead. Arm 2
changed by ADR-038 (2026-11-07): the gap pass and the curator file
suggestions too, and no pass writes skills; the "Autonomy + the validation
gate" section below describes the removed write path. The pure stale-skill
archive is unchanged.

**Builds on:**
- [[ADR-008]] — skills are procedural memory in SKILL.md; this adds an agent
  that writes them autonomously.
- [[ADR-016]] — the reserved `skill-gap-learner` registry slot, the run-log
  (the on-the-fly arm's substrate), and the two-layer model (capabilities vs
  maintenance agents).
- Supersedes the sunset preference-learner (ADR-016) with an all-facets version.

## Context

The operator wanted Nucleus to learn skills **at runtime the way Hermes does**:
Hermes' standout behavior is a background skill-review that fires during normal
operation, watches the conversation for skill-worthy signals (corrections,
frustration, new techniques, a loaded skill that was wrong), and autonomously
creates/patches skills — with a periodic curator consolidating the library.

**The mapping problem.** Hermes *is* its agent loop: it counts tool iterations
and forks a review in-process with the live conversation. Nucleus drives
`claude` as a tmux CLI and only sees `ask()` boundaries + the transcript JSONL
(which already records the full conversation incl. tool_use). So the faithful
port is a **detached one-shot reviewer session that reads the transcript**, not
an in-process fork. Hermes' two prompts (`SKILL_REVIEW_PROMPT`,
`CURATOR_REVIEW_PROMPT`) were read at the source and ported.

## Decision

A `chores/skill-gap-learner` crate with two arms sharing one review engine.

### Arm 1 — on-the-fly (the runtime behavior) — superseded by ADR-038

A capability (`skill_review`) on the conversational agents (discord, whatsapp,
chat). `SessionPool` counts asks per `chat_key`; crossing
`[skill_learner].nudge_interval` flags `AskResult.review_due`, and the handler
fires a **detached** `skill-gap-learner review --transcript <path> --venue <v>
--chat-key <k>` after replying — fire-and-forget, never blocking or failing the
reply. The reviewer is a one-shot Session that reads the transcript, runs the
ported `SKILL_REVIEW_PROMPT`, and **writes/patches skills itself** via its file
tools.

### Arm 2 — periodic (the curator)

The `skill-gap-learner` maintenance agent (launchd-cron daily). `learn` runs:
1. **auto-transitions** (pure, no LLM): archive agent-created, unpinned skills
   idle past `archive_after_days` (activity = max(last_used, mtime)); count the
   merely-stale. Never touches hand-written (`created_by != agent`) or pinned.
2. **gap detection**: read the last 7 days of every agent's diary, propose +
   create skills for recurring patterns that lack one. Since 2026-11-07
   (ADR-038) it files `create`/`patch` suggestions (origin `gap`) instead
   of creating skills.
3. **curate**: the ported `CURATOR_REVIEW_PROMPT` — consolidate agent-created
   skills into class-level umbrellas, **archive-never-delete** to `.archive/`.
   Since 2026-11-07 (ADR-038) a consolidation is a patch (or create)
   suggestion for the umbrella with an explicit `retires` list; accept
   moves only agent-created, unpinned retired skills to `.archive/`.

### Autonomy + the validation gate (removed 2026-11-07)

Superseded by ADR-038: every pass files suggestions, the operator decides,
and the gate below no longer exists. Kept as the record of the first
design.

Skills are written **autonomously** (Hermes-faithful) to the operator-personal
tree only (gitignored — Rule 1; never the committed tree). Since ADR-032 that
tree is `.nucleus/.claude/skills/` (resolved by
`nucleus_core::skills::personal_skills_root`); it was `~/.claude/skills/` when
this ADR was written. Written skills carry `flavor: learned` and
`created_by: agent`. Oversight is the curator
(archive-never-delete), the `/skills` dashboard, and — the key safeguard —
a **validation gate**: every touched SKILL.md is re-parsed by
`nucleus_core::skills::validate` (required frontmatter + the required
`# When to invoke` / `# Steps` / `# Failure modes` sections, Rule 11). A
malformed write is quarantined to `.rejected/` rather than landing live. This
makes direct writes as reliable as skill-creator — enforced mechanically, not
just by prompt.

### Shared infrastructure

- `nucleus_core::skills` — discovery, strict→lenient frontmatter parse,
  `validate()`, and `fire_skill_review()` (removed by ADR-038). Shared with the dashboard `/skills`
  handler so both read skills identically.
- `[skill_learner]` config: `nudge_interval` (12, removed by ADR-038), `cron`, `stale_after_days`
  (30), `archive_after_days` (90), `enabled` — all serde-defaulted.
- Frontmatter-based staleness (`created_by`, `pinned`, `last_used`),
  markdown-canonical — no `.usage.json` sidecar (ADR-004 footgun).

## Verified

- On-the-fly: a transcript with a workflow correction + a terseness preference
  produced a clean, validated `rust-service-deploy` skill (run-log ok=true).
- Periodic: a real `learn` left hand-written skills untouched, created a valid
  `calendar-invite` skill from the actual reminders-calendar deferral pattern,
  ran the curator, and passed the gate (gap + curate run-logged ok=true).

## Future work

- Persona auto-evolution (ADR-004's "SOUL slot") as reviewable suggestions —
  the learner is the right home; not built yet.
- Tool-iteration-aware nudging — done differently by ADR-038: the pre-filter
  counts tool calls, failed calls retried successfully and corrections in
  each transcript part.
- Usage counters / pinning surfaced + editable from `/agents` or `/skills`.

## Out of scope

- An interactive skill-authoring UI — `/skill-creator` remains the operator's
  manual path; this is the autonomous complement.
- Touching the committed `.claude/skills/` tree — accepted suggestions are
  written only to the operator-personal tree (`.nucleus/.claude/skills/`,
  ADR-032).
