import { useState } from "react";
import { Trash2, RefreshCw, AlertTriangle } from "lucide-react";
import { type GeneratedImage, imageUrl, deleteImage } from "@/lib/api/gallery";

// One generated image — thumbnail + prompt + metadata, with a hover delete.
// Renders a spinner while pending and an error state if generation failed, so
// the async lifecycle (ADR-019) shows inline. Mirrors news/NewsCard styling.
export default function ImageCard({
  image,
  onDeleted,
}: {
  image: GeneratedImage;
  onDeleted: () => void;
}) {
  const [busy, setBusy] = useState(false);

  const onDelete = async () => {
    if (busy) return;
    setBusy(true);
    try {
      await deleteImage(image.id);
      onDeleted();
    } catch {
      setBusy(false);
    }
  };

  return (
    <div className="group overflow-hidden rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)]">
      <div className="relative">
        {image.status === "ready" ? (
          <a href={imageUrl(image.id)} target="_blank" rel="noreferrer">
            <img
              src={imageUrl(image.id)}
              alt={image.prompt}
              loading="lazy"
              className="aspect-square w-full object-cover"
            />
          </a>
        ) : image.status === "failed" ? (
          <div className="flex aspect-square w-full flex-col items-center justify-center gap-1 bg-[var(--color-nucleus-bg)] px-3 text-center text-[var(--color-status-down)]">
            <AlertTriangle size={18} strokeWidth={1.75} />
            <span className="text-[10px]">generation failed</span>
          </div>
        ) : (
          <div className="flex aspect-square w-full items-center justify-center gap-1.5 bg-[var(--color-nucleus-bg)] text-xs text-[var(--color-nucleus-faint)]">
            <RefreshCw size={14} className="animate-spin" />
            generating…
          </div>
        )}
        <span className="absolute left-1.5 top-1.5 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)]/80 px-1.5 py-0.5 text-[10px] text-[var(--color-nucleus-accent)]">
          {image.model}
        </span>
        <button
          onClick={onDelete}
          disabled={busy}
          aria-label="delete image"
          className="absolute right-1.5 top-1.5 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)]/80 p-1 text-[var(--color-nucleus-faint)] opacity-0 transition-opacity hover:text-[var(--color-status-down)] group-hover:opacity-100 disabled:opacity-40"
        >
          <Trash2 size={13} strokeWidth={1.75} />
        </button>
      </div>
      <div className="p-2.5">
        <p className="line-clamp-2 text-xs text-[var(--color-nucleus-text)]">{image.prompt}</p>
        {image.status === "failed" && image.error ? (
          <p className="mt-1 line-clamp-2 text-[10px] text-[var(--color-status-down)]">{image.error}</p>
        ) : (
          <p className="mt-1 text-[10px] tabular-nums text-[var(--color-nucleus-faint)] opacity-70">
            {image.width}×{image.height} · seed {image.seed} ·{" "}
            {new Date(image.created_at).toLocaleString()}
          </p>
        )}
      </div>
    </div>
  );
}
