import { RefreshCw } from "lucide-react";
import PageShell from "@/components/PageShell";
import HealthTile from "@/components/dashboard/HealthTile";
import GlancesTile from "@/components/dashboard/GlancesTile";
import DockerTile from "@/components/dashboard/DockerTile";
import TunnelTile from "@/components/dashboard/TunnelTile";
import AgentsHealthTile from "@/components/dashboard/AgentsHealthTile";
import { usePolling } from "@/lib/hooks";
import {
  getDashboardHealth,
  getDashboardGlances,
  getDashboardDocker,
  getDashboardTunnel,
  listAgents,
} from "@/lib/api";

export default function HomePage() {
  const health = usePolling(getDashboardHealth, 30_000);
  const glances = usePolling(getDashboardGlances, 30_000);
  const docker = usePolling(getDashboardDocker, 15_000);
  const tunnel = usePolling(getDashboardTunnel, 30_000);
  const agents = usePolling(listAgents, 20_000);

  const refresh = () => {
    health.refetch();
    glances.refetch();
    docker.refetch();
    tunnel.refetch();
    agents.refetch();
  };

  return (
    <PageShell
      title="dashboard"
      actions={
        <button
          onClick={refresh}
          className="flex items-center gap-1.5 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-2.5 py-1 text-xs text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
        >
          <RefreshCw size={12} strokeWidth={1.75} />
          refresh
        </button>
      }
    >
      <div className="grid grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-3">
        <HealthTile data={health.data} />
        <AgentsHealthTile data={agents.data} />
        <DockerTile data={docker.data} />
        <TunnelTile data={tunnel.data} />
        <div className="md:col-span-2 xl:col-span-3">
          <GlancesTile data={glances.data} />
        </div>
      </div>
    </PageShell>
  );
}
