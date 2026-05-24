import { useMemo, useState } from "react";
import { RefreshCw, Bell, History } from "lucide-react";
import PageShell from "@/components/PageShell";
import Tabs from "@/components/Tabs";
import ReminderRow from "@/components/reminders/ReminderRow";
import RecentFireRow from "@/components/reminders/RecentFireRow";
import { useFetch, usePolling, type FetchState } from "@/lib/hooks";
import {
  listReminders,
  listReminderHistory,
  type ReminderView,
  type ReminderStatus,
  type RecentFire,
} from "@/lib/api";

const STATUSES: ReminderStatus[] = ["active", "pending", "paused", "fired", "cancelled"];
const DEFAULT_VISIBLE: ReminderStatus[] = ["active", "pending"];
const HISTORY_POLL_MS = 30_000;

type TabValue = "manage" | "history";

export default function RemindersPage() {
  const [tab, setTab] = useState<TabValue>("manage");
  const [visible, setVisible] = useState<Set<ReminderStatus>>(new Set(DEFAULT_VISIBLE));

  const needFired = visible.has("fired");
  const needCancelled = visible.has("cancelled");

  const reminders = useFetch(
    () => listReminders({ includeFired: needFired, includeCancelled: needCancelled }),
    [needFired, needCancelled],
  );
  // Fire-attempt audit log (folded in from the retired /cron surface).
  const history = usePolling(listReminderHistory, HISTORY_POLL_MS);

  // Optimistic splice on write-action responses (no list refetch).
  const [optimistic, setOptimistic] = useState<Record<number, ReminderView>>({});
  const onChange = (r: ReminderView) => setOptimistic((m) => ({ ...m, [r.id]: r }));

  const merged = useMemo(
    () => (reminders.data ?? []).map((r) => optimistic[r.id] ?? r),
    [reminders.data, optimistic],
  );

  // Sort by next fire ascending (the "upcoming" reading); no-next-fire last.
  const filtered = useMemo(() => {
    return merged
      .filter((r) => visible.has(r.status as ReminderStatus))
      .sort((a, b) => (a.next_fire_at ?? "~").localeCompare(b.next_fire_at ?? "~"));
  }, [merged, visible]);

  const counts = useMemo(() => {
    const acc: Record<string, number> = {};
    for (const r of merged) acc[r.status] = (acc[r.status] ?? 0) + 1;
    return acc;
  }, [merged]);

  return (
    <PageShell
      title={
        <>
          reminders <span className="text-[var(--color-nucleus-faint)]">/ admin</span>
        </>
      }
      actions={
        <button
          onClick={() => {
            reminders.refetch();
            history.refetch();
            setOptimistic({});
          }}
          className="flex items-center gap-1.5 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-2.5 py-1 text-xs text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
        >
          <RefreshCw size={12} strokeWidth={1.75} />
          refresh
        </button>
      }
    >
      <Tabs
        tabs={[
          { value: "manage", label: "manage", count: merged.length || null },
          { value: "history", label: "history", count: history.data?.length ?? null },
        ]}
        value={tab}
        onChange={setTab}
      />

      {tab === "manage" ? (
        <ManageTab
          reminders={reminders}
          filtered={filtered}
          merged={merged}
          counts={counts}
          visible={visible}
          setVisible={setVisible}
          onChange={onChange}
        />
      ) : (
        <HistoryTab history={history} />
      )}
    </PageShell>
  );
}

function ManageTab({
  reminders,
  filtered,
  merged,
  counts,
  visible,
  setVisible,
  onChange,
}: {
  reminders: FetchState<ReminderView[]>;
  filtered: ReminderView[];
  merged: ReminderView[];
  counts: Record<string, number>;
  visible: Set<ReminderStatus>;
  setVisible: React.Dispatch<React.SetStateAction<Set<ReminderStatus>>>;
  onChange: (r: ReminderView) => void;
}) {
  return (
    <>
      <div className="mb-5 flex flex-wrap items-center gap-2">
        {STATUSES.map((s) => {
          const active = visible.has(s);
          return (
            <button
              key={s}
              onClick={() =>
                setVisible((prev) => {
                  const next = new Set(prev);
                  if (next.has(s)) next.delete(s);
                  else next.add(s);
                  return next;
                })
              }
              className={[
                "rounded border px-2.5 py-1 text-xs transition-colors",
                active
                  ? "border-[var(--color-nucleus-accent)] text-[var(--color-nucleus-accent)]"
                  : "border-[var(--color-nucleus-border)] text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-text)]",
              ].join(" ")}
            >
              {s} <span className="ml-1 opacity-70 tabular-nums">{counts[s] ?? 0}</span>
            </button>
          );
        })}
        <div className="ml-auto text-xs text-[var(--color-nucleus-faint)]">
          {reminders.data
            ? `${filtered.length} of ${merged.length} shown`
            : reminders.loading
              ? "fetching…"
              : (reminders.error ?? "")}
        </div>
      </div>

      {reminders.error ? (
        <div className="rounded border border-[var(--color-status-down)] bg-[var(--color-nucleus-surface)] px-3 py-2 text-sm text-[var(--color-status-down)]">
          {reminders.error}
        </div>
      ) : !reminders.data ? (
        <div className="text-sm text-[var(--color-nucleus-faint)]">fetching…</div>
      ) : filtered.length === 0 ? (
        <div className="flex items-center gap-2 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-3 py-6 text-sm text-[var(--color-nucleus-faint)]">
          <Bell size={14} strokeWidth={1.75} />
          no reminders match the selected statuses
        </div>
      ) : (
        <ul className="space-y-2">
          {filtered.map((r) => (
            <li key={r.id}>
              <ReminderRow reminder={r} onChange={onChange} />
            </li>
          ))}
        </ul>
      )}
    </>
  );
}

function HistoryTab({ history }: { history: FetchState<RecentFire[]> }) {
  if (history.error) {
    return (
      <div className="rounded border border-[var(--color-status-down)] bg-[var(--color-nucleus-surface)] px-3 py-2 text-sm text-[var(--color-status-down)]">
        {history.error}
      </div>
    );
  }
  if (!history.data) {
    return <div className="text-sm text-[var(--color-nucleus-faint)]">fetching…</div>;
  }
  if (history.data.length === 0) {
    return (
      <div className="flex items-center gap-2 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-3 py-6 text-sm text-[var(--color-nucleus-faint)]">
        <History size={14} strokeWidth={1.75} />
        no fires recorded yet
      </div>
    );
  }
  return (
    <ul className="space-y-1.5">
      {history.data.map((f) => (
        <RecentFireRow key={f.id} fire={f} />
      ))}
    </ul>
  );
}
