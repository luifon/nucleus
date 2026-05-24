import { Link } from "react-router-dom";
import { Boxes, ArrowRight } from "lucide-react";
import Tile from "@/components/Tile";
import { type AgentView } from "@/lib/api";

// Unified agent-health tile on the / landing (ADR-016). Summarizes every
// enabled registry agent by status, computed server-side from one
// /agents/api/list call. Links to the full /agents surface.

// Status → display order + color class for the count chips.
const ROWS: { status: AgentView["status"]; cls: string }[] = [
  { status: "errored", cls: "text-[var(--color-status-down)]" },
  { status: "stopped", cls: "text-[var(--color-status-down)]" },
  { status: "running", cls: "text-[var(--color-status-ok)]" },
  { status: "hosted", cls: "text-[var(--color-status-ok)]" },
  { status: "idle", cls: "text-[var(--color-nucleus-faint)]" },
  { status: "unknown", cls: "text-[var(--color-status-warn)]" },
];

export default function AgentsHealthTile({ data }: { data: AgentView[] | null }) {
  if (!data) {
    return <Tile Icon={Boxes} label="agents" status="…" statusKind="idle" />;
  }
  const counts = new Map<string, number>();
  for (const a of data) counts.set(a.status, (counts.get(a.status) ?? 0) + 1);
  const bad = (counts.get("errored") ?? 0) + (counts.get("stopped") ?? 0);

  return (
    <Tile
      Icon={Boxes}
      label="agents"
      status={`${data.length}`}
      statusKind={bad > 0 ? "down" : "ok"}
    >
      <ul className="space-y-1 text-[11px]">
        {ROWS.filter((r) => (counts.get(r.status) ?? 0) > 0).map((r) => (
          <li key={r.status} className="flex items-center gap-2">
            <span className={r.cls}>[{r.status.toUpperCase()}]</span>
            <span className="ml-auto text-[var(--color-nucleus-text)]">
              {counts.get(r.status)}
            </span>
          </li>
        ))}
      </ul>
      <Link
        to="/agents"
        className="mt-3 flex items-center gap-1 text-[11px] text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
      >
        all agents <ArrowRight size={10} strokeWidth={2} />
      </Link>
    </Tile>
  );
}
