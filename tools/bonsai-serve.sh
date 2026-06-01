#!/usr/bin/env bash
# Launch the Bonsai Image FastAPI backend (backend-only — no Next.js frontend)
# as a long-lived loopback service for the Nucleus dashboard's /gallery surface.
# See ADR-019. Mirrors the macOS backend launch in the demo's scripts/serve.sh
# (module scripts.local_backend_mac:app + the MFLUX_STUDIO_* env).
#
# Required env (set by tools/launchd/bonsai.plist, or export in your shell):
#   NUCLEUS_BONSAI_DIR   path to the cloned Bonsai-Image-Demo checkout
#   NUCLEUS_BONSAI_PORT  loopback port to listen on (default 8093)
#   BONSAI_VARIANT       ternary (default) | binary
set -euo pipefail

DEMO_DIR="${NUCLEUS_BONSAI_DIR:?set NUCLEUS_BONSAI_DIR to the Bonsai-Image-Demo checkout}"
PORT="${NUCLEUS_BONSAI_PORT:-8093}"
VARIANT="${BONSAI_VARIANT:-ternary}"

if [ ! -x "$DEMO_DIR/.venv/bin/uvicorn" ]; then
  echo "bonsai-serve: $DEMO_DIR/.venv/bin/uvicorn missing — run the demo's setup.sh first" >&2
  exit 1
fi

cd "$DEMO_DIR"

# Backend-only launch. Env mirrors scripts/serve.sh's Darwin branch: both baked
# model paths are exported unconditionally (image-studio only isabs-checks them;
# a missing binary dir is fine since we ship ternary), TE 4-bit on, GPU arm of
# /backends disabled so the picker can't offer a remote-only gemlite backend.
exec env \
  MFLUX_STUDIO_DEFAULT_BACKEND="bonsai-${VARIANT}-mlx" \
  MFLUX_STUDIO_BAKED_MODEL_PATH="$DEMO_DIR/models/bonsai-image-4B-ternary-mlx" \
  MFLUX_STUDIO_BAKED_BINARY_MODEL_PATH="$DEMO_DIR/models/bonsai-image-4B-binary-mlx" \
  MFLUX_STUDIO_TE_4BIT=true \
  MFLUX_STUDIO_FORCE_DISABLE_GPU=true \
  MFLUX_STUDIO_LAZY_COMPONENTS=true \
  MFLUX_STUDIO_EVICT_TRANSFORMER=true \
  MFLUX_STUDIO_EVICT_VAE=true \
  "$DEMO_DIR/.venv/bin/uvicorn" scripts.local_backend_mac:app \
    --host 127.0.0.1 --port "$PORT"
