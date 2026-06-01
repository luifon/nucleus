import { useState, type KeyboardEvent } from "react";
import { Wand2, RefreshCw } from "lucide-react";
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
// Per-model sizes: SDXL (NoobAI) is incoherent below ~768² — only offer the
// resolutions each model actually renders well at.
const MODELS = [
  { id: "noobai", label: "NoobAI · SDXL", est: "~3 min", defaultSize: 1024, sizes: [768, 1024] },
  { id: "bonsai", label: "Bonsai · fast", est: "~30s", defaultSize: 512, sizes: [512, 768, 1024] },
] as const;

const MODEL_KEY = "gallery_model";

function initialModel(): string {
  const saved = localStorage.getItem(MODEL_KEY);
  return MODELS.some((m) => m.id === saved) ? (saved as string) : "noobai";
}

export default function GalleryPage() {
  const images = useFetch(listImages);
  const status = useFetch(galleryStatus);
  const [prompt, setPrompt] = useState("");
  const [model, setModel] = useState(initialModel);
  const [size, setSize] = useState<number>(
    () => MODELS.find((m) => m.id === initialModel())?.defaultSize ?? 1024,
  );
  const [seed, setSeed] = useState("");
  const [generating, setGenerating] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const current = MODELS.find((m) => m.id === model) ?? MODELS[0];

  const onModelChange = (id: string) => {
    setModel(id);
    localStorage.setItem(MODEL_KEY, id);
    // Snap the size to the model's native default (SDXL wants 1024, Bonsai 512).
    setSize(MODELS.find((m) => m.id === id)?.defaultSize ?? size);
  };

  const onGenerate = async () => {
    const p = prompt.trim();
    if (!p || generating) return;
    setGenerating(true);
    setError(null);
    const body: GenerateBody = { prompt: p, model, width: size, height: size };
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
              value={size}
              onChange={(e) => setSize(Number(e.target.value))}
              disabled={generating}
              className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-2 py-1 text-xs text-[var(--color-nucleus-text)] focus:border-[var(--color-nucleus-accent)] focus:outline-none"
            >
              {current.sizes.map((s) => (
                <option key={s} value={s}>
                  {s}²
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
            {generating ? `generating… ${current.est}` : "generate"}
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
        {generating && (
          <div className="flex aspect-square items-center justify-center rounded border border-dashed border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] text-xs text-[var(--color-nucleus-faint)]">
            <span className="flex items-center gap-1.5">
              <RefreshCw size={13} className="animate-spin" /> {current.label.split(" ")[0]} {current.est}
            </span>
          </div>
        )}
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
