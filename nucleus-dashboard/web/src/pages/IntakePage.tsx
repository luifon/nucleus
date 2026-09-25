import { useEffect, useMemo, useState } from "react";
import { Inbox, RefreshCw } from "lucide-react";
import PageShell from "@/components/PageShell";
import Tabs from "@/components/Tabs";
import ItemRow from "@/components/intake/ItemRow";
import { useFetch, usePollWhile } from "@/lib/hooks";
import { listIntakeItems, type IntakeItem } from "@/lib/api";
import { isOpenItem, isWorking } from "@/lib/intake";

// Issue pipeline items (ADR-036): list with stage, repo and what waits on
// the operator; the detail holds the eval, the plan, the thread with a
// reply box, the implementation, the PR and the issue comment. The list
// refreshes every POLL_MS while an agent or Nucleus works on an item.
const POLL_MS = 5_000;

type TabValue = "open" | "all";

export default function IntakePage() {
  const [tab, setTab] = useState<TabValue>("open");
  const items = useFetch((signal) => listIntakeItems({ all: true }, signal));

  // Row returned by an action, shown until the next list response.
  const [optimistic, setOptimistic] = useState<Record<number, IntakeItem>>({});
  useEffect(() => setOptimistic({}), [items.data]);
  const onChange = (i: IntakeItem) => {
    setOptimistic((m) => ({ ...m, [i.id]: i }));
    items.refetch();
  };

  const merged = useMemo(() => (items.data ?? []).map((i) => optimistic[i.id] ?? i), [items.data, optimistic]);
  const open = merged.filter((i) => isOpenItem(i.stage));
  const shown = tab === "open" ? open : merged;
  const working = merged.some(isWorking);
  usePollWhile(items.refetch, working, POLL_MS);
  const now = Date.now();

  return (
    <PageShell
      title={
        <>
          intake <span className="text-[var(--color-nucleus-faint)]">/ issue pipeline</span>
        </>
      }
      actions={
        <button
          onClick={() => items.refetch()}
          className="flex items-center gap-1.5 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-2.5 py-1 text-xs text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
        >
          <RefreshCw size={12} strokeWidth={1.75} />
          refresh
        </button>
      }
    >
      <Tabs
        tabs={[
          { value: "open", label: "open", count: items.data ? open.length : null },
          { value: "all", label: "all", count: items.data ? merged.length : null },
        ]}
        value={tab}
        onChange={setTab}
      />
      {items.error && !items.data ? (
        <div className="rounded border border-[var(--color-status-down)] bg-[var(--color-nucleus-surface)] px-3 py-2 text-sm text-[var(--color-status-down)]">
          {items.error}
        </div>
      ) : !items.data ? (
        <div className="text-sm text-[var(--color-nucleus-faint)]">fetching…</div>
      ) : shown.length === 0 ? (
        <div className="flex items-center gap-2 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-3 py-6 text-sm text-[var(--color-nucleus-faint)]">
          <Inbox size={14} strokeWidth={1.75} />
          {tab === "open" ? "no open items" : "no items recorded yet"}
        </div>
      ) : (
        <>
          <div className="mb-5 flex flex-wrap items-center gap-2 text-xs text-[var(--color-nucleus-faint)]">
            <span>{open.length} open</span>
            <span>·</span>
            <span>{merged.length} total</span>
            {working && <span className="ml-auto">refreshing every {POLL_MS / 1000}s</span>}
            {items.error && <span className="ml-auto text-[var(--color-status-down)]">{items.error}</span>}
          </div>
          <ul className="space-y-2">
            {shown.map((i) => (
              <li key={i.id}>
                <ItemRow item={i} now={now} onChange={onChange} />
              </li>
            ))}
          </ul>
        </>
      )}
    </PageShell>
  );
}
