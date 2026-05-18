# ADR-009 — Persona configurability

**Status:** Placeholder / deferred (2026-05-18)

This ADR is a stub. The need surfaced as personas grew from 1-2 venues to 4+
(Jerry on Discord, the WhatsApp persona, JARVIS on Gmail, Q on dashboard
chat per ADR-012). The `persona_growth_concern` T2 memory captures the
warning; this ADR is the response slot. Must land **before ADR-010**
(setup wizard), because the wizard configures personas and can't be
forced into the current hardcoded-per-venue assumption.

## Problem

Persona files are scattered as `messaging/<venue>/persona.md` (and
`chat/persona.md`), one per venue, hardcoded. The system has no notion of
"operator chooses how many personas they want and how to map them to
venues" — every new venue forces an archetype decision, every consolidation
attempt requires touching files in different folders. Cognitive load grows
linearly with venue count.

Important clarification: this is **not** about deleting personas. Personas
stay. What changes is that the venue → persona mapping becomes a config
knob the operator drives, not a code-organization decision.

## Direction (subject to revision)

- **Personas relocate** to `personas/<name>.md` at the workspace root. One
  file per distinct character voice.
- **Venue spawn code reads** `NUCLEUS_PERSONA_<VENUE>` env var (e.g.,
  `NUCLEUS_PERSONA_DISCORD=assistant`) to look up which persona file to
  `--append-system-prompt`-inject. Per CLAUDE.md Rule 7, the venue's code
  identity stays venue-named; only the persona pointer is configurable.
- **Default ships one persona** named `assistant`, with all four
  `NUCLEUS_PERSONA_<VENUE>` placeholders pointing at it in `.env.example`.
  Fresh-clone operators get a single unified voice unless they explicitly
  diverge.
- **Operator can collapse, expand, or relabel** by editing `.env` plus
  the `personas/` directory contents. No code changes required to add or
  remove a persona.
- **Validation at startup**: a missing `personas/<name>.md` for any
  configured venue is a hard error, not a silent fallback.

## Setup wizard integration (ADR-010)

The wizard gains a "persona setup" phase that asks one upfront question —
*"one persona across all venues, a small set, or per-venue?"* — and
populates the `NUCLEUS_PERSONA_<VENUE>` env vars accordingly. Without
this ADR, the wizard would be forced into per-venue prompts as the only
shape.

## Open design questions for the full ADR

When this is picked up for real spec, decide:

- Directory name (`personas/` vs `characters/` vs `voices/`)
- Default persona name (`assistant` vs operator-supplied)
- Migration shape: merge four current persona files into one, or relocate
  as-is and let operator collapse later
- Whether `WHATSAPP_PERSONA_NAME` (used in the brain-dump ADR's reply
  signature) gets replaced by a `display_name:` frontmatter lookup on the
  persona file, or stays as a parallel env var
- Whether to clean up the `dev.nucleus.alfred` launchd label (Rule 7 drift
  — persona name in code identifier) as part of the same migration, or
  punt to a separate change

## Out of scope

- Removing personas as a concept
- Persona sharing/marketplace
- Mid-session persona swap (operator restarts the bot)
- Anything about *what voice* a persona should have — that's editorial,
  not architectural

## References

- ADR-010 — setup wizard, depends on this
- ADR-012 — canvas, introduces Q (which becomes a `personas/q.md` under
  this ADR rather than a hardcoded `chat/persona.md`)
- CLAUDE.md Rule 1 — secrets in `.env`; persona names live there
- CLAUDE.md Rule 7 — venue ≠ persona split, reinforced
- `persona_growth_concern` (T2 memory) — the warning this ADR addresses
