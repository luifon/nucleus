import { useMemo } from "react";
import { RefreshCw, Calendar, History, Cpu } from "lucide-react";
import PageShell from "@/components/PageShell";
import SectionHeader from "@/components/SectionHeader";
import LaunchdJobTile from "@/components/cron/LaunchdJobTile";
import UpcomingFireRow from "@/components/cron/UpcomingFireRow";
import RecentFireRow from "@/components/cron/RecentFireRow";
import { usePolling, useFetch } from "@/lib/hooks";
import {
  listLaunchdJobs,
  listUpcomingFires,
  listRecentFires,
} from "@/lib/api";

const LAUNCHD_POLL_MS  = 30_000;
const UPCOMING_POLL_MS = 60_000;
const RECENT_POLL_MS   = 30_000;

export default function CronPage() {
  // launchd + recent fires drift on their own (daemons restart, new
  // fires land) — poll. Upcoming changes only when reminders mutate,
  // poll less aggressively.
  const launchd  = usePolling(listLaunchdJobs,    LAUNCHD_POLL_MS);
  const upcoming = usePolling(listUpcomingFires,  UPCOMING_POLL_MS);
  const recent   = usePolling(listRecentFires,    RECENT_POLL_MS);

  const failingCount = useMemo(
    () => (launchd.data ?? []).filter((j) => j.pid === null && (j.last_exit ?? 0) > 0).length,
    [launchd.data],
  );
  const recentFailCount = useMemo(
    () => (recent.data ?? []).filter((f) => f.success === 0).length,
    [recent.data],
  );

  return (
    <PageShell
      title={
        <>
          cron <span className="text-[var(--color-nucleus-faint)]">/ scheduled work</span>
        </>
      }
      actions={
        <button
          onClick={() => { launchd.refetch(); upcoming.refetch(); recent.refetch(); }}
          className="flex items-center gap-1.5 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-2.5 py-1 text-xs text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
        >
          <RefreshCw size={12} strokeWidth={1.75} />
          refresh
        </button>
      }
    >
      <section className="mb-8">
        <SectionHeader
          label={`launchd · ${launchd.data?.length ?? "…"}`}
          hint={
            failingCount > 0
              ? `${failingCount} failing`
              : launchd.data
                ? "all clean"
                : undefined
          }
        />
        <FetchGate state={launchd} empty="no dev.nucleus.* plists loaded" Icon={Cpu}>
          {(rows) => (
            <div className="grid grid-cols-1 gap-2 md:grid-cols-2 xl:grid-cols-3">
              {rows.map((j) => <LaunchdJobTile key={j.label} job={j} />)}
            </div>
          )}
        </FetchGate>
      </section>

      <section className="mb-8">
        <SectionHeader
          label={`upcoming · ${upcoming.data?.length ?? "…"}`}
          hint="next 40 fires, soonest first"
        />
        <FetchGate state={upcoming} empty="no upcoming reminders" Icon={Calendar}>
          {(rows) => (
            <ul className="space-y-1.5">
              {rows.map((f) => <UpcomingFireRow key={f.id} fire={f} />)}
            </ul>
          )}
        </FetchGate>
      </section>

      <section>
        <SectionHeader
          label={`history · ${recent.data?.length ?? "…"}`}
          hint={recentFailCount > 0 ? `${recentFailCount} failed` : "last 60 fires"}
        />
        <FetchGate state={recent} empty="no fires yet" Icon={History}>
          {(rows) => (
            <ul className="space-y-1">
              {rows.map((f) => <RecentFireRow key={f.id} fire={f} />)}
            </ul>
          )}
        </FetchGate>
      </section>
    </PageShell>
  );
}

// Common "loading | error | empty | rows" pattern. Pulled inline as a
// helper because cron is the first surface that uses it three times in
// one page. If a fourth surface needs the same gate, promote to
// components/.
function FetchGate<T>({
  state,
  empty,
  Icon,
  children,
}: {
  state: ReturnType<typeof useFetch<T[]>>;
  empty: string;
  Icon: React.ComponentType<{ size?: number; strokeWidth?: number; className?: string }>;
  children: (rows: T[]) => React.ReactNode;
}) {
  if (state.error) {
    return (
      <div className="rounded border border-[var(--color-status-down)] bg-[var(--color-nucleus-surface)] px-3 py-2 text-xs text-[var(--color-status-down)]">
        {state.error}
      </div>
    );
  }
  if (!state.data && state.loading) {
    return <div className="text-xs text-[var(--color-nucleus-faint)]">fetching…</div>;
  }
  if (!state.data || state.data.length === 0) {
    return (
      <div className="flex items-center gap-2 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-3 py-3 text-xs text-[var(--color-nucleus-faint)]">
        <Icon size={12} strokeWidth={1.75} />
        {empty}
      </div>
    );
  }
  return <>{children(state.data)}</>;
}
