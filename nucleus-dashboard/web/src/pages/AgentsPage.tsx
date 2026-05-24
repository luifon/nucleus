import { RefreshCw, Boxes } from "lucide-react";
import PageShell from "@/components/PageShell";
import AgentTile from "@/components/agents/AgentTile";
import { usePolling } from "@/lib/hooks";
import { listAgents, type AgentClass, type AgentView } from "@/lib/api";

// The ADR-016 front door. One tile per registry agent, grouped by class.
// Liveness is computed server-side per the agent's launch mechanism; we
// poll so PID/exit/window state drifts in real time. This is what
// /sessions collapsed into — the attach affordance now lives per tile,
// alongside the run-log + launchd-log the old surface could never show.
const POLL_MS = 20_000;

// Display order + headings for the class groups.
const GROUPS: { class: AgentClass; label: string; blurb: string }[] = [
  { class: "conversational", label: "conversational", blurb: "operator-facing daemons hosting Claude pools" },
  { class: "scheduled", label: "scheduled", blurb: "launchd-cron domain jobs that drive Claude" },
  { class: "maintenance", label: "maintenance", blurb: "read diaries → durable artifacts" },
  { class: "ephemeral", label: "ephemeral", blurb: "on-demand, spawned by other agents" },
  { class: "infra", label: "infra", blurb: "host process / scheduler" },
];

export default function AgentsPage() {
  const agents = usePolling(listAgents, POLL_MS);

  return (
    <PageShell
      title={
        <>
          agents <span className="text-[var(--color-nucleus-faint)]">/ registry</span>
        </>
      }
      subtitle="Every agent in agents.toml (ADR-016), with live state probed per its launch mechanism. Expand a tile for its run-log (Claude transcript pointers) or launchd log tail."
      actions={
        <button
          onClick={agents.refetch}
          className="flex items-center gap-1.5 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-2.5 py-1 text-xs text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
        >
          <RefreshCw size={12} strokeWidth={1.75} />
          refresh
        </button>
      }
    >
      {agents.error ? (
        <div className="rounded border border-[var(--color-status-down)] bg-[var(--color-nucleus-surface)] px-3 py-2 text-sm text-[var(--color-status-down)]">
          {agents.error}
        </div>
      ) : !agents.data ? (
        <div className="text-sm text-[var(--color-nucleus-faint)]">fetching…</div>
      ) : agents.data.length === 0 ? (
        <div className="flex items-center gap-2 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-3 py-6 text-sm text-[var(--color-nucleus-faint)]">
          <Boxes size={14} strokeWidth={1.75} />
          no agents in the registry
        </div>
      ) : (
        <div className="space-y-7">
          {GROUPS.map((g) => {
            const inGroup = agents.data!.filter((a) => a.class === g.class);
            if (inGroup.length === 0) return null;
            return <AgentGroup key={g.class} label={g.label} blurb={g.blurb} agents={inGroup} />;
          })}
        </div>
      )}
    </PageShell>
  );
}

function AgentGroup({
  label,
  blurb,
  agents,
}: {
  label: string;
  blurb: string;
  agents: AgentView[];
}) {
  return (
    <section>
      <div className="mb-2 flex items-baseline gap-2">
        <h2 className="text-sm text-[var(--color-nucleus-accent)]">┌── {label}</h2>
        <span className="text-[11px] text-[var(--color-nucleus-faint)] opacity-70">{blurb}</span>
      </div>
      <div className="grid grid-cols-1 gap-2 md:grid-cols-2 xl:grid-cols-3">
        {agents.map((a) => (
          <AgentTile key={a.name} agent={a} />
        ))}
      </div>
    </section>
  );
}
