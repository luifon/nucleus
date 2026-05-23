# ADR-014 — Obsidian vault customization for read-mostly operator use

**Status:** Accepted (2026-05-23) — Implemented (2026-05-23)

## Context

The Obsidian vault at `~/Documents/Obsidian/` is the T3 surface in
the tiered memory model (ADR-002, ADR-005). Today its appearance is
near-stock — default theme, default fonts, no community plugins, a
single CSS snippet (`nucleus-para-colors.css`) implementing the
rainbow PARA palette from the [[obsidian-tweaks]] skill, and a stale
`Jane Doe Workspace.md` static bio file from the Hermes era left over as
the default-open file.

The operator's **interaction pattern with the vault** is read-mostly:

- **Writes**: predominantly arrive via the WhatsApp brain-dump
  pipeline (ADR-005 + ADR-005a). The operator dictates or types into
  WhatsApp; the bot plans multi-op vault writes, asks for approval,
  applies them.
- **Reads**: navigation, glancing, occasional manual nudge (a small
  edit, a moved link). The operator opens the vault to *see what's
  there* — what the bots have been writing, what's currently active,
  what the recent shape of things is.

This asymmetry should drive every customization choice. The default
power-user Obsidian-customization advice (Templater, QuickAdd, capture
shortcuts, dataview-driven todo flows) is **capture-oriented** —
optimizing the friction-to-new-note path. None of that buys the
operator anything; the capture path is already covered by a
fundamentally better surface (the bot, on the phone). What the vault
*needs* is the opposite: a configuration tuned for **seeing the bot's
output** quickly and pleasantly.

A second framing concern: the vault's visual identity should not
drift away from the Nucleus stack as a whole. The Nucleus UI surfaces
have a locked aesthetic — JetBrains Mono, near-black with amber
accent, terminal-leaning (memory: [[nucleus_visual_design]]). Stock
Obsidian's UI (Inter + a different blue accent) reads as a
separate app. Closing that gap with targeted snippet work — without
adopting a marketplace theme — keeps the vault visually continuous
with the rest of Nucleus while preserving the read-mostly,
distraction-free editor surface Obsidian is good at.

A locked rainbow PARA palette already exists for folder coloring
(`nucleus-para-colors.css`, ADR-008-skills-adjacent [[obsidian-tweaks]]
skill encodes the per-bucket hex values). This ADR layers on top of
that palette, not replacing it.

## Decision

Customize the vault in four axes — **font/identity**, **dashboard**,
**plugin set**, **folder icons** — sized to the read-mostly use case.
Skip the capture-side power-user plugins (Templater, QuickAdd, Tasks)
on principle: that pipeline is the bot's job, not the operator's.

### 1. Font + identity polish

- **Editor + interface font**: switch to **JetBrains Mono** (matching
  the Nucleus UI). Single-font setup (mono for editor, UI, monospace
  blocks) — terminal feel, no font-stack inconsistency.
- **Nucleus polish CSS snippet** (`nucleus-polish.css`) shipping
  alongside the existing `nucleus-para-colors.css`:
  - **Amber accent** (`#e6b450`, matching the Nucleus signature) on
    the active tab, active line gutter, and selection highlight —
    one consistent accent color across the editor.
  - **Tightened heading scale** — Obsidian's defaults are blog-shaped
    (large h1, bouncy h2). Knot it down by ~15% to feel more like a
    working surface, less like a publishing target.
  - **Hairline inline code** — replace the default grey-blob `code`
    background with a 1px border + transparent background. Mono font,
    same readability, less visual noise.
  - **Callout palette align** — recolor the standard Obsidian
    callouts (note/tip/warning/info/success/danger) to draw from the
    rainbow PARA palette instead of stock blue/yellow/red — so a
    `> [!info]` matches the indigo of Resources, a `> [!success]`
    matches the green of Projects, etc. Existing callout syntax keeps
    working; only colors change.

Both snippets are **Style Settings**–annotated (see #3) so the
palette and accent can be tuned from the Obsidian UI rather than by
editing CSS.

### 2. Live home dashboard

Replace the stale `Jane Doe Workspace.md` with a real dashboard:

- Create `Home.md` at vault root with this shape:
  ```
  ## Today              — link to today's daily note (create-if-missing)
  ## Recent inbox       — last N files in 0-Inbox/ by mtime
  ## Active projects    — Bases view over 3-Projects/ filtered status=active
  ## Areas              — flat link list of 4-Areas/ children
  ## Recently touched   — top 10 files across vault by mtime
  ```
- Set `Home.md` as the default file to open on Obsidian startup
  (via `app.json` → `defaultFileOpenInNewTab` / workspace pin).
- Old `Jane Doe Workspace.md` moves to `7-Archives/2026-05-23-operator-workspace.md`
  with a header note recording why (kept for historical reference, not
  for active use).

Dashboard implementation mixes two surfaces:

- **Bases** (core, already enabled) — for the structured PARA queries
  (Active projects, Areas). Backed by frontmatter status fields. Saved
  as `Home-projects.base` / `Home-areas.base` and embedded in
  `Home.md`. Native Obsidian, no plugin dependency.
- **Dataview** (community plugin) — for the ad-hoc queries (Recent
  inbox, Recently touched, Today's daily-note link). More flexible
  for mtime-sorted lists than Bases's frontmatter-required model.

Both are read-only views. Operator never edits the dashboard; bots
write into the source folders, dashboard reflects.

### 3. Community plugin set (curated, minimal)

Four community plugins, ordered by leverage:

| Plugin | ID | Purpose |
|---|---|---|
| Style Settings | `obsidian-style-settings` | Exposes CSS-snippet variables as UI knobs. Multiplier for all future snippet work. |
| Dataview | `dataview` | Dashboard ad-hoc queries; inline-query support in any note. |
| Calendar | `calendar` (liamcain) | Sidebar month view, click-to-navigate daily notes. Trivial install, high read-time payoff. |
| Iconize | `obsidian-icon-folder` (FlorianWoelki) | Per-folder icons in file explorer. Reduces reliance on the numeric prefix for at-a-glance bucket recognition. |

**Explicitly not installed**:

- **Templater, QuickAdd, Tasks** — capture-oriented; the brain-dump
  pipeline already covers this surface better.
- **Periodic Notes** — no current need for weekly/monthly notes. YAGNI.
- **Excalidraw** — Canvas (core) is already enabled and covers the
  common diagram case.
- **Any marketplace theme** (Minimal, AnuPpuccin, Things, etc.) —
  conflicts with the locked Nucleus aesthetic per
  [[feedback_design_aesthetics]]. Stock theme + targeted snippets
  stays truer.

### 4. Iconize folder icons (opinionated)

Lucide icons (Obsidian's bundled set) assigned per bucket. Pairs with
the rainbow palette — color + glyph reinforces recognition:

| # | Folder | Icon (Lucide name) |
|---|---|---|
| 0 | `0-Inbox` | `lucide-inbox` |
| 1 | `1-Main-Notes` | `lucide-compass` |
| 2 | `2-Daily-Notes` | `lucide-calendar-days` |
| 3 | `3-Projects` | `lucide-rocket` |
| 4 | `4-Areas` | `lucide-target` |
| 5 | `5-Resources` | `lucide-book-open` |
| 6 | `6-Slipbox` | `lucide-sparkles` |
| 7 | `7-Archives` | `lucide-archive` |

All reversible from the Iconize UI later; this is the seed
configuration, not the locked palette.

## Out of scope (explicitly)

- **Capture flow optimization** (Templater/QuickAdd/Tasks). The
  brain-dump pipeline owns this. Revisit only if the operator's
  pattern shifts to direct vault editing.
- **Mobile-specific layouts.** The vault is desktop-primary; mobile
  is a glance-only surface and stock Obsidian mobile is fine.
- **Theme adoption.** Snippet work only.
- **Folder structure changes.** Locked per ADR-005 rainbow renumber
  (2026-05-21).
- **Sync configuration.** Obsidian Sync is already enabled
  (`core-plugins.json`) and outside this ADR's scope.

## Implementation steps

Sequential, executed once (Obsidian must be closed for the JSON
config edits in steps 3 and 6):

1. **Write `Home.md`** at vault root with Bases + Dataview queries
   inline; move existing `Jane Doe Workspace.md` → `7-Archives/`.
2. **Create `Home-projects.base` and `Home-areas.base`** files for
   the Bases views embedded in Home.md.
3. **Edit `appearance.json`** — set `textFontFamily`,
   `interfaceFontFamily`, `monospaceFontFamily` to `JetBrains Mono`;
   add `nucleus-polish` to `enabledCssSnippets`.
4. **Write `nucleus-polish.css`** to `.obsidian/snippets/` with
   amber-accent / heading / inline-code / callout rules, all
   Style-Settings-annotated.
5. **Install plugins** by downloading latest release from each
   plugin's GitHub repo into
   `.obsidian/plugins/<id>/{manifest.json,main.js,styles.css}` and
   appending the IDs to `.obsidian/community-plugins.json`.
6. **Edit `app.json`** — set `defaultViewMode`, attachment defaults,
   and any plugin-specific gating that needs to be on at first
   plugin-load.
7. **Seed Iconize config** at
   `.obsidian/plugins/obsidian-icon-folder/data.json` with the
   per-folder mapping in §4.
8. **Restart Obsidian.** Verify each plugin loaded, dashboard
   renders, fonts switched, icons appear, snippet active. Iterate on
   the snippet's Style Settings knobs to land the amber accent
   exactly.

## Update the obsidian-tweaks skill

The skill at `.claude/skills/obsidian-tweaks/SKILL.md` currently
documents the CSS-snippet + graph.json + userIgnoreFilters surfaces.
Post-implementation, extend it to also cover:

- The plugin set (and *which plugins are deliberately omitted*, so
  future Claude sessions don't propose adding Templater/QuickAdd
  again),
- The Iconize per-folder icon convention,
- The Style Settings annotation pattern for new snippets,
- The Home dashboard structure (so changes to PARA structure cascade
  into the dashboard queries).

Skill remains the *how-to* (procedural, per ADR-008); this ADR
remains the *why* (architectural decision).

## Future iteration

If the operator's pattern shifts toward manual capture (the bot
becomes inadequate, or a niche flow emerges that's faster typed than
spoken), revisit Templater/QuickAdd then. Don't pre-install in
anticipation.

If the rainbow palette tweaks land repeatedly in the same direction,
codify the new values back into the [[obsidian-tweaks]] skill's
"Locked rainbow palette" table — Style Settings makes iteration
cheap, but the locked values are the ground truth that all bot-written
notes assume.
