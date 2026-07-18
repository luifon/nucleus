---
name: obsidian-tweaks
description: Modify the operator's Obsidian vault appearance, filtering, and plugin config. Triggered when the user asks to style/color/hide/configure things in Obsidian — file-explorer folder colors + icons, graph view color groups, "Excluded files" patterns, tab accents, callout/tag styling, plugin data, the Home dashboard. Covers CSS snippets (Style Settings-annotated), `graph.json`, `app.json`/`appearance.json`, the committed plugin set, and the Home.md dashboard. Encodes the Nucleus deep-PARA palette + depth-tier conventions + the read-mostly use model from ADR-014 so iterations stay consistent across sessions.
flavor: recipe
mcp_needed: []
last_used: null
last_failure: null
failure_count_30d: 0
notify_on_failure: []
---

# Obsidian vault tweaks

This skill is the source of truth for modifying the operator's Obsidian vault's appearance and filtering rules. Most Obsidian customization in this stack runs through three config surfaces, all under `~/Documents/Obsidian/.obsidian/`.

## File map

| Surface | File | What it controls |
|---|---|---|
| **CSS snippets** | `~/Documents/Obsidian/.obsidian/snippets/*.css` | File explorer + tab + editor visuals |
| **Graph view** | `~/Documents/Obsidian/.obsidian/graph.json` | Color groups, search filter, force layout |
| **Excluded files** | `~/Documents/Obsidian/.obsidian/app.json` (`userIgnoreFilters`) | Regex-based hiding from graph / search / dim in explorer |
| **Other plugins** | `~/Documents/Obsidian/.obsidian/plugins/<id>/data.json` | Per-plugin config |

Create the `snippets/` directory if it doesn't exist (`mkdir -p`) — Obsidian only creates it on first manual snippet.

## Folder convention (established 2026-05-15, renumbered 2026-05-21)

The vault's top-level folders are 8, renumbered to a rainbow scheme:

| # | Folder | Purpose |
|---|---|---|
| 0 | `0-Inbox` | Capture-now-organize-later landing pad |
| 1 | `1-Main-Notes` | Hub / MOCs / recurring-question answers |
| 2 | `2-Daily-Notes` | Date-stamped journal (`YYYY-MM-DD.md`) |
| 3 | `3-Projects` | Short-term efforts with deadline + outcome (PARA P) |
| 4 | `4-Areas` | Ongoing responsibilities (PARA A) |
| 5 | `5-Resources` | Reference material on topics of interest (PARA R) |
| 6 | `6-Slipbox` | Atomic evergreen notes (Zettelkasten) |
| 7 | `7-Archives` | Inactive items, cold storage (PARA Archives) |

All styling, color-group rules, and bot routing logic key off these exact names. The vault's per-folder `README.md` files are the ground truth for what belongs where.

## Locked rainbow palette

Solid, vibrant rainbow on dark Obsidian background. **Do not lighten/pastel these** unless the user explicitly asks — every text/background pair is WCAG AA verified (≥4.5:1 contrast). Archives is intentionally NOT rainbow — it stays muted italic to signal "cold storage."

| Bucket | BG hex | Text hex | rgb int (for `graph.json`) | Contrast |
|---|---|---|---|---|
| `0-Inbox` | `#c0392b` red | `#ffffff` | `12597547` | 5.9:1 |
| `1-Main-Notes` | `#e67e22` orange | `#1a1a1a` | `15105570` | 6.3:1 |
| `2-Daily-Notes` | `#f1c40f` yellow | `#1a1a1a` | `15844367` | 11.7:1 |
| `3-Projects` | `#1e8449` green | `#ffffff` | `1999945` | 5.5:1 |
| `4-Areas` | `#2471a3` blue | `#ffffff` | `2388387` | 5.4:1 |
| `5-Resources` | `#5b3aa8` indigo | `#ffffff` | `5978792` | 10:1 |
| `6-Slipbox` | `#8e44ad` violet | `#ffffff` | `9323693` | 5.8:1 |
| `7-Archives` | muted (italic, opacity 0.7) | `var(--text-faint)` | `7105123` (graph only) | n/a |

To convert any new hex → Obsidian's `graph.json` rgb integer: `printf "%d\n" 0xRRGGBB` in bash.

## CSS snippet pattern — depth-aware folder coloring

Three visual tiers within depth-having buckets (`1-Main-Notes`, `3-Projects`, `4-Areas`, `5-Resources`):

- **L0** (top folder, e.g., `3-Projects`): **solid** brand-colour background, contrast text, weight 700.
- **L1** (direct children — folders or files one level inside): `rgba(brand, 0.18)` tinted background, lighter shade of brand colour for text on dark, weight 600.
- **L2+** (grandchildren and deeper): no text override, thin `2px` left border at `rgba(brand, 0.4)`, transparent background.

Flat buckets (`0-Inbox`, `2-Daily-Notes`, `6-Slipbox`) only get L0 styling. Archives gets only the muted italic treatment regardless of depth.

Depth selection uses `:has()` (Chromium 105+, current Obsidian is fine):

```css
/* L0 — solid rainbow background */
.nav-folder-title[data-path="3-Projects"] {
  color: #ffffff;
  background: #1e8449;
  font-weight: 700;
  border-radius: 4px;
}

/* L1 — folders and files directly inside 3-Projects */
.nav-folder:has(> .nav-folder-title[data-path="3-Projects"])
  > .nav-folder-children > .nav-folder > .nav-folder-title,
.nav-folder:has(> .nav-folder-title[data-path="3-Projects"])
  > .nav-folder-children > .nav-file   > .nav-file-title { … }

/* L2+ — anything deeper */
.nav-folder:has(> .nav-folder-title[data-path="3-Projects"])
  > .nav-folder-children > .nav-folder > .nav-folder-children
  :is(.nav-folder-title, .nav-file-title) { … }
```

`7-Archives` is styled with italic + opacity 0.7 instead of a coloured background — signals "filed away" and visually demotes it below the rainbow.

Tab underlines use `.workspace-tab-header[data-path^="3-Projects/"] .workspace-tab-header-inner { border-bottom: 2px solid <brand>; }` — one per coloured bucket, none for Archives.

## graph.json shape

```jsonc
{
  "colorGroups": [
    {
      "query": "path:3-Projects",        // search query, NOT regex
      "color": { "a": 1, "rgb": 1999945 } // 24-bit int (R<<16 | G<<8 | B)
    },
    …
  ],
  "search": "",                           // optional graph-only filter
  …
}
```

After editing `graph.json`, the user has to **close and reopen the graph view tab** — the change isn't picked up live.

## userIgnoreFilters (app.json) — hide from graph / search / dim in explorer

```json
{
  "userIgnoreFilters": [
    "(^|/)_"
  ]
}
```

Each entry is a regex matched against the full path. Common patterns:

- `(^|/)_` — anything whose name starts with `_` (templates, meta files like `_original-capture.md`)
- `(^|/)\\.` — dotfiles (rare in vaults — Obsidian normally hides these anyway)
- `(^|/)templates(/|$)` — a specific folder
- `\\.draft\\.md$` — a filename suffix

**Important caveats:**

- **Graph view**: matched files don't appear ✓
- **Search**: dimmed/filtered by default (toggleable in search UI) ✓
- **File explorer**: still shows them, just *greyed out* — Obsidian doesn't fully hide via this setting. Full hiding from the explorer needs the *Hide Folders* community plugin.

After editing `app.json`, the user has to **toggle any setting in the Files-and-links pane** (or restart Obsidian) for it to re-read.

## Reload checklist

| What you changed | How to reload |
|---|---|
| CSS snippet file | Settings → Appearance → CSS snippets → toggle the snippet OFF then ON (or click the refresh icon for new files) |
| `graph.json` | Close + reopen the graph view tab |
| `app.json` (userIgnoreFilters) | Toggle any Settings option / restart Obsidian |
| Plugin data | Disable + re-enable the plugin in Settings → Community plugins |

## Working with this skill

1. **Confirm the exact folder names before writing selectors.** Run `ls -d ~/Documents/Obsidian/[0-9]*` — the vault uses 8 numbered top-level folders (see table above), but it could change.
2. **Don't lighten brand colors without explicit ask.** The deep palette above is what the operator approved after iteration; lightening it triggers re-do.
3. **Minimal-change rule applies here too.** If the user says "fix the child text", touch only the child text — not the background, not the tabs.
4. **Verify with `ls`/`cat` before assuming defaults.** `app.json` is empty (`{}`) by default; `snippets/` may not exist.
5. **One CSS snippet per concern** is fine. The existing `nucleus-para-colors.css` is the canonical PARA snippet; new concerns (callout styling, tag colors, etc.) can live in their own snippet files so toggles stay independent.

## Vault is read-mostly (ADR-014)

The operator's interaction pattern with the vault is **read-mostly**:
writes arrive via the WhatsApp brain-dump pipeline (ADR-005 + ADR-005a),
reads are navigation and glancing. This shapes which customizations
are in scope and which aren't.

### What's installed (and why)

| Plugin | ID | Purpose |
|---|---|---|
| Style Settings | `obsidian-style-settings` | Exposes CSS-snippet variables as UI knobs |
| Dataview | `dataview` | Home dashboard ad-hoc queries; inline queries in any note |
| Calendar | `calendar` (liamcain, pin to stable not beta) | Sidebar month view → daily notes |
| Iconize | `obsidian-icon-folder` (FlorianWoelki) | Per-folder icons in file explorer |

### What's deliberately NOT installed

- **Templater / QuickAdd / Tasks** — capture-oriented. The brain-dump
  pipeline owns this surface. Do not propose installing these unless
  the operator's write pattern changes (i.e., they start typing
  directly into the vault as the primary write path).
- **Marketplace themes** (Minimal, AnuPpuccin, Things, etc.) — conflicts
  with the locked Nucleus aesthetic per [[feedback_design_aesthetics]].
  Stock theme + targeted snippets only.
- **Periodic Notes** — YAGNI; no current weekly/monthly note flow.
- **Excalidraw** — Canvas (core) is already enabled.

### Plugin install convention

Plugins are installed via GitHub releases, not via the Obsidian
Community Plugin UI:

```bash
mkdir -p ~/Documents/Obsidian/.obsidian/plugins/<plugin-id>/
cd ~/Documents/Obsidian/.obsidian/plugins/<plugin-id>/
# Download main.js + manifest.json + styles.css (if present) from the
# tagged GitHub release. For Calendar specifically, pin to the latest
# 1.5.x — the 2.0.0-beta line uses plugin id `calendar-beta` and is
# experimental.
```

The plugin ID in `manifest.json` MUST match the directory name or
Obsidian silently refuses to load it. Then add the ID to
`.obsidian/community-plugins.json` (a JSON array of enabled IDs).

**Obsidian must be quit** while editing `appearance.json`, `app.json`,
`workspace.json`, `community-plugins.json`, or per-plugin `data.json`
— Obsidian rewrites these on quit and will clobber unsaved edits.
CSS-snippet files and vault content (`.md`, `.base`) can be edited
hot; Obsidian re-reads them.

## Iconize folder-icon convention

Iconize's `data.json` has a **two-level shape** — plugin settings nest
under a `settings` key, folder-path → icon mappings are top-level keys.
Putting settings at the top level (without a `settings` wrapper) makes
the plugin's `loadIconFolderData()` throw a TypeError on first read
(`data.settings[k]` where `data.settings` is undefined) — the plugin
silently fails to initialize and no icons render. Verified against
v2.14.7 bundle.

Minimum working data.json:

```json
{
  "settings": {
    "migrated": 6,
    "lucideIconPackType": "native"
  },
  "0-Inbox": "LiInbox",
  "...": "..."
}
```

`migrated` should be the current max migration version Iconize knows
about (6 as of v2.14.7) so it skips legacy-format migrations — safe
for fresh installs. On first load Iconize will flesh out all other
DEFAULT_SETTINGS keys and rewrite the file; you don't need to pre-fill
them. `lucideIconPackType: "native"` is the default but worth pinning.

Lucide icons use the `Li` PascalCase prefix (`inbox` → `LiInbox`,
`calendar-days` → `LiCalendarDays`). Normalization rule: split on
spaces/dashes/underscores, PascalCase each part, join. The seed
mapping from ADR-014:

| Folder | Icon |
|---|---|
| `0-Inbox` | `LiInbox` |
| `1-Main-Notes` | `LiCompass` |
| `2-Daily-Notes` | `LiCalendarDays` |
| `3-Projects` | `LiRocket` |
| `4-Areas` | `LiTarget` |
| `5-Resources` | `LiBookOpen` |
| `6-Slipbox` | `LiSparkles` |
| `7-Archives` | `LiArchive` |

Reversible from the plugin UI (right-click folder → Change icon). If
the operator changes one, update this table; the table is the
intended state.

## Style Settings annotation pattern

New CSS snippets should be annotated with a `@settings` block at the
top so their variables are tunable from the UI without re-editing the
snippet:

```css
/* @settings
name: Snippet display name
id: snippet-id (must match enabledCssSnippets entry)
settings:
    -
        id: variable-name
        title: UI label
        description: What this does
        type: variable-color | variable-number-slider | variable-text
        format: hex | rgb | hex-rgb
        default: '#hex' or numeric
*/

.theme-dark, .theme-light {
  --variable-name: <default>;
}
```

Then reference `var(--variable-name)` in the CSS body. Style Settings
exposes a UI under Settings → Style Settings; changes write to a
separate config and override the snippet defaults at runtime.

## Home dashboard

`Home.md` at vault root is the operator-facing dashboard. It's the
**default pinned tab** (workspace.json + bookmarks.json). Sections
are Dataview queries today — Bases will take over the Projects/Areas
sections once those buckets carry a `status` frontmatter field.

The dashboard is **read-only by convention**. Bots write into the
source folders (`0-Inbox`, `2-Daily-Notes`, `3-Projects`, `4-Areas`);
Home.md just reflects. When adding a new section, prefer a query
over a static link list.

If the PARA folder structure ever changes (renumber, rename, split),
**Home.md and the .base files must be updated in lockstep** — their
queries name folders literally.

## Reload checklist (updated)

| What you changed | Obsidian must be closed? | How to reload |
|---|---|---|
| CSS snippet file | No | Settings → Appearance → CSS snippets → toggle OFF then ON |
| `graph.json` | No | Close + reopen the graph view tab |
| `app.json` (userIgnoreFilters) | Yes (rewritten on quit) | Restart Obsidian |
| `appearance.json` (fonts, snippet enablement) | Yes | Restart Obsidian |
| Plugin `data.json` | Yes (rewritten on quit) | Restart Obsidian, or disable+re-enable plugin |
| `community-plugins.json` (enable/disable) | Yes | Restart Obsidian |
| `workspace.json` | Yes (rewritten constantly) | Restart Obsidian |
| Vault content (`.md`, `.base`) | No | Obsidian re-reads on focus |
