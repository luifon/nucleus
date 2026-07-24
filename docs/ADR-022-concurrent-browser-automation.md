# ADR-022 — Concurrent browser automation: isolated contexts + shared storage state

Date: 2026-07-18
Status: accepted

## Context

Every Nucleus session — interactive operator sessions, reminder skill-fires,
venue bots — gets the project-scoped Playwright MCP server from `.mcp.json`.
Until now that server ran a **persistent profile**
(`--user-data-dir ~/.nucleus/playwright-profile`), which Chromium enforces as
single-instance: the first session to open a browser owns the profile; every
concurrent attempt dies with
`Browser is already in use for ~/.nucleus/playwright-profile`.

This was not an edge case. In the week of 2026-07-14, 4 of 6 DSU-prep
reminder fires failed on exactly this collision (reminder #19 three days in a
row), and the documented recovery ladder (`pkill` + deleting Chromium
`Singleton*` markers) is fighting the design rather than fixing it — the
upstream `@playwright/mcp` README states plainly that a persistent profile is
one-browser-at-a-time and concurrent clients should run `--isolated` or
distinct profiles.

The persistent profile existed for one reason: **logins** (AppFlowy web
today; whatever replaces it next). The contention it caused was collateral.

## Decision

Split the two concerns the profile was conflating:

1. **Concurrency: every session is isolated.** `.mcp.json` now runs
   `@playwright/mcp` with `--isolated --storage-state
   ~/.nucleus/playwright-storage.json`. Each session gets its own ephemeral
   browser seeded with the shared cookies/localStorage/IndexedDB snapshot.
   N sessions can drive browsers at the same time; nothing is shared, nothing
   collides, and a crashed fire's browser dies with it instead of stranding a
   lock.

2. **Auth: one owner, explicit flows.** The persistent profile at
   `~/.nucleus/playwright-profile` remains the single home of real logins,
   and only `tools/playwright-auth/` opens it:

   - `playwright-auth init` — create an empty storage state (fresh setup;
     the MCP server errors with ENOENT if the file is missing).
   - `playwright-auth capture [--origins a,b]` — headless export of the
     profile's state into the storage-state file. Origins must be listed for
     sites that keep tokens in localStorage/IndexedDB (state only exports for
     visited origins); pure-cookie sites need nothing.
   - `playwright-auth login --url <url>` — headed browser on the auth
     profile; the operator logs in, closes the window, and capture runs
     automatically. This is THE re-auth procedure: logging in inside a normal
     (isolated) session is ephemeral and silently lost.

   The storage-state file holds live session cookies — it is written
   `0600`, lives outside the repo, and is never committed (Rule 1 posture).

## Consequences

- The "Browser is already in use" failure class is gone for normal
  operation. It can only reappear if a pre-migration MCP server is still
  running with the old args (sessions pick up the new config on their next
  restart) or if two `playwright-auth` runs overlap — both self-identifying.
- Auth expiry changes shape: instead of a lock error, an expired snapshot
  surfaces as a logged-out page in an otherwise healthy browser. Recovery is
  `playwright-auth login` (or `capture` after the operator re-authed), not
  file surgery. The lock-recovery ladder in the triage recovery skill is replaced
  accordingly.
- Logins no longer accrete implicitly. A session that logs in somewhere does
  not persist it — deliberate: credentials flow through one auditable,
  operator-driven path.
- Multiple simultaneous Chromiums cost memory. Browser use in Nucleus is
  sporadic and short-lived (fires close on completion); "close the browser
  when done" survives as hygiene, no longer as a machine-wide mutex.
- The `--isolated` flag means in-browser state (open tabs, downloads) is
  discarded on close. No Nucleus flow depended on cross-session browser
  state; anything durable already lands in the vault or a DB.

## Verification (2026-07-18)

- Old config, two concurrent MCP servers → one fails with the historical
  `profile is already in use` error (failure reproduced at the failing
  layer).
- New committed config, two concurrent servers → both navigate
  successfully in parallel.
- AppFlowy workspace (the contract board behind reminder #19's failed
  fires) renders **authenticated** in an isolated context seeded from the
  captured storage state — no `/login` bounce, board UI present.
- Missing storage-state file → clean ENOENT error at browser launch,
  covered by `playwright-auth init` in setup.
