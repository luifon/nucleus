import { useMemo, useState } from "react";
import { RefreshCw, AlertTriangle } from "lucide-react";
import PageShell from "@/components/PageShell";
import SectionHeader from "@/components/SectionHeader";
import FilterDropdown from "@/components/FilterDropdown";
import NewsCard from "@/components/NewsCard";
import { useFetch, todayLocal } from "@/lib/hooks";
import {
  listNewsItems,
  listNewsSources,
  voteOnNews,
  type NewsItem,
} from "@/lib/api";

const NOTABLE_THRESHOLD = 0.6;

export default function NewsPage() {
  const [fetchDate, setFetchDate] = useState(todayLocal());
  const [minScore, setMinScore] = useState(0);
  const [selectedSources, setSelectedSources] = useState<string[]>([]);

  const items = useFetch(
    () => listNewsItems({ fetchDate, minScore, limit: 200 }),
    [fetchDate, minScore],
  );
  const sources = useFetch(listNewsSources);

  // Source filter applies client-side; the API doesn't take a source list
  // and adding it would mean a schema change.
  const filtered = useMemo(() => {
    if (!items.data) return [];
    if (selectedSources.length === 0) return items.data;
    const set = new Set(selectedSources);
    return items.data.filter((it) => set.has(it.source_name));
  }, [items.data, selectedSources]);

  const { hero, notable, rest } = useMemo(() => splitItems(filtered), [filtered]);

  return (
    <PageShell
      title={
        <>
          news <span className="text-[var(--color-nucleus-faint)]">/ feed</span>
        </>
      }
      actions={
        <button
          onClick={() => { items.refetch(); sources.refetch(); }}
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
          {hero && (
            <NewsCard
              item={hero}
              variant="hero"
              onVote={(v) => voteOnNews(hero.id, v).then(items.refetch)}
            />
          )}

          {notable.length > 0 && (
            <section>
              <SectionHeader label={`notable · ${notable.length}`} />
              <div className="grid grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-3">
                {notable.map((it) => (
                  <NewsCard
                    key={it.id}
                    item={it}
                    variant="notable"
                    onVote={(v) => voteOnNews(it.id, v).then(items.refetch)}
                  />
                ))}
              </div>
            </section>
          )}

          {rest.length > 0 && (
            <section>
              <SectionHeader label={`others · ${rest.length}`} />
              <div className="grid grid-cols-1 gap-2.5 md:grid-cols-2 xl:grid-cols-4">
                {rest.map((it) => (
                  <NewsCard
                    key={it.id}
                    item={it}
                    variant="rest"
                    onVote={(v) => voteOnNews(it.id, v).then(items.refetch)}
                  />
                ))}
              </div>
            </section>
          )}
        </div>
      )}
    </PageShell>
  );
}

function splitItems(items: NewsItem[]): {
  hero: NewsItem | null;
  notable: NewsItem[];
  rest: NewsItem[];
} {
  if (items.length === 0) return { hero: null, notable: [], rest: [] };
  // Items already come sorted by notable_score DESC from the backend.
  const [hero, ...others] = items;
  const notable: NewsItem[] = [];
  const rest: NewsItem[] = [];
  for (const it of others) {
    if ((it.notable_score ?? 0) >= NOTABLE_THRESHOLD) notable.push(it);
    else rest.push(it);
  }
  return { hero, notable, rest };
}
