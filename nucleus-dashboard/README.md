# nucleus-dashboard

Unified operator app per [ADR-015](../docs/ADR-015-nucleus-dashboard-unified-operator-app.md).
Subsumes the standalone `dashboard/`, `chat/`, and `news/api/` crates
into one axum binary serving a React SPA at the origin in
`$NUCLEUS_PUBLIC_URL` (port 8092).

## Layout

```
nucleus-dashboard/
├── api/           Rust workspace member — axum server, all routes
│   └── src/
├── web/           React + Vite + Tailwind v4 frontend
│   └── src/
└── README.md      this file
```

## Dev workflow

Two terminals during development:

```bash
# Terminal 1 — axum backend on :8092
cargo run -p nucleus-dashboard

# Terminal 2 — Vite dev server on :5173 (proxies /api/* and /chat/ws → :8092)
cd nucleus-dashboard/web
npm install
npm run dev
```

Browse to `http://localhost:5173`.

## Production build

```bash
cd nucleus-dashboard/web
npm run build      # outputs to nucleus-dashboard/web/dist/

cargo build --release -p nucleus-dashboard
```

axum serves `web/dist/` via `ServeDir` at the SPA's root. The
`NUCLEUS_DASHBOARD_WEB_DIST` env var overrides the default location
(useful for the launchd-installed binary that runs from a copied
release).

## Aesthetic discipline

Per ADR-015 §"Aesthetic guardrails": hand-rolled components only (no
shadcn, no Headless UI, no marketplace theme), JBM mono everywhere,
near-black + amber accent, terminal-flavor flourishes (`[nucleus]`,
`┌──`, bracketed status pills). PRs that drift SaaS-shaped get
rejected.

## Migration status

This crate runs in **parallel** with the standalone `dashboard/`,
`chat/`, and `news/api/` crates during Phase 1 (build) and Phase 2
(Playwright look/feel comparison). Phase 3 deletes the old crates
in one PR — short-lived parallel, hard-cut sunset, no zombie infra.
See ADR-015 §"Migration strategy".
