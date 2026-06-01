"""NoobAI-XL (SDXL) image-generation backend for the Nucleus /gallery surface.

Mirrors the Bonsai backend's HTTP contract so the dashboard's Axum proxy is
model-agnostic: POST /generate {prompt, seed, steps, width, height} -> PNG bytes,
GET /backends for a status probe. SDXL via HuggingFace diffusers on Apple MPS.

Lifecycle (ADR-019): lazy-load + idle-unload. The model is NOT loaded at
startup; it loads on the first /generate and is freed after NUCLEUS_NOOBAI_IDLE_SECS
of inactivity, so an idle service holds ~no model RAM (just the Python process).
SDXL reloads in ~15s, so we keep it warm between back-to-back generations rather
than evicting after each.

Model gotchas (MPS), all of which otherwise yield all-black images:
  - Laxhar/noobai-XL-Vpred-1.0 ships an epsilon scheduler but is v-prediction →
    force EulerDiscrete(prediction_type="v_prediction", rescale_betas_zero_snr).
  - Use bfloat16 + pipe.upcast_vae(): diffusers only auto-upcasts the VAE for
    fp16, so a bf16 VAE decodes to NaN.
  - Do NOT enable_attention_slicing(): on MPS it makes the UNET emit NaN at
    >=768^2. Unsliced is correct at 1024^2 and fits 16 GB.
"""

import gc
import io
import os
import threading
import time

import torch
from contextlib import asynccontextmanager
from diffusers import DiffusionPipeline, EulerDiscreteScheduler
from fastapi import FastAPI, HTTPException, Response
from pydantic import BaseModel, Field

MODEL_ID = os.environ.get("NUCLEUS_NOOBAI_MODEL", "Laxhar/noobai-XL-Vpred-1.0")
DEFAULT_STEPS = int(os.environ.get("NUCLEUS_NOOBAI_STEPS", "26"))
DEFAULT_CFG = float(os.environ.get("NUCLEUS_NOOBAI_CFG", "5.0"))
DEFAULT_RESCALE = float(os.environ.get("NUCLEUS_NOOBAI_GUIDANCE_RESCALE", "0.7"))
DEFAULT_W = int(os.environ.get("NUCLEUS_NOOBAI_WIDTH", "1024"))
DEFAULT_H = int(os.environ.get("NUCLEUS_NOOBAI_HEIGHT", "1024"))
# Free the model after this many seconds idle. Keeps a generation *session* warm
# but releases the ~5 GB when you walk away.
IDLE_SECS = int(os.environ.get("NUCLEUS_NOOBAI_IDLE_SECS", "600"))

# Guards _pipe across the generate path and the idle reaper. Held for the whole
# (minutes-long) generation; the reaper just waits its turn.
_lock = threading.Lock()
_pipe = None
_device = "mps" if torch.backends.mps.is_available() else "cpu"
_last_used = 0.0


def _load():
    """Load the pipeline if not resident. Caller holds _lock."""
    global _pipe
    if _pipe is not None:
        return _pipe
    pipe = DiffusionPipeline.from_pretrained(MODEL_ID, torch_dtype=torch.bfloat16)
    pipe.scheduler = EulerDiscreteScheduler.from_config(
        pipe.scheduler.config,
        prediction_type="v_prediction",
        rescale_betas_zero_snr=True,
    )
    pipe = pipe.to(_device)
    try:
        pipe.upcast_vae()
    except Exception:
        pass
    _pipe = pipe
    return _pipe


def _unload():
    """Free the pipeline + reclaim MPS memory. Caller holds _lock."""
    global _pipe
    if _pipe is None:
        return
    _pipe = None
    gc.collect()
    if torch.backends.mps.is_available():
        try:
            torch.mps.empty_cache()
        except Exception:
            pass


def _idle_reaper():
    while True:
        time.sleep(60)
        with _lock:
            if _pipe is not None and (time.monotonic() - _last_used) > IDLE_SECS:
                _unload()


@asynccontextmanager
async def lifespan(_app: FastAPI):
    # Lazy: don't load at startup — just start the idle reaper. The model loads
    # on the first /generate.
    threading.Thread(target=_idle_reaper, daemon=True).start()
    yield
    with _lock:
        _unload()


app = FastAPI(lifespan=lifespan)


@app.get("/backends")
def backends():
    # Liveness/status probe — must NOT load the model or reset the idle timer,
    # or the dashboard's status checks would keep it pinned warm forever.
    return {"model": MODEL_ID, "device": _device, "loaded": _pipe is not None}


class GenerateRequest(BaseModel):
    prompt: str = Field(min_length=1)
    seed: int = 0
    steps: int = Field(default=DEFAULT_STEPS, ge=1)
    width: int = Field(default=DEFAULT_W, ge=64)
    height: int = Field(default=DEFAULT_H, ge=64)


@app.post("/generate")
def generate(req: GenerateRequest) -> Response:
    global _last_used
    generator = torch.Generator(device="cpu").manual_seed(int(req.seed))
    with _lock:
        pipe = _load()
        _last_used = time.monotonic()
        try:
            result = pipe(
                prompt=req.prompt,
                num_inference_steps=req.steps,
                guidance_scale=DEFAULT_CFG,
                guidance_rescale=DEFAULT_RESCALE,
                width=req.width,
                height=req.height,
                generator=generator,
            )
        except Exception as e:  # noqa: BLE001 — clean 500 to the proxy
            raise HTTPException(status_code=500, detail=f"generation failed: {e}")
        _last_used = time.monotonic()
    buf = io.BytesIO()
    result.images[0].save(buf, format="PNG")
    return Response(content=buf.getvalue(), media_type="image/png")
