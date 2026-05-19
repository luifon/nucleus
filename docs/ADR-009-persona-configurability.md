# ADR-009 — Persona configurability

**Status:** Proposed (2026-05-18)

## Context

Personas today are hardcoded files at `messaging/<venue>/persona.md` (and
`chat/persona.md` once ADR-012 lands). Each venue has its own dedicated
character; the mapping is baked into the filesystem layout. This worked at
1-2 venues. At 4+ the cognitive load is real — "who handles X again?",
"why is this persona scoped to that venue?", "what changes if I want one
voice across everything?" The memory `persona_growth_concern` flagged
this; the user wants it addressed before ADR-010 (setup wizard) ships,
because the wizard configures personas and shouldn't be forced into the
current one-file-per-venue assumption.

A second issue surfaces alongside: the existing persona files contain
**operator-personal content** (character names, voice references,
sometimes biographical context) and yet they're committed to the repo —
a Rule 2 drift. The fix overlaps with the persona-decoupling work: if
personas relocate to a new location, the new location can be gitignored
by default, with only generic templates shipping in the repo.

Important framing: this ADR is **not** about deleting personas or
forcing unification. It's about making the venue → persona mapping a
config knob the operator drives. The operator can collapse to one
persona across all venues, keep four distinct ones, or anything in
between.

## Decision

Three coupled changes:

1. **Personas relocate** to `personas/<name>.md` at the workspace root.
   Real persona files are **gitignored**; only `.example` templates ship.
2. **Each venue's spawn code reads `NUCLEUS_PERSONA_<VENUE>` from `.env`**
   to know which persona to inject at spawn time. A shared helper in
   `nucleus_core` resolves the env var to a file path, validates
   existence, applies `${USER_NAME}` substitution per Rule 3, returns
   the content for `--append-system-prompt`.
3. **Two persona templates ship in the migration commit**:
   `personas/assistant.md.example` (neutral default) and
   `personas/robot.md.example` (terse, affirmatives-driven). Future
   templates can be added by dropping more `.example` files in the
   directory — no code changes needed.

The existing `WHATSAPP_PERSONA_NAME` env var (currently driving the
reply-signature footer per ADR-005a) is **removed**; the display name
comes from `display_name:` frontmatter in the persona file. Single
source of truth.

The `dev.nucleus.alfred` launchd label — a persona name leaking into a
code identifier, the same Rule 7 drift this ADR addresses at the file
level — is renamed to `dev.nucleus.whatsapp` as part of the same
migration.

## Storage layout

```
personas/
├── README.md                       # explains the system + how to add templates
├── assistant.md.example            # neutral template (committed)
├── robot.md.example                # terse/affirmative template (committed)
├── <slug>.md                       # operator's real persona files (gitignored)
└── ...
```

`.gitignore` gains:

```
# Persona files are operator-personal. Only .example templates are committed.
personas/*.md
!personas/*.md.example
!personas/README.md
```

The old persona files at `messaging/<venue>/persona.md` and
`chat/persona.md` are **deleted** as part of the migration commit.

## Persona file format

YAML frontmatter (one field) + free-form markdown body:

```markdown
---
display_name: ROBOT
---

You are an automated assistant operating on the operator's behalf.

# Voice

Speak in declarative affirmatives ...
```

- `display_name` (optional) — the human-readable name used in the
  reply-signature footer and anywhere else the operator's persona is
  surfaced. Falls back to the slug (the env var value) if absent.
- Body is free-form markdown, appended to the spawn's system prompt via
  `--append-system-prompt`. No required sections — voice characterization
  is editorial, the operator writes what's useful.

Behavioral rules (Rule 6 outbound authorization, vault-writing rules,
etc.) are NOT in persona files — they come from CLAUDE.md, auto-loaded
into every session.

## Shipped templates

### `personas/assistant.md.example`

Neutral default. Direct, helpful, low-ceremony voice. For operators who
want a generic single-persona setup.

### `personas/robot.md.example`

Terse affirmative-driven voice — "Copy.", "Affirmative.", "Anomaly
detected.", declarative-past-tense reports. For operators who prefer an
automated-system feel, or for environments where the bot's response
density matters (long auto-runs, log-shaped output).

Both templates set voice only — behavior comes from CLAUDE.md.

## Config

`.env` gains four new keys (one per conversational venue):

```
NUCLEUS_PERSONA_DISCORD=assistant
NUCLEUS_PERSONA_WHATSAPP=assistant
NUCLEUS_PERSONA_GMAIL=assistant
NUCLEUS_PERSONA_CHAT=assistant
```

`.env.example` ships with all four pointing at `assistant`. Fresh-clone
operators get a single unified voice unless they explicitly diverge.

The operator can:

- **Unify** — keep all four pointing at the same name (collapse to one
  persona)
- **Diverge** — point each at a different name (per-venue personas)
- **Mix** — group some, separate others

Non-conversational surfaces (news-fetcher, distillers, reminders-tick,
news-api) do not have `NUCLEUS_PERSONA_*` env vars — they don't address
a user; they're system jobs.

## Spawn-time resolution

A new helper in `nucleus_core`:

```rust
// core/src/config.rs
pub fn resolve_persona(venue: &str) -> Result<PersonaContent> {
    // 1. Read NUCLEUS_PERSONA_<VENUE_UPPER> from settings
    // 2. Build path: <workspace>/personas/<name>.md
    // 3. Hard error if missing (no silent fallback)
    // 4. Parse frontmatter for display_name (default: <name>)
    // 5. Apply ${USER_NAME} substitution per Rule 3
    // 6. Return { body, display_name }
}

pub struct PersonaContent {
    pub body: String,         // for --append-system-prompt
    pub display_name: String, // for reply footer
}
```

Each venue's spawn code calls `resolve_persona("discord")` (or
"whatsapp", "gmail", "chat") and feeds the result into the
`SpawnOptions`. The TypeScript WhatsApp crate mirrors this in
`messaging/whatsapp/src/persona.ts`.

Missing persona file = hard error with a clear message. No silent
fallback to "default" — silent fallbacks hide configuration errors.

## Migration

The migration commit is **operator-content-free** by design. The
secret-check hook never trips because no `.env` values appear in the
diff.

### What goes in the migration commit

1. Add `personas/README.md` explaining the system
2. Add `personas/assistant.md.example` and `personas/robot.md.example`
   templates
3. Update `.gitignore` to ignore `personas/*.md` except `.example` files
4. Add `nucleus_core::config::resolve_persona()` helper
5. Update each conversational venue's spawn code (Discord, WhatsApp,
   Gmail, chat) to call the helper instead of reading the local
   `messaging/<venue>/persona.md`
6. Remove `WHATSAPP_PERSONA_NAME` from `.env.example` and the
   `formatReply` code that reads it (use frontmatter `display_name`
   instead)
7. Add `NUCLEUS_PERSONA_DISCORD/WHATSAPP/GMAIL/CHAT=assistant` to
   `.env.example`
8. **Delete** `messaging/discord/persona.md`, `messaging/whatsapp/persona.md`,
   `messaging/gmail/persona.md` (and `chat/persona.md` if it exists by
   the time ADR-012 lands first — otherwise this isn't a concern).
   Deletions don't trip the secret hook (only `+` lines are scanned).
9. **Rename** the WhatsApp launchd plist:
   - `tools/launchd/alfred.plist.example` →
     `tools/launchd/whatsapp.plist.example`
   - Inside the file, change the `Label` from `__LAUNCHD_PREFIX__.alfred`
     to `__LAUNCHD_PREFIX__.whatsapp`
   - Add a note to README about `launchctl bootout
     gui/$UID/dev.nucleus.alfred` (removes the old service) before
     re-running `tools/launchd/install.sh`

### What the operator does locally (not committed)

1. `cp personas/assistant.md.example personas/<their-slug>.md` (or
   `robot.md.example`, or a custom one they author from scratch)
2. Edit the new file: paste in voice content from the now-deleted
   `messaging/<venue>/persona.md` files (recoverable from git history:
   `git show HEAD~1:messaging/discord/persona.md`)
3. Edit `.env`: set `NUCLEUS_PERSONA_<VENUE>` to point at their slug(s)
4. Restart the affected bots so the new persona resolution takes effect

### Operator-content recovery

The deleted persona files live in git history. The operator can recover
content from the commit immediately before the migration:

```bash
git show <pre-migration-sha>:messaging/discord/persona.md
git show <pre-migration-sha>:messaging/whatsapp/persona.md
git show <pre-migration-sha>:messaging/gmail/persona.md
```

This is intentional — committing operator-personal content to the repo
was the Rule 2 drift; the migration moves us out of that state. The
history retention is a one-time recovery mechanism, not an ongoing
practice.

## Setup wizard integration (ADR-010 dependency)

The wizard (ADR-010) gains a **persona-setup phase** (new Phase 3, between
service-select and `.env` walkthrough). Two-option flow:

```
? Persona setup
  > One persona, all venues (recommended for most)
    Per-venue personas (more flexibility, more maintenance)

[if "one persona"]
? Pick a starting template
  > assistant — neutral, helpful
    robot     — terse, affirmatives ("Affirmative.", "Copy.")

? Persona slug (lowercase) › robot
? Edit personas/robot.md now? (y/N) ›
[ wizard sets all four NUCLEUS_PERSONA_<VENUE>=robot ]

[if "per-venue"]
[ wizard walks each venue, prompting for slug + template per venue ]
```

Templates available in the multi-choice = whatever `.example` files
exist in `personas/`. Adding a new template to the repo (e.g.,
`personas/concierge.md.example`) makes it appear in the wizard
automatically.

## WhatsApp DM mode — forward-compatible env-var shape

ADR-005b (sibling addendum to ADR-005a) covers the WhatsApp DM mode
work: allowing the operator to converse with the bot in DM, separate
from the group brain-dump pipeline. The persona-config env vars from
this ADR cleanly extend to per-context overrides:

```
NUCLEUS_PERSONA_WHATSAPP=robot                  # default for WhatsApp
NUCLEUS_PERSONA_WHATSAPP_DM=robot               # DM override (optional)
NUCLEUS_PERSONA_WHATSAPP_BRAINDUMP=robot        # brain-dump override (optional)
```

The `resolve_persona` helper signature becomes
`resolve_persona(venue, context: Option<&str>)` once ADR-005b lands. v1
of this ADR ships the venue-only form; ADR-005b adds the context
parameter. Forward-compatible; no schema break.

## Related cleanups NOT in this ADR

These are Rule 7 drifts of the same shape (persona name leaking into
code identifier) but with separate migration concerns:

- **The `--channels alfred` reminder channel name.** Renaming to
  `whatsapp` requires migrating existing rows in `memory/reminders.db`
  (any active reminders using the old channel name). Separate work,
  separate commit. Tracked in CLAUDE.md Rule 10's eventual update.
- **Any other code references to persona names** (`messaging/whatsapp/`
  internal log lines, etc.). Audited during implementation; cleaned up
  in the same commit if trivial, separately if not.

## Out of scope

- Persona "marketplace" or sharing infrastructure — personas are
  operator-personal config, never published
- Mid-session persona swap — operator restarts the bot to pick up a new
  persona
- Persona content authoring tools — the operator uses their editor;
  there is no `nucleus persona create` CLI
- Cross-language persona content — the file is markdown; the operator
  writes in whatever language they prefer; no built-in translation
- Removing personas as a concept — they remain; this ADR just relocates
  and decouples them
- WhatsApp DM mode implementation — that's ADR-005b

## References

- ADR-005a — brain-dump review; the `WHATSAPP_PERSONA_NAME` env var
  this ADR removes was introduced there
- ADR-005b (proposed) — WhatsApp DM mode; depends on this ADR's
  per-context env-var shape
- ADR-010 — setup wizard; depends on this ADR for the persona-selection
  phase
- ADR-012 — canvas; introduces Q persona, which becomes a
  `personas/q.md` under this ADR rather than a hardcoded
  `chat/persona.md`
- CLAUDE.md Rule 1 — secrets in `.env`; persona names live there
- CLAUDE.md Rule 2 — personal state stays gitignored; persona content
  joins the gitignored set
- CLAUDE.md Rule 3 — templates ship, real configs don't;
  `personas/*.md.example` is the template surface
- CLAUDE.md Rule 7 — venue vs persona split; reinforced. Code
  identifiers stay venue-named; persona names live in config
- `persona_growth_concern` (T2 memory) — the warning this ADR addresses
