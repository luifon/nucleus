import { useMemo, useState } from "react";
import { RefreshCw, Bell } from "lucide-react";
import PageShell from "@/components/PageShell";
import ReminderRow from "@/components/reminders/ReminderRow";
import { useFetch } from "@/lib/hooks";
import { listReminders, type ReminderView, type ReminderStatus } from "@/lib/api";

const STATUSES: ReminderStatus[] = ["active", "pending", "paused", "fired", "cancelled"];
const DEFAULT_VISIBLE: ReminderStatus[] = ["active", "pending"];

export default function RemindersPage() {
  const [visible, setVisible] = useState<Set<ReminderStatus>>(new Set(DEFAULT_VISIBLE));

  // The backend list endpoint takes include_fired + include_cancelled
  // flags; the rest of the filtering is cheap client-side.
  const needFired = visible.has("fired");
  const needCancelled = visible.has("cancelled");

  const reminders = useFetch(
    () => listReminders({ includeFired: needFired, includeCancelled: needCancelled }),
    [needFired, needCancelled],
  );

  // Local optimistic state — when an action returns the updated row,
  // splice it into the list so the UI updates without a list refetch.
  const [optimistic, setOptimistic] = useState<Record<number, ReminderView>>({});
  const onChange = (r: ReminderView) => setOptimistic((m) => ({ ...m, [r.id]: r }));

  const merged = useMemo(() => {
    return (reminders.data ?? []).map((r) => optimistic[r.id] ?? r);
  }, [reminders.data, optimistic]);

  const filtered = useMemo(
    () => merged.filter((r) => visible.has(r.status as ReminderStatus)),
    [merged, visible],
  );

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
          onClick={() => { reminders.refetch(); setOptimistic({}); }}
          className="flex items-center gap-1.5 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-2.5 py-1 text-xs text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
        >
          <RefreshCw size={12} strokeWidth={1.75} />
          refresh
        </button>
      }
    >
      <div className="mb-5 flex flex-wrap items-center gap-2">
        {STATUSES.map((s) => {
          const active = visible.has(s);
          const count = counts[s] ?? 0;
          return (
            <button
              key={s}
              onClick={() => {
                setVisible((prev) => {
                  const next = new Set(prev);
                  if (next.has(s)) next.delete(s);
                  else next.add(s);
                  return next;
                });
              }}
              className={[
                "rounded border px-2.5 py-1 text-xs transition-colors",
                active
                  ? "border-[var(--color-nucleus-accent)] text-[var(--color-nucleus-accent)]"
                  : "border-[var(--color-nucleus-border)] text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-text)]",
              ].join(" ")}
            >
              {s} <span className="ml-1 opacity-70 tabular-nums">{count}</span>
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
    </PageShell>
  );
}
