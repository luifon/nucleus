# ADR-019 — Image generation surface (local Bonsai model + dashboard gallery)

**Status:** Accepted (implemented 2026-06-01)
**Related:** ADR-015 (unified dashboard), ADR-018 (WhatsApp media — brick 3), ADR-011 (perimeter)

## Context

Nucleus gained the ability to generate images locally: PrismML's **Bonsai
Image 4B** (a 2-bit-quantized FLUX.2 Klein 4B diffusion transformer) runs
on-device via MLX on Apple Silicon (~25–35s at 512² warm on an M1 Pro, 4-step
sampling). We want a first-class operator surface to drive it — a prompt box and
a persistent gallery — rather than a CLI-only capability.

Bonsai ships a FastAPI backend (`POST /generate` → raw PNG bytes). The model is
delivered as a custom **git fork of mlx** that must be compiled from source
against Xcode's Metal toolchain — non-trivial infra, so it lives in an external
checkout (`$NUCLEUS_BONSAI_DIR`, e.g. `~/Development/Bonsai-Image-Demo`), not
vendored into this repo.

## Decision

A three-tier surface, reusing the existing dashboard stack (ADR-015):

```
React /gallery  ──fetch──>  Axum dashboard :8092  ──reqwest──>  Bonsai FastAPI :8093
 prompt + gallery grid      /gallery/api/{generate,images,status}   POST /generate -> PNG
                            serves PNGs at /gallery/files/*
                            persists memory/gallery.db + memory/gallery/<id>.png
```

1. **Bonsai backend as an always-warm loopback service.** A launchd agent
   (`dev.nucleus.bonsai`, `tools/launchd/bonsai.plist.example` →
   `tools/bonsai-serve.sh`) runs the demo's macOS uvicorn backend
   (`scripts.local_backend_mac:app`) on `127.0.0.1:[ports].bonsai` (8093),
   `RunAtLoad`+`KeepAlive`. Backend-only — the demo's Next.js frontend is not
   started. Opt-in: install.sh skips it unless `NUCLEUS_BONSAI_DIR` is set.

   **Always-warm vs lazy:** chosen always-warm for instant generations. It holds
   ~2 GB RAM continuously, acceptable on the 16 GB host alongside the bots. The
   design isolates lifecycle to the service: switching to lazy/idle-unload later
   (dashboard-managed spawn + idle timer) needs no API or UI change.

2. **Axum proxy + persistence** (`handlers/gallery.rs`, mounted `/gallery/api`).
   `POST /generate` forwards the prompt (+ optional seed/steps/size; a
   time-derived seed when unset) to the Bonsai backend, writes the returned PNG
   to `memory/gallery/<uuid>.png`, records a row in `memory/gallery.db`
   (`generated_images`), and returns it. `GET /images` lists, `DELETE
   /images/{id}` removes row + file, `GET /status` probes backend reachability.
   PNG bytes are served by a `ServeDir` mount at `/gallery/files/*`. The surface
   is tolerated-missing: if `gallery.db` can't open it's simply absent.

3. **React surface** (`pages/GalleryPage.tsx`, route `/gallery`, directly below
   Chat in the sidebar). Prompt textarea + size/seed controls + a `[model
   up/down]` pill, over a persistent gallery grid (`ImageCard`). Matches the
   locked ADR-015 aesthetic (near-black + amber, JBM mono, bordered tiles).

Generation is **synchronous** — the `POST /generate` request blocks the tens of
seconds it takes; the UI shows a "generating…" placeholder. Consistent with how
the chat surface blocks on long Claude turns; no job queue needed at this scale.

## Consequences

- One warm Bonsai backend is the single inference service. **ADR-018 brick 3**
  (WhatsApp image delivery) reuses the same `:8093` backend — one service, two
  consumers — instead of a separate `gen-image` path.
- `[ports].bonsai` (nucleus.toml) and the plist's `NUCLEUS_BONSAI_PORT` must
  agree (both default 8093); changing the port means updating both.
- Bonsai is an external dependency keyed by `$NUCLEUS_BONSAI_DIR`; a fresh clone
  without it just doesn't get the surface (graceful).
- `dev.nucleus.bonsai` is now part of the runtime baseline — added to
  `tools/healthcheck.sh`'s persistent-services check.
- Gallery PNGs accumulate under `memory/gallery/`; deletion is manual (per-image)
  for now. No retention sweep yet.

## Alternatives considered

- **Vendor Bonsai into the repo** — rejected: large repo + multi-GB models +
  a from-source mlx-fork compile; wrong thing to commit. External checkout +
  env pointer is cleaner.
- **Lazy/idle-unload backend** — deferred (see above); revisit if RAM pressure
  appears.
- **Reimplement inference in-process (Rust/MLX)** — rejected: the FastAPI
  backend already exists and is the model author's supported path.

## Verification

`./tools/healthcheck.sh` shows `dev.nucleus.bonsai` running; `curl
127.0.0.1:8092/gallery/api/status` → `reachable:true`; generating from the
`/gallery` surface yields a PNG that persists across reloads and is deletable.
