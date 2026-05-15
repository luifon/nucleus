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

## PARA folder convention (established 2026-05-15)

The vault's top-level folders are: `0-Inbox`, `1-Projects`, `2-Areas`, `3-Resources`, `4-Archives` (note: **Archives** plural, not Archive). All styling and color-group rules key off these exact names.

## Locked colour palette

Deep, saturated colours — readable on tinted backgrounds. **Do not lighten/pastel these** unless the user explicitly asks for "lighter" — the deep palette is what the operator has approved after iteration.

| Bucket | Hex | rgb int (for `graph.json`) | Vibe |
|---|---|---|---|
| `0-Inbox` | `#8a7438` | `9073720` | deep gold — "fresh / unsorted" |
| `1-Projects` | `#c47020` | `12873760` | burnt amber — active work |
| `2-Areas` | `#4d8c2a` | `5082154` | deep grass — ongoing responsibilities |
| `3-Resources` | `#1f8275` | `2065013` | deep teal — reference library |
| `4-Archives` | `#6c6a63` | `7105123` | muted grey — done / cold |

To convert any new hex → Obsidian's `graph.json` rgb integer: `printf "%d\n" 0xRRGGBB` in bash.

## CSS snippet pattern — depth-aware folder coloring

Three visual tiers per PARA bucket — established and approved in the snippet `nucleus-para-colors.css`:

- **L0** (top folder, e.g., `1-Projects`): deep brand colour text, weight 700, `rgba(brand, 0.22)` background.
- **L1** (direct children — folders or files one level inside): same deep colour text, weight 600, `rgba(brand, 0.12)` background.
- **L2+** (grandchildren and deeper): no text override (uses Obsidian default), thin `2px` left border at `rgba(brand, 0.4)`, transparent background.

Depth selection uses `:has()` (Chromium 105+, current Obsidian is fine):

```css
/* L0 */
.nav-folder-title[data-path="1-Projects"] { … }

/* L1 — folders and files directly inside 1-Projects */
.nav-folder:has(> .nav-folder-title[data-path="1-Projects"])
  > .nav-folder-children > .nav-folder > .nav-folder-title,
.nav-folder:has(> .nav-folder-title[data-path="1-Projects"])
  > .nav-folder-children > .nav-file   > .nav-file-title { … }

/* L2+ — anything deeper */
.nav-folder:has(> .nav-folder-title[data-path="1-Projects"])
  > .nav-folder-children > .nav-folder > .nav-folder-children
  :is(.nav-folder-title, .nav-file-title) { … }
```

`Archives` is styled with italic + opacity 0.7 instead of a coloured background — keeps it distinct from `Inbox` so the two transient buckets don't blur.

Tab underlines use `.workspace-tab-header[data-path^="1-Projects/"] .workspace-tab-header-inner { border-bottom: 2px solid <brand>; }`.

## graph.json shape

```jsonc
{
  "colorGroups": [
    {
      "query": "path:1-Projects",        // search query, NOT regex
      "color": { "a": 1, "rgb": 12873760 } // 24-bit int (R<<16 | G<<8 | B)
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

1. **Confirm the exact folder names before writing selectors.** Run `ls -d ~/Documents/Obsidian/[0-9]*` — the vault uses `4-Archives` plural, but it could change.
2. **Don't lighten brand colors without explicit ask.** The deep palette above is what the operator approved after iteration; lightening it triggers re-do.
3. **Minimal-change rule applies here too.** If the user says "fix the child text", touch only the child text — not the background, not the tabs.
4. **Verify with `ls`/`cat` before assuming defaults.** `app.json` is empty (`{}`) by default; `snippets/` may not exist.
5. **One CSS snippet per concern** is fine. The existing `nucleus-para-colors.css` is the canonical PARA snippet; new concerns (callout styling, tag colors, etc.) can live in their own snippet files so toggles stay independent.
