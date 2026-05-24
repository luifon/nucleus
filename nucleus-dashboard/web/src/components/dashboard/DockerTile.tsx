import { Container } from "lucide-react";
import Tile from "@/components/Tile";
import StatusPill, { type StatusKind } from "@/components/StatusPill";
import { type DockerResp } from "@/lib/api";

export default function DockerTile({ data }: { data: DockerResp | null }) {
  if (!data) return <Tile Icon={Container} label="docker" status="…" statusKind="idle" />;
  if (!data.available) {
    return (
      <Tile Icon={Container} label="docker" status="OFFLINE" statusKind="idle">
        <div className="text-[11px] text-[var(--color-nucleus-faint)]">
          {data.error ?? "daemon not reachable"}
        </div>
      </Tile>
    );
  }
  if (data.containers.length === 0) {
    return (
      <Tile Icon={Container} label="docker" status="0" statusKind="idle">
        <div className="text-[11px] text-[var(--color-nucleus-faint)]">no containers</div>
      </Tile>
    );
  }
  const running = data.containers.filter((c) => c.state === "running").length;
  return (
    <Tile
      Icon={Container}
      label="docker"
      status={`${running}/${data.containers.length}`}
      statusKind={running === data.containers.length ? "ok" : "warn"}
    >
      <ul className="space-y-1 text-[11px]">
        {data.containers.slice(0, 6).map((c) => (
          <li key={c.id} className="flex items-center gap-2">
            <span
              className="truncate text-[var(--color-nucleus-text)]"
              title={`${c.image} · ${c.status}`}
            >
              {c.names[0]?.replace(/^\//, "") ?? c.id.slice(0, 8)}
            </span>
            <StatusPill kind={stateKind(c.state)}>{c.state.toUpperCase()}</StatusPill>
          </li>
        ))}
      </ul>
    </Tile>
  );
}

function stateKind(state: string): StatusKind {
  switch (state) {
    case "running":
      return "ok";
    case "restarting":
    case "paused":
      return "warn";
    case "exited":
    case "dead":
      return "down";
    default:
      return "idle";
  }
}
