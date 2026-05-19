# Personas

Per-venue character files the spawned Claude session loads via
`--append-system-prompt`. `.example` templates ship as starting
points; real `personas/<slug>.md` files can be authored locally or
committed — whatever fits the operator's privacy posture.

## How it works

Each conversational venue (Discord, WhatsApp, Gmail, future chat) reads
a `NUCLEUS_PERSONA_<VENUE>` env var from `.env`. The value is a slug
(e.g. `assistant`, `robot`) that resolves to `personas/<slug>.md` at
spawn time. The helper `nucleus_core::config::resolve_persona()`
(`messaging/whatsapp/src/persona.ts` for the TS side) does the lookup,
applies `${USER_NAME}` substitution, and returns:

- `body` — the persona markdown, fed into `--append-system-prompt`
- `display_name` — the human-readable name (frontmatter `display_name`,
  else the slug), used in the WhatsApp reply-signature footer

See ADR-009 for the full spec.

## File format

YAML frontmatter (optional, one field) + markdown body:

```markdown
---
display_name: ROBOT
---

You are an automated assistant operating on ${USER_NAME}'s behalf.

# Voice

...
```

`display_name` is optional. When absent, the slug (the env var value)
is used as the display name. The frontmatter is stripped from the body
before it's fed to the session — it's metadata for the resolver, not
content for Claude.

## Authoring a persona

1. Copy a template: `cp personas/assistant.md.example personas/<slug>.md`
2. Edit the body. Voice characterization only — behavioral rules
   (Rule 6 outbound authorization, vault-writing conventions, etc.)
   come from `CLAUDE.md`, which is auto-loaded into every session.
3. Set the venue's env var in `.env`:
   - `NUCLEUS_PERSONA_DISCORD=<slug>`
   - `NUCLEUS_PERSONA_WHATSAPP=<slug>`
   - `NUCLEUS_PERSONA_GMAIL=<slug>`
4. Restart the affected bot(s) so the new persona resolves.

The same slug can be reused across venues for a unified voice, or each
venue can point at its own.

## Shipped templates

- `assistant.md.example` — neutral default. Direct, helpful, low
  ceremony. Reasonable starting point for any venue.
- `robot.md.example` — terse affirmative-driven voice ("Copy.",
  "Affirmative.", "Anomaly detected."). For operators who prefer an
  automated-system feel.

Add more templates by dropping a `personas/<name>.md.example` file in
this directory — no code changes needed. The future setup wizard
(ADR-010) discovers templates by glob.

## Per-context overrides (ADR-005b)

After ADR-005b lands, WhatsApp gains DM mode and the env-var shape
extends to per-context overrides:

```
NUCLEUS_PERSONA_WHATSAPP=assistant         # venue default
NUCLEUS_PERSONA_WHATSAPP_DM=robot          # optional override for DMs
NUCLEUS_PERSONA_WHATSAPP_BRAINDUMP=...     # optional override for brain-dump group
```

Resolution order: more-specific key first, then venue default, then
hard error. The override knobs are optional — leaving them unset keeps
the venue default everywhere.
