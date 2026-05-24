// Per-bucket color badge using the locked PARA palette from ADR-014
// (see obsidian-tweaks skill for the source of truth). Reused on
// vault file rows + anywhere else a bucket reference appears.

const BUCKET_COLORS: Record<string, { bg: string; fg: string }> = {
  "0-Inbox":        { bg: "#c0392b", fg: "#ffffff" },
  "1-Main-Notes":   { bg: "#e67e22", fg: "#1a1a1a" },
  "2-Daily-Notes":  { bg: "#f1c40f", fg: "#1a1a1a" },
  "3-Projects":     { bg: "#1e8449", fg: "#ffffff" },
  "4-Areas":        { bg: "#2471a3", fg: "#ffffff" },
  "5-Resources":    { bg: "#5b3aa8", fg: "#ffffff" },
  "6-Slipbox":      { bg: "#8e44ad", fg: "#ffffff" },
  "7-Archives":     { bg: "#444444", fg: "#aaaaaa" },
};

export default function BucketBadge({ bucket }: { bucket: string }) {
  if (!bucket) {
    return (
      <span className="rounded border border-[var(--color-nucleus-border)] px-1.5 py-px text-[10px] text-[var(--color-nucleus-faint)]">
        root
      </span>
    );
  }
  const c = BUCKET_COLORS[bucket];
  if (!c) {
    return (
      <span className="rounded border border-[var(--color-nucleus-border)] px-1.5 py-px text-[10px] text-[var(--color-nucleus-faint)]">
        {bucket}
      </span>
    );
  }
  return (
    <span
      className="rounded px-1.5 py-px text-[10px] tabular-nums"
      style={{ backgroundColor: c.bg, color: c.fg }}
    >
      {bucket}
    </span>
  );
}
