#!/usr/bin/env bash
# Launch the NoobAI-XL (SDXL) image-generation backend for the Nucleus
# /gallery surface (ADR-019). Self-contained diffusers + Apple-MPS FastAPI
# service, started by tools/launchd/noobai.plist. Mirrors tools/bonsai-serve.sh.
#
# Env:
#   NUCLEUS_NOOBAI_PORT   loopback port to listen on (default 8094)
#   NUCLEUS_NOOBAI_MODEL  HF model id (default Laxhar/noobai-XL-Vpred-1.0)
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/noobai" && pwd)"
PORT="${NUCLEUS_NOOBAI_PORT:-8094}"

# Let unsupported MPS ops fall back to CPU instead of erroring mid-generation.
export PYTORCH_ENABLE_MPS_FALLBACK=1
export HF_HUB_DISABLE_TELEMETRY=1
export TOKENIZERS_PARALLELISM=false

cd "$DIR"
# `uv run` syncs the venv from pyproject.toml on first run, then execs uvicorn.
exec uv run --quiet uvicorn serve:app --host 127.0.0.1 --port "$PORT"
