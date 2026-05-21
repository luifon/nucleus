---
name: obsidian-tweaks
description: Modify the operator's Obsidian vault appearance and filtering. Triggered when the user asks to style/color/hide things in Obsidian — file-explorer folder colors, graph view color groups, "Excluded files" patterns, tab accents, callout/tag styling. Covers CSS snippets, `graph.json` color groups, and `app.json` `userIgnoreFilters`. Encodes the Nucleus deep-PARA palette and depth-tier conventions so iterations stay consistent across sessions.
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
