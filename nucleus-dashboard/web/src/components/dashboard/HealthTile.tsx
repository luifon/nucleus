import { Heart, Check, X } from "lucide-react";
import Tile from "@/components/Tile";
import { type HealthOverview } from "@/lib/api";

export default function HealthTile({ data }: { data: HealthOverview | null }) {
  if (!data) {
    return <Tile Icon={Heart} label="health" status="…" statusKind="idle" />;
  }
  const allOk = data.ok_count === data.total;
  return (
    <Tile
      Icon={Heart}
      label="health"
      status={`${data.ok_count}/${data.total}`}
      statusKind={allOk ? "ok" : "down"}
    >
      <ul className="space-y-1 text-[11px]">
        {data.checks.map((c) => (
          <li key={c.name} className="flex items-center gap-2">
            {c.ok ? (
              <Check size={10} strokeWidth={2} className="text-[var(--color-status-ok)]" />
            ) : (
              <X size={10} strokeWidth={2} className="text-[var(--color-status-down)]" />
            )}
            <span className="text-[var(--color-nucleus-text)]">{c.name}</span>
            <span
              className="ml-auto truncate text-[var(--color-nucleus-faint)]"
              title={c.detail}
            >
              {c.detail}
            </span>
          </li>
        ))}
      </ul>
    </Tile>
  );
}
