import { Globe } from "lucide-react";
import Tile from "@/components/Tile";
import { type TunnelResp } from "@/lib/api";

export default function TunnelTile({ data }: { data: TunnelResp | null }) {
  if (!data) return <Tile Icon={Globe} label="tunnel" status="…" statusKind="idle" />;
  if (!data.configured) {
    return (
      <Tile Icon={Globe} label="tunnel" status="OFF" statusKind="idle">
        <div className="text-[11px] text-[var(--color-nucleus-faint)]">
          NUCLEUS_PUBLIC_URL not set
        </div>
      </Tile>
    );
  }
  return (
    <Tile
      Icon={Globe}
      label="tunnel"
      status={data.ok ? "OK" : data.status_code ? `${data.status_code}` : "DOWN"}
      statusKind={data.ok ? "ok" : "down"}
    >
      <div className="text-[11px] text-[var(--color-nucleus-faint)]">
        <div className="truncate" title={data.url ?? ""}>{data.url}</div>
        <div>
          {data.elapsed_ms != null ? `${data.elapsed_ms}ms` : "—"}
          {data.error && ` · ${data.error}`}
        </div>
      </div>
    </Tile>
  );
}
