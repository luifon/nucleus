import { useState, useEffect, type KeyboardEvent } from "react";
import { Wand2 } from "lucide-react";
import PageShell from "@/components/PageShell";
import SectionHeader from "@/components/SectionHeader";
import StatusPill from "@/components/StatusPill";
import ImageCard from "@/components/gallery/ImageCard";
import { useFetch } from "@/lib/hooks";
import {
  listImages,
  generateImage,
  galleryStatus,
  type GenerateBody,
} from "@/lib/api/gallery";

// Image generation surface (ADR-019). Prompt → a selectable local model → gallery.
// Per-model aspect presets: SDXL (NoobAI) wants ~1MP buckets and a PORTRAIT ratio
// for full-body characters (a square frame squashes proportions). Bonsai is square.
const MODELS = [
  {
    id: "bonsai",
    label: "Bonsai · fast",
    defaultDim: "512x512",
    dims: [
      { label: "512² · square", w: 512, h: 512 },
      { label: "768² · square", w: 768, h: 768 },
      { label: "1024² · square", w: 1024, h: 1024 },
    ],
  },
] as const;

const MODEL_KEY = "gallery_model";

function initialModel(): string {
  const saved = localStorage.getItem(MODEL_KEY);
  return MODELS.some((m) => m.id === saved) ? (saved as string) : "bonsai";
}

function defaultDimFor(id: string): string {
  return MODELS.find((m) => m.id === id)?.defaultDim ?? "1024x1024";
}

export default function GalleryPage() {
  const images = useFetch(listImages);
  const status = useFetch(galleryStatus);
  const [prompt, setPrompt] = useState("");
  const [model, setModel] = useState(initialModel);
  const [dim, setDim] = useState<string>(() => defaultDimFor(initialModel()));
  const [seed, setSeed] = useState("");
  const [generating, setGenerating] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const current = MODELS.find((m) => m.id === model) ?? MODELS[0];

  const onModelChange = (id: string) => {
    setModel(id);
    localStorage.setItem(MODEL_KEY, id);
    // Snap to the model's native default aspect (NoobAI → portrait, Bonsai → 512²).
    setDim(defaultDimFor(id));
  };

  const onGenerate = async () => {
    const p = prompt.trim();
    if (!p || generating) return;
    setGenerating(true);
    setError(null);
    const [w, h] = dim.split("x").map(Number);
    const body: GenerateBody = { prompt: p, model, width: w, height: h };
    const s = parseInt(seed, 10);
    if (Number.isFinite(s)) body.seed = s;
    try {
      await generateImage(body);
      setPrompt("");
      images.refetch();
    } catch (e) {
      setError(String(e));
    } finally {
      setGenerating(false);
    }
  };

  const onKey = (e: KeyboardEvent) => {
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") onGenerate();
  };

  // Async generation (ADR-019): generation runs server-side, so poll the list
  // while anything is still pending and let the tiles flip to images on their
  // own — survives navigating away / closing the tab.
  const anyPending = (images.data ?? []).some((i) => i.status === "pending");
  useEffect(() => {
    if (!anyPending) return;
    const t = setTimeout(() => images.refetch(), 4000);
    return () => clearTimeout(t);
  }, [anyPending, images.data, images.refetch]);

  const count = images.data?.length ?? 0;

  return (
    <PageShell
      title="gallery"
      subtitle="Local image generation (MLX / diffusers) — prompts run on-device; results persist here."
      actions={
        <div className="flex items-center gap-2">
          {status.loading ? (
            <StatusPill kind="idle">…</StatusPill>
          ) : (
            (status.data?.backends ?? []).map((b) => (
              <StatusPill key={b.name} kind={b.reachable ? "ok" : "down"}>
                {b.name}
              </StatusPill>
            ))
          )}
        </div>
      }
    >
      <div className="mb-6 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] p-3">
        <textarea
          value={prompt}
          onChange={(e) => setPrompt(e.target.value)}
          onKeyDown={onKey}
          placeholder="describe an image…  (⌘/Ctrl+Enter to generate)"
          rows={3}
          disabled={generating}
          className="w-full resize-none rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-3 py-2 text-sm text-[var(--color-nucleus-text)] placeholder:text-[var(--color-nucleus-faint)] focus:border-[var(--color-nucleus-accent)] focus:outline-none"
        />
        <div className="mt-2 flex flex-wrap items-center gap-3">
          <label className="flex items-center gap-1.5 text-xs text-[var(--color-nucleus-faint)]">
            model
            <select
              value={model}
              onChange={(e) => onModelChange(e.target.value)}
              disabled={generating}
              className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-2 py-1 text-xs text-[var(--color-nucleus-text)] focus:border-[var(--color-nucleus-accent)] focus:outline-none"
            >
              {MODELS.map((m) => (
                <option key={m.id} value={m.id}>
                  {m.label}
                </option>
              ))}
            </select>
          </label>
          <label className="flex items-center gap-1.5 text-xs text-[var(--color-nucleus-faint)]">
            size
            <select
              value={dim}
              onChange={(e) => setDim(e.target.value)}
              disabled={generating}
              className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-2 py-1 text-xs text-[var(--color-nucleus-text)] focus:border-[var(--color-nucleus-accent)] focus:outline-none"
            >
              {current.dims.map((d) => (
                <option key={d.label} value={`${d.w}x${d.h}`}>
                  {d.label}
                </option>
              ))}
            </select>
          </label>
          <label
            className="flex items-center gap-1.5 text-xs text-[var(--color-nucleus-faint)]"
            title="Starting point for the model's randomness. Same prompt + seed + size = the same image (reproducible). Leave blank for a new random variation each time; pin a number to reproduce or iterate on a result you liked."
          >
            seed
            <input
              value={seed}
              onChange={(e) => setSeed(e.target.value.replace(/[^0-9]/g, ""))}
              placeholder="random"
              inputMode="numeric"
              disabled={generating}
              className="w-24 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-2 py-1 text-xs tabular-nums text-[var(--color-nucleus-text)] placeholder:text-[var(--color-nucleus-faint)] focus:border-[var(--color-nucleus-accent)] focus:outline-none"
            />
          </label>
          <button
            onClick={onGenerate}
            disabled={generating || !prompt.trim()}
            className="ml-auto flex items-center gap-1.5 rounded border border-[var(--color-nucleus-accent)] bg-[color-mix(in_srgb,var(--color-nucleus-accent)_12%,transparent)] px-3 py-1.5 text-sm text-[var(--color-nucleus-accent)] transition-opacity disabled:opacity-50"
          >
            <Wand2 size={13} strokeWidth={1.75} />
            {generating ? "queuing…" : "generate"}
          </button>
        </div>
        {error && (
          <div className="mt-2 rounded border border-[var(--color-status-down)] px-3 py-2 text-xs text-[var(--color-status-down)]">
            {error}
          </div>
        )}
      </div>

      <SectionHeader label={`gallery · ${count}`} hint={images.loading ? "loading…" : undefined} />

      {images.error && (
        <div className="mb-3 rounded border border-[var(--color-status-down)] px-3 py-2 text-sm text-[var(--color-status-down)]">
          {images.error}
        </div>
      )}

      <div className="grid grid-cols-2 gap-3 md:grid-cols-3 xl:grid-cols-4">
        {(images.data ?? []).map((img) => (
          <ImageCard key={img.id} image={img} onDeleted={images.refetch} />
        ))}
      </div>

      {!images.loading && count === 0 && !generating && (
        <div className="mt-8 text-center text-sm text-[var(--color-nucleus-faint)]">
          no images yet — describe one above to get started.
        </div>
      )}
    </PageShell>
  );
}
