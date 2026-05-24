# ADR-015 — Nucleus-dashboard: single operator app subsuming dashboard, chat, news, and admin surfaces

**Status:** Accepted (2026-05-23) — Implemented (2026-05-24)

> **Updated by [[ADR-016]] (2026-05-24).** The `/sessions` surface described
> below was built then **deleted** — it's superseded by `/agents` (the
> registry-backed front door). The `/sessions` references in the surface and
> route tables are historical; the tmux copy-attach affordance now lives on
> each `/agents` tile, and the `/` landing's deferred agents-health tile
> shipped with ADR-016.

**Supersedes (in part):**
- [[ADR-001]] — the "one crate per operator surface" topology.
  News-fetcher (the cron worker) and the messaging bots remain
  separate; only the *operator-facing HTTP surfaces* consolidate.

**Reframes:**
- [[ADR-011]] (Tailscale perimeter) — gating becomes path-scoped on a
  single origin instead of per-tunnel. Sequencing changes:
  nucleus-dashboard ships first, Tailscale gates its operator paths
  second.
- [[ADR-012]] (canvas) — chat-v2 is no longer a new binary. Canvas
  becomes a feature *inside nucleus-dashboard's chat surface*. The
  "parallel rollout via `$NUCLEUS_CHAT_V2_PUBLIC_URL`" mechanic in
  ADR-012 collapses into nucleus-dashboard's own (short-lived)
  parallel rollout.

## Context

Nucleus operator surfaces today are three separate crates served at
three URLs through three cloudflared routes:

- `dashboard/` — observability tiles (services, containers, reminders,
  news, vault writes). Read-only.
- `chat/` — interactive Claude chat with persona Q (once ADR-012 lands).
- `news/api/` — read-only RSS-shape news API. Public by design (ADR-001).

Plus a growing list of subsystems whose state is **invisible to the
operator**:

- **Sessions** (tmux-hosted long-lived Claude sessions, Rule 4) — daily
  04:00 rotation lands silently; no UI to see what's running, what
  rotated when, what's idle.
- **Skills** (ADR-008) — operator-personal at `~/.claude/skills/`,
  developer at `.claude/skills/`. No surface lists them, shows
  `last_used`, or surfaces `failure_count_30d` / `notify_on_failure`.
  The "skill quietly broke three weeks ago" failure mode is real.
- **Reminders** (ADR-006) — manageable from the `reminders` CLI; the
  dashboard already shows the next-N widget, but pause/resume/cancel
  still requires shell.
- **Diary + distillation** (ADR-004) — bots write diaries; nothing
  surfaces them.
- **Daily routines** — reminder-fire-with-`--system-prompt` flows
  (ADR-008 §"Skill-fire reminders"), scheduled news fetches,
  preference learner, gmail metabolism. Each is invisible until
  something breaks.
- **Vault writes** — brain-dump pipeline applies multi-op plans; the
  outcome is a WhatsApp message and an `mtime` change, no consolidated
  feed.

Hermes ([[hermes_dormant]]) solved this by collapsing every surface
into one React SPA. The trade-off there was heavy: Vite + React 19 +
Tailwind + xterm + three.js + shadcn-style components + `@nous-research/ui`
design system, ~7,700 lines of TS across 12 pages, full
configuration-as-UI (`ConfigPage`, `EnvPage`) that conflicts with
Nucleus's config-as-files discipline.

The shape was right. The visibility was right. The aesthetic
(SaaS/shadcn) and the scope (config-as-UI) were wrong for Nucleus.

This ADR adopts the **shape** — one app, every surface as a tab — while
holding the line on **aesthetic** (Nucleus terminal × tiles, per
[[nucleus_visual_design]] and [[feedback_design_aesthetics]]) and
**configuration discipline** (`.env` + `nucleus.toml` + persona/skill
files stay the source of truth — the unified app *visualizes* them
but does not edit them, with the narrow exception of reminder
pause/resume which is already CLI-driven).

A second concern that resolves naturally: cross-surface aesthetic
drift. Dashboard, chat, and news today each render in subtly
different HTML/CSS because each crate hand-rolls its own. One app
means one design system, one component vocabulary, one type ramp.

## Decision

Build **`nucleus-dashboard/`** — a new workspace member that
consolidates every operator-facing HTTP surface into one binary
(`nucleus-dashboard`), served at a single origin (`web.northmark.tech`).

### Stack

- **Backend**: axum (Rust). One binary, `nucleus-dashboard`. Serves
  the SPA shell + all JSON APIs + the chat WebSocket + the news
  public API.
- **Frontend**: React 19 + Vite + TypeScript + Tailwind CSS v4.
  Components are **hand-rolled** (no shadcn CLI, no off-the-shelf
  component library, no marketplace theme). The component layer is
  written from scratch in the Nucleus aesthetic: JBM mono, near-black
  backgrounds, amber accent, tile/terminal vibe with ASCII flourishes
  (`[nucleus]`, `┌──`, status pills in bracketed uppercase).
- **No design-system dependency.** Specifically not `shadcn/ui` (CLI
  or otherwise), not `radix-ui`, not Headless UI, and not
  `@nous-research/ui` (Hermes' source). Tailwind is used as a
  utility layer, not as a license to render the Vercel/Linear look.
- **Build**: `npm run build` in `nucleus-dashboard/web/` outputs to
  `nucleus-dashboard/api/dist/` (gitignored). axum embeds the
  static assets via `rust-embed` (or similar) at compile time, or
  serves from disk in dev. `cargo build --release` triggers `npm run
  build` via a `build.rs` script.

This is an explicit aesthetic-discipline departure for the Nucleus
stack: we accept the React + Vite + Tailwind toolchain (Node, npm,
Vite dev server) in exchange for the visibility and consolidation
wins. The persona-archetype + visual-design memories remain the
guardrails — when designs trend SaaS-shaped during implementation,
revert.

### Scope (everything operator-facing including news)

| Surface | Status today | Role in nucleus-dashboard |
|---|---|---|
| Dashboard tiles | `dashboard/` crate | `/` — landing page, same widgets, redesigned in unified vocabulary |
| Chat | `chat/` crate | `/chat` — same Claude session shape, ready for canvas (ADR-012) |
| News public | `news/api/` crate | `/news/api/*` — same public read-only API contract; whitelisted from Tailscale gating |
| News admin | none today (toml-only) | `/news` — sources, scoring config viewer, recent fetch runs |
| Sessions | tmux-only | `/sessions` — list of `nucleus-*` tmux sessions, windows, last-activity, rotation history; "attach" deep-link via tmux command copy-to-clipboard |
| Skills | filesystem-only | `/skills` — list of operator + developer skills with frontmatter (description, flavor, `last_used`, `failure_count_30d`, `notify_on_failure`, source path). Missing fields render `—` |
| Reminders | dashboard widget + CLI | `/reminders` — full admin surface: list, filter, fire history, pause/resume/cancel (wraps CLI); read-only view of seed reminders |
| Diary + distillation | filesystem | `/diary` — per-agent diary entries, latest distillation outputs (per ADR-004) |
| Vault writes feed | filesystem mtime | `/vault` — chronological feed of brain-dump applies (file, op kind, source persona), backed by an audit log if one exists or filesystem mtime scan if not |
| Cron / routines | launchd + reminders | `/cron` — read-only aggregation: launchd plists, upcoming reminder fires, recent fire history |

News stays in scope despite being public because:
- Keeping the contract identical (`/news/api/*`, JSON shape) means
  downstream subscribers don't break;
- Tailscale gating becomes path-scoped (`/news/api/*` public through
  cloudflared, everything else Tailscale-only) — the more complex
  perimeter choice, accepted in exchange for one app instead of two;
- The news *admin* view (sources, scoring) and the news *public* read
  surface naturally live in one app once they share a database
  (which they already do at `memory/news.db`).

### Aesthetic guardrails

This is a React app — historically associated with the SaaS look
the operator has vetoed. Discipline rules baked into the design
review process:

1. **Hand-roll components.** No `shadcn` CLI, no `radix-ui` UI
   primitives library, no Headless UI, no Mantine, no Chakra.
   Components live in `nucleus-dashboard/web/src/components/`
   written from scratch — `<StatusTile>`, `<Banner>`, `<RouteShell>`,
   `<CommandBracket>`, etc. `lucide-react` is acceptable for icons
   (matches the Nucleus + Obsidian Lucide convention from ADR-014).
2. **JBM mono everywhere.** Single font, no UI/text/mono split. Same
   font in the editor (ADR-014), the dashboard widgets, the chat
   transcript.
3. **Color palette is locked.** Near-black `#0a0a0a` background, amber
   accent `#e6b450` (matches ADR-014's polish snippet), text in
   `var(--text-normal)` / faint variants. Status colors borrow from
   the locked PARA rainbow (ADR-014, `obsidian-tweaks` skill table).
4. **Terminal-flavor flourishes** stay. ASCII brackets (`[nucleus]`,
   `┌──`, `▸`), bracketed-uppercase status pills (`[OK]`, `[DOWN]`,
   `[FIRING]`), monospace tabular numerals.
5. **No motion library.** No GSAP, no Framer Motion, no React Spring.
   CSS transitions only, and sparingly.
6. **No 3D.** Hermes ships three.js + @react-three/fiber. Nucleus does
   not. If a surface ever genuinely needs a graph (session timeline),
   use Observable Plot or just SVG.

If a PR makes nucleus-dashboard look like Linear/Vercel/Notion/
Obsidian-default, that PR gets rejected. The aesthetic memos
([[feedback_design_aesthetics]], [[nucleus_visual_design]]) remain
the ground truth.

### Configuration discipline (unchanged)

Nucleus-dashboard **visualizes** config; it does not edit it. The
Hermes ConfigPage/EnvPage pattern is explicitly out.

- `.env` — read-only from the dashboard (display masked, no edit form).
- `nucleus.toml` — read-only.
- Persona files at `personas/*.md` (ADR-009) — viewable, not editable.
- Skill files — viewable (full body), not editable.

Narrow exceptions where the dashboard writes are explicitly fine
because they're already CLI-driven and the CLI just wraps a DB
write:

- Reminder pause / resume / cancel (existing `reminders` CLI).
- Marking a diary entry as read (if we add that).
- Acknowledging a failing skill (clearing `failure_count_30d`).

Anything else editorial — change a persona, change a skill body,
change news scoring weights — happens by editing the file in
`$EDITOR`. The dashboard links out (or shows the file path) but does
not provide a form.

## Architecture

### Workspace shape

```
nucleus/
├── nucleus-dashboard/                NEW
│   ├── api/                          ← Rust workspace member
│   │   ├── Cargo.toml                  [package] name = "nucleus-dashboard"
│   │   ├── build.rs                    invokes `npm run build` in ../web/
│   │   └── src/
│   │       ├── main.rs
│   │       ├── handlers/
│   │       │   ├── sessions.rs
│   │       │   ├── skills.rs
│   │       │   ├── reminders.rs
│   │       │   ├── diary.rs
│   │       │   ├── vault.rs
│   │       │   ├── news.rs           public + admin
│   │       │   ├── chat.rs           ws + session lifecycle
│   │       │   └── ...
│   │       └── collectors/           lifted from dashboard/
│   ├── web/                          ← React + Vite + Tailwind, not a Cargo member
│   │   ├── package.json
│   │   ├── vite.config.ts
│   │   ├── tailwind.config.ts
│   │   ├── index.html
│   │   └── src/
│   │       ├── App.tsx
│   │       ├── pages/
│   │       ├── components/
│   │       ├── lib/api.ts            typed fetch wrappers
│   │       └── theme/                CSS variables, Tailwind tokens
│   └── api/dist/                     gitignored; embed target for axum
├── chat/                             [DELETED at sunset]
├── dashboard/                        [DELETED at sunset]
├── news/api/                         [DELETED at sunset]
├── news/fetcher/                     KEPT — cron worker, separate concern
└── messaging/, chores/, core/        unchanged
```

Workspace `members` array gains `"nucleus-dashboard/api"`, loses
`"chat"`, `"dashboard"`, `"news/api"` at sunset. `news/fetcher` stays
a workspace member because it runs as its own launchd-driven binary,
separate from the HTTP surface.

The folder is named `nucleus-dashboard/` (not `nucleus-web/`) so the
inner `web/` subfolder doesn't read as "web-web". The Cargo
workspace member lives at `nucleus-dashboard/api/`; its
`[package] name = "nucleus-dashboard"` is what `cargo build`
produces. The React app at `nucleus-dashboard/web/` is built by a
`build.rs` script in api/ during `cargo build --release`.

### Routes (axum)

Single origin (`web.northmark.tech`), path-scoped:

| Path prefix | Auth posture (post-ADR-011) | Source |
|---|---|---|
| `/` | operator-only (Tailscale) | Dashboard landing |
| `/chat`, `/chat/api`, `/chat/ws` | operator-only | Chat (lifts from `chat/` crate) |
| `/sessions`, `/sessions/api` | operator-only | New |
| `/skills`, `/skills/api` | operator-only | New |
| `/reminders`, `/reminders/api` | operator-only | New |
| `/diary`, `/diary/api` | operator-only | New |
| `/vault`, `/vault/api` | operator-only | New |
| `/cron`, `/cron/api` | operator-only | New |
| `/news` (SPA route) | operator-only | Admin view |
| `/news/api/*` | **public** (cloudflared) | Lifts from `news/api/` crate |
| `/api/health` | public | Standard |
| `/assets/*`, embedded SPA assets | public (static, no secrets) | Vite output |

The `news/api` public contract is preserved bit-for-bit so external
subscribers don't break.

### Frontend shell

`App.tsx` renders a left sidebar with the route inventory, a content
area, and a top strip with the `[nucleus]` mark + amber `▸`
connection indicator. Each route is a `pages/<Name>Page.tsx` file.
Pages own their data fetching via typed wrappers in `lib/api.ts`.

Router: React Router 7's minimal `<Routes>` setup — no nested-route
data-loading gymnastics. Keep it small.

## Migration strategy

**Short-lived parallel rollout, then immediate cleanup.** This does
*not* override [[feedback_cleanup_over_parallel]] — the parallel
period exists for the narrow purpose of running both the old
crates (`dashboard/`, `chat/`, `news/api/`) and `nucleus-dashboard`
side-by-side just long enough for **Playwright look/feel comparison**.
Once nucleus-dashboard has achieved aesthetic parity (or improvement)
across every surface and the operator has lived on it for the
verification window, the old crates are **hard-cut deleted in the
same change** — same week, not "we'll get to it later". The memory
stays as written.

### Phases

1. **Phase 1 — Build everything (this work).** On feature branch
   `nucleus-dashboard`:
   - Scaffold `nucleus-dashboard/{api,web}/` (axum + Vite +
     Tailwind + theme tokens + empty pages).
   - Port the dashboard widgets, chat surface, and news public API.
   - Build all new surfaces (`/sessions`, `/skills`, `/reminders`,
     `/diary`, `/vault`, `/cron`).
   - Add `tools/launchd/nucleus-dashboard.plist.example`.
   - Add cloudflared route `web.northmark.tech` → local port.
   - Commit to the feature branch incrementally surface-by-surface;
     merge to main only when feature-complete.

2. **Phase 2 — Playwright comparison + iteration.** Per
   [[playwright_mcp_single_browser]] (one Chromium across the
   machine — close it before testing). Compare each surface
   side-by-side: old dashboard vs `/`, old chat vs `/chat`,
   old news vs `/news`. Iterate on aesthetic and behavior until
   parity or improvement on every surface.

3. **Phase 3 — Sunset (one PR, same week).** Remove `dashboard/`,
   `chat/`, `news/api/` from the workspace. Delete their
   `tools/launchd/*.plist.example`. Update the cloudflared config
   to remove `dashboard.<domain>`, `chat.<domain>`, `news.<domain>`
   (or point them at nucleus-dashboard's origin if we want URL
   compat). Update README + ADR-001 + ADR-012.

4. **Phase 4 — Tailscale gating ([[ADR-011]] proper).** Apply
   Tailscale-Serve gating to the operator paths on
   `web.northmark.tech` (everything except `/news/api/*` and
   `/api/health`). News stays publicly accessible via cloudflared.

5. **Phase 5 — Canvas ([[ADR-012]]).** Implement canvas as a
   feature inside `/chat`. The "parallel rollout via
   `$NUCLEUS_CHAT_V2_PUBLIC_URL`" plan in ADR-012 collapses —
   nucleus-dashboard already proved chat in production; canvas
   ships behind a feature flag inside the same route.

## Implications for prior ADRs

- **ADR-001** — "one crate per surface" is amended for operator
  surfaces. News-fetcher, bots, distiller, etc. remain separate.
- **ADR-011** — sequencing changes (must come after this ADR).
  Gating becomes path-scoped on the nucleus-dashboard origin
  instead of per-tunnel.
- **ADR-012** — canvas is now a nucleus-dashboard-internal feature.
  No separate binary, no separate URL. The parallel-rollout
  mechanic collapses into nucleus-dashboard's own parallel rollout.

## Out of scope

- **Config editing in the UI.** Permanently out. `.env` and
  `nucleus.toml` stay file-edited. Skills stay file-edited.
  Personas stay file-edited.
- **Plugin system.** Hermes has plugins. Nucleus doesn't, and
  nucleus-dashboard doesn't introduce one. Capabilities are crates
  in the workspace.
- **Mobile-first design.** Desktop-only. The operator already has
  Obsidian mobile + WhatsApp on the phone; nucleus-dashboard is a
  desktop surface.
- **Auth beyond Tailscale.** No login, no session cookies, no API
  keys for the operator routes. Tailscale-network membership IS
  the auth. (Per ADR-011.)

## Future work

- **News admin extraction.** If news scoring config becomes complex
  enough to warrant a real editor (vs read-only viewer + `$EDITOR`),
  build it. Until then, view-only.
- **Skill editor.** Same. View-only unless the friction of opening
  the file becomes the bottleneck.
- **Real-time log tail.** Hermes' LogsPage. Defer until the absence
  of it bites.
- **Skill telemetry population.** ADR-008 defines `last_used` /
  `failure_count_30d` / `notify_on_failure` as frontmatter fields,
  but population code may not exist yet (verify during Phase 1).
  If not, `/skills` surfaces what exists and renders `—` for
  missing values. Wiring population is a follow-up task, not a
  Phase 1 blocker.
