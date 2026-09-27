import { useEffect, useMemo, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { Inbox, RefreshCw } from "lucide-react";
import FilterDropdown from "@/components/FilterDropdown";
import PageShell from "@/components/PageShell";
import ItemRow from "@/components/work/ItemRow";
import ItemView from "@/components/work/ItemView";
import { useFetch, usePollWhile } from "@/lib/hooks";
import { listWorkItems, type WorkListItem } from "@/lib/api";
import {
  applyFilter,
  filterFromSearch,
  filterSummary,
  filterToSearch,
  isOpenItem,
  isWorking,
  itemFromSearch,
  sourceOptions,
  STATUS_FILTERS,
  type ListFilter,
  type StatusFilter,
} from "@/lib/work";

// Work items (ADR-036). `/work` is the list with stage, repo and what
// waits on the operator, filtered by status and source (both kept in the
// URL query). `/work?item=<n>` is the item page (components/work/ItemView):
// the conversation with the refinement agent, the plan with its versions
// and approval, and the item's details and actions. WhatsApp notices link
// there. On a phone the item page takes the whole screen and links back to
// the list; from the lg breakpoint the list stays in a column beside it.
// The list refreshes every POLL_MS while an agent or Nucleus works on an
// item.
const POLL_MS = 5_000;

export default function WorkPage() {
  const [params, setParams] = useSearchParams();
  const selected = itemFromSearch(params);
  const filter = filterFromSearch(params);
  const setFilter = (f: ListFilter) => setParams(filterToSearch(params, f), { replace: true });
  const items = useFetch((signal) => listWorkItems({ all: true }, signal));

  // Row returned by an action, shown until the next list response.
  const [optimistic, setOptimistic] = useState<Record<number, Partial<WorkListItem>>>({});
  useEffect(() => setOptimistic({}), [items.data]);
  const onChange = (i: Partial<WorkListItem> & { id: number }) => {
    setOptimistic((m) => ({ ...m, [i.id]: i }));
    items.refetch();
  };

  const all = useMemo(
    () => (items.data ?? []).map((i) => (optimistic[i.id] ? ({ ...i, ...optimistic[i.id] } as WorkListItem) : i)),
    [items.data, optimistic],
  );
  const shown = applyFilter(all, filter);
  const open = all.filter((i) => isOpenItem(i.stage));
  const working = all.some(isWorking);
  usePollWhile(items.refetch, working, POLL_MS);

  const filters = (
    <ListFilters items={all} filter={filter} onChange={setFilter} className={selected === null ? "mb-5" : "mb-2"} />
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
          <div className="px-3 pt-2">{filters}</div>
          <div className="flex-1 overflow-y-auto p-2">
            <ItemList items={items} shown={shown} filtered={shown.length < all.length} selected={selected} search={params} compact />
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
          work <span className="text-[var(--color-nucleus-faint)]">/ work items</span>
        </>
      }
      actions={refresh}
    >
      {filters}
      {items.data && all.length > 0 && (
        <div className="mb-5 flex flex-wrap items-center gap-2 text-xs text-[var(--color-nucleus-faint)]">
          <span>{shown.length} shown</span>
          <span>·</span>
          <span>{open.length} open</span>
          <span>·</span>
          <span>{all.length} total</span>
          {working && <span className="ml-auto">refreshing every {POLL_MS / 1000}s</span>}
          {items.error && <span className="ml-auto text-[var(--color-status-down)]">{items.error}</span>}
        </div>
      )}
      <ItemList items={items} shown={shown} filtered={shown.length < all.length} selected={null} search={params} />
    </PageShell>
  );
}

/** The status and source multi-selects. */
export function ListFilters({
  items,
  filter,
  onChange,
  className = "",
  initialOpen = null,
}: {
  items: readonly WorkListItem[];
  filter: ListFilter;
  onChange: (f: ListFilter) => void;
  className?: string;
  /** Which dropdown starts open (tests, screenshots). */
  initialOpen?: "status" | "source" | null;
}) {
  const sources = sourceOptions(items);
  const count = (v: StatusFilter) => items.filter((i) => applyFilter([i], { status: [v], source: null }).length > 0).length;
  const statusSelected = filter.status ?? [];
  const sourceSelected = filter.source ?? [];
  const statusLabels = STATUS_FILTERS.filter((s) => statusSelected.includes(s.value)).map((s) => s.label);
  const sourceLabels = sources.filter((s) => sourceSelected.includes(s.value)).map((s) => s.label);
  return (
    <div className={`flex flex-wrap items-center gap-2 ${className}`} role="group" aria-label="filters">
      <FilterDropdown
        label="status"
        options={STATUS_FILTERS.map((s) => ({
          value: s.value,
          label: s.label,
          meta: <span className="text-[10px] text-[var(--color-nucleus-faint)]">{count(s.value)}</span>,
        }))}
        selected={statusSelected}
        onChange={(next) => onChange({ ...filter, status: next.length === 0 ? null : (next as StatusFilter[]) })}
        summary={filterSummary(statusLabels, STATUS_FILTERS.length)}
        initialOpen={initialOpen === "status"}
      />
      <FilterDropdown
        label="source"
        options={sources}
        selected={sourceSelected}
        onChange={(next) => onChange({ ...filter, source: next.length === 0 ? null : next })}
        summary={filterSummary(sourceLabels, sources.length)}
        initialOpen={initialOpen === "source"}
      />
    </div>
  );
}

function ItemList({
  items,
  shown,
  filtered,
  selected,
  search,
  compact = false,
}: {
  items: { data: WorkListItem[] | null; error: string | null };
  shown: WorkListItem[];
  /** The filter hides some items. */
  filtered: boolean;
  selected: number | null;
  /** The current query: an item link keeps the filter. */
  search: URLSearchParams;
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
        {filtered ? "no items match the filters" : "no items recorded yet"}
      </div>
    );
  }
  return (
    <ul className={compact ? "space-y-1.5" : "space-y-2"}>
      {shown.map((i) => (
        <li key={i.id}>
          <ItemRow item={i} selected={i.id === selected} compact={compact} search={search} />
        </li>
      ))}
    </ul>
  );
}
