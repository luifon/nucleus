import { useCallback, useMemo, useState } from "react";
import { RefreshCw, AlertTriangle } from "lucide-react";
import PageShell from "@/components/PageShell";
import SectionHeader from "@/components/SectionHeader";
import FilterDropdown from "@/components/FilterDropdown";
import NewsCard, { type NewsCardActions } from "@/components/news/NewsCard";
import NewsBriefTile from "@/components/news/NewsBriefTile";
import { useFetch, todayLocal } from "@/lib/hooks";
import {
  getNewsBrief,
  listNewsItems,
  listNewsSources,
  recordNewsOpen,
  voteOnNews,
  type NewsItem,
} from "@/lib/api";
import { orderForDisplay, serialQueue, sharedEvents, splitItems } from "@/lib/news";

export default function NewsPage() {
  const [fetchDate, setFetchDate] = useState(todayLocal());
  const [minScore, setMinScore] = useState(0);
  const [selectedSources, setSelectedSources] = useState<string[]>([]);

  const items = useFetch(
    () => listNewsItems({ fetchDate, minScore, limit: 200 }),
    [fetchDate, minScore],
  );
  const sources = useFetch(listNewsSources);
  // The date travels with the answer, so a brief never shows above another
  // day's items while a new date loads or after its request fails.
  const brief = useFetch(
    () => getNewsBrief(fetchDate).then((b) => ({ date: fetchDate, brief: b })),
    [fetchDate],
  );
  // One card at a time shows the reason strip, as on the widget.
  const [reasonFor, setReasonFor] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);

  const refetchItems = items.refetch;
  const refetchBrief = brief.refetch;
  // Writes go out one at a time, in click order. The server stamps a vote when
  // it arrives and the latest stamp wins (ADR-031), so two requests in flight
  // at once could store a downvote after the reason that followed it.
  const enqueue = useMemo(() => serialQueue(), []);
  const act = useCallback(
    (work: () => Promise<unknown>): Promise<boolean> =>
      enqueue(work).then(
        () => {
          setActionError(null);
          refetchItems();
          refetchBrief();
          return true;
        },
        (e) => {
          setActionError(String(e));
          return false;
        },
      ),
    [enqueue, refetchItems, refetchBrief],
  );
  const closeReason = useCallback(() => setReasonFor(null), []);

  // A downvote opens the reason strip on its card; a reason is optional and
  // arrives as a second downvote carrying it (ADR-031). The strip closes only
  // once the reason is stored, so a failed save keeps the typed note.
  const actionsFor = (it: NewsItem): NewsCardActions => ({
    onVote: (v) => {
      setReasonFor(v === -1 ? it.id : null);
      void act(() => voteOnNews(it.id, v));
    },
    onReason: (key, note) =>
      act(() => voteOnNews(it.id, -1, { key, note })).then((ok) => {
        if (ok) setReasonFor((open) => (open === it.id ? null : open));
        return ok;
      }),
    onReasonOpen: () => setReasonFor(it.id),
    onReasonClose: closeReason,
    onOpen: (url) => {
      setReasonFor(null);
      void act(() => recordNewsOpen(it.id, url));
    },
  });

  // Source filter applies client-side; the API doesn't take a source list
  // and adding it would mean a schema change.
  const filtered = useMemo(() => {
    if (!items.data) return [];
    if (selectedSources.length === 0) return items.data;
    const set = new Set(selectedSources);
    return items.data.filter((it) => set.has(it.source_name));
  }, [items.data, selectedSources]);

  const { hero, notable, rest } = useMemo(() => splitItems(orderForDisplay(filtered)), [filtered]);
  const shared = useMemo(() => sharedEvents(filtered), [filtered]);
  const card = (it: NewsItem, variant: "hero" | "notable" | "rest") => (
    <NewsCard
      key={it.id}
      item={it}
      variant={variant}
      reasonOpen={reasonFor === it.id}
      sharedEvent={!!it.event_slug && shared.has(it.event_slug)}
      actions={actionsFor(it)}
    />
  );

  return (
    <PageShell
      title={
        <>
          news <span className="text-[var(--color-nucleus-faint)]">/ feed</span>
        </>
      }
      actions={
        <button
          onClick={() => { items.refetch(); sources.refetch(); brief.refetch(); }}
          className="flex items-center gap-1.5 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-2.5 py-1 text-xs text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
        >
          <RefreshCw size={12} strokeWidth={1.75} />
          refresh
        </button>
      }
    >
      <div className="mb-6 flex flex-wrap items-center gap-3 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-4 py-2.5 text-sm">
        <label className="flex items-center gap-2 text-[var(--color-nucleus-faint)]">
          <span className="text-xs">fetch_date</span>
          <input
            type="date"
            value={fetchDate}
            onChange={(e) => setFetchDate(e.target.value)}
            className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-1.5 py-1 text-xs text-[var(--color-nucleus-text)] [color-scheme:dark]"
          />
        </label>
        <label className="flex items-center gap-2 text-[var(--color-nucleus-faint)]">
          <span className="text-xs">min_score</span>
          <input
            type="range"
            min={0}
            max={1}
            step={0.05}
            value={minScore}
            onChange={(e) => setMinScore(Number(e.target.value))}
            className="accent-[var(--color-nucleus-accent)]"
          />
          <span className="w-10 text-right text-xs tabular-nums text-[var(--color-nucleus-text)]">
            {minScore.toFixed(2)}
          </span>
        </label>
        {sources.data && (
          <FilterDropdown
            label="sources"
            options={sources.data.map((s) => ({
              value: s.name,
              label: s.name,
              meta: s.last_error ? (
                <AlertTriangle size={10} strokeWidth={1.75} className="text-[var(--color-status-down)]" />
              ) : !s.enabled ? (
                <span className="text-[10px] text-[var(--color-nucleus-faint)]">off</span>
              ) : null,
            }))}
            selected={selectedSources}
            onChange={setSelectedSources}
          />
        )}
        <div className="ml-auto text-xs text-[var(--color-nucleus-faint)]">
          {items.data ? `${filtered.length} of ${items.data.length} items` : items.loading ? "fetching…" : items.error ?? ""}
        </div>
      </div>

      {actionError && (
        <div className="mb-4 rounded border border-[var(--color-status-down)] bg-[var(--color-nucleus-surface)] px-3 py-2 text-sm text-[var(--color-status-down)]">
          {actionError}
        </div>
      )}

      {brief.error && (
        <div className="mb-4 text-xs text-[var(--color-status-warn)]">
          The brief could not be loaded: {brief.error}
        </div>
      )}
      {brief.data?.date === fetchDate && brief.data.brief && !brief.error && (
        <NewsBriefTile brief={brief.data.brief} />
      )}

      {items.error ? (
        <div className="rounded border border-[var(--color-status-down)] bg-[var(--color-nucleus-surface)] px-3 py-2 text-sm text-[var(--color-status-down)]">
          {items.error}
        </div>
      ) : !items.data ? (
        <div className="text-sm text-[var(--color-nucleus-faint)]">fetching…</div>
      ) : filtered.length === 0 ? (
        <div className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-3 py-6 text-center text-sm text-[var(--color-nucleus-faint)]">
          no items match this filter combination
        </div>
      ) : (
        <div className="space-y-8">
          {hero && card(hero, "hero")}

          {notable.length > 0 && (
            <section>
              <SectionHeader label={`notable · ${notable.length}`} />
              <div className="grid grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-3">
                {notable.map((it) => card(it, "notable"))}
              </div>
            </section>
          )}

          {rest.length > 0 && (
            <section>
              <SectionHeader label={`others · ${rest.length}`} />
              <div className="grid grid-cols-1 gap-2.5 md:grid-cols-2 xl:grid-cols-4">
                {rest.map((it) => card(it, "rest"))}
              </div>
            </section>
          )}
        </div>
      )}
    </PageShell>
  );
}
