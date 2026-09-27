import { useEffect, useMemo, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { Inbox, RefreshCw } from "lucide-react";
import PageShell from "@/components/PageShell";
import Tabs from "@/components/Tabs";
import ItemRow from "@/components/work/ItemRow";
import ItemView from "@/components/work/ItemView";
import { useFetch, usePollWhile } from "@/lib/hooks";
import { listWorkItems, type WorkItem } from "@/lib/api";
import { isOpenItem, isWorking, itemFromSearch } from "@/lib/work";

// Issue pipeline items (ADR-036). `/work` is the list with stage, repo
// and what waits on the operator. `/work?item=<n>` is the item page
// (components/work/ItemView): the conversation with the refinement
// agent, the plan with its versions and approval, and the item's details
// and actions. WhatsApp notices link there. On a phone the item page
// takes the whole screen and links back to the list; from the lg
// breakpoint the list stays in a column beside it. The list refreshes
// every POLL_MS while an agent or Nucleus works on an item.
const POLL_MS = 5_000;

type TabValue = "open" | "all";

export default function WorkPage() {
  const [params] = useSearchParams();
  const selected = itemFromSearch(params);
  const [tab, setTab] = useState<TabValue>("open");
  const items = useFetch((signal) => listWorkItems({ all: true }, signal));

  // Row returned by an action, shown until the next list response.
  const [optimistic, setOptimistic] = useState<Record<number, WorkItem>>({});
  useEffect(() => setOptimistic({}), [items.data]);
  const onChange = (i: WorkItem) => {
    setOptimistic((m) => ({ ...m, [i.id]: i }));
    items.refetch();
  };

  const merged = useMemo(() => (items.data ?? []).map((i) => optimistic[i.id] ?? i), [items.data, optimistic]);
  const open = merged.filter((i) => isOpenItem(i.stage));
  const shown = tab === "open" ? open : merged;
  const working = merged.some(isWorking);
  usePollWhile(items.refetch, working, POLL_MS);

  const tabs = (
    <Tabs
      className={selected === null ? "mb-5" : "mb-0"}
      tabs={[
        { value: "open", label: "open", count: items.data ? open.length : null },
        { value: "all", label: "all", count: items.data ? merged.length : null },
      ]}
      value={tab}
      onChange={setTab}
    />
  );
  const refresh = (
    <button
      onClick={() => items.refetch()}
      title="refresh"
      className="flex items-center gap-1.5 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-2.5 py-1 text-xs text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
    >
      <RefreshCw size={12} strokeWidth={1.75} />
      refresh
    </button>
  );

  if (selected !== null) {
    return (
      <div className="flex h-full overflow-hidden">
        <aside
          aria-label="items"
          className="hidden w-72 shrink-0 flex-col border-r border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] lg:flex"
        >
          <div className="flex items-center justify-between px-3 pt-2">
            <span className="text-xs uppercase tracking-widest text-[var(--color-nucleus-faint)] opacity-70">items</span>
            <button onClick={() => items.refetch()} title="refresh" className="text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]">
              <RefreshCw size={12} strokeWidth={1.75} />
            </button>
          </div>
          <div className="px-1">{tabs}</div>
          <div className="flex-1 overflow-y-auto p-2">
            <ItemList items={items} shown={shown} tab={tab} selected={selected} compact />
          </div>
        </aside>
        <div className="min-w-0 flex-1 overflow-hidden">
          <ItemView itemId={selected} onChange={onChange} />
        </div>
      </div>
    );
  }

  return (
    <PageShell
      title={
        <>
          work <span className="text-[var(--color-nucleus-faint)]">/ issue pipeline</span>
        </>
      }
      actions={refresh}
    >
      {tabs}
      {items.data && shown.length > 0 && (
        <div className="mb-5 flex flex-wrap items-center gap-2 text-xs text-[var(--color-nucleus-faint)]">
          <span>{open.length} open</span>
          <span>·</span>
          <span>{merged.length} total</span>
          {working && <span className="ml-auto">refreshing every {POLL_MS / 1000}s</span>}
          {items.error && <span className="ml-auto text-[var(--color-status-down)]">{items.error}</span>}
        </div>
      )}
      <ItemList items={items} shown={shown} tab={tab} selected={null} />
    </PageShell>
  );
}

function ItemList({
  items,
  shown,
  tab,
  selected,
  compact = false,
}: {
  items: { data: WorkItem[] | null; error: string | null };
  shown: WorkItem[];
  tab: TabValue;
  selected: number | null;
  compact?: boolean;
}) {
  if (items.error && !items.data) {
    return (
      <div className="rounded border border-[var(--color-status-down)] bg-[var(--color-nucleus-surface)] px-3 py-2 text-sm text-[var(--color-status-down)]">
        {items.error}
      </div>
    );
  }
  if (!items.data) return <div className="px-1 text-sm text-[var(--color-nucleus-faint)]">fetching…</div>;
  if (shown.length === 0) {
    return (
      <div className="flex items-center gap-2 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-3 py-6 text-sm text-[var(--color-nucleus-faint)]">
        <Inbox size={14} strokeWidth={1.75} />
        {tab === "open" ? "no open items" : "no items recorded yet"}
      </div>
    );
  }
  return (
    <ul className={compact ? "space-y-1.5" : "space-y-2"}>
      {shown.map((i) => (
        <li key={i.id}>
          <ItemRow item={i} selected={i.id === selected} compact={compact} />
        </li>
      ))}
    </ul>
  );
}
