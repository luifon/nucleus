import { useMemo, useState } from "react";
import { RefreshCw, BookOpen } from "lucide-react";
import PageShell from "@/components/PageShell";
import Select from "@/components/Select";
import DiaryEntryCard from "@/components/diary/DiaryEntryCard";
import { useFetch } from "@/lib/hooks";
import { listDiaryAgents, listRecentDiary } from "@/lib/api";

const ALL = "__all__";

export default function DiaryPage() {
  const [agent, setAgent] = useState<string>(ALL);

  const agents = useFetch(listDiaryAgents);
  const entries = useFetch(
    () => listRecentDiary({ agent: agent === ALL ? undefined : agent, limit: 30 }),
    [agent],
  );

  const options = useMemo(() => {
    const base = [{ value: ALL, label: "all agents" }];
    if (!agents.data) return base;
    return base.concat(
      agents.data.map((a) => ({
        value: a.name,
        label: `${a.name} (${a.entry_count})`,
      })),
    );
  }, [agents.data]);

  return (
    <PageShell
      title={
        <>
          diary <span className="text-[var(--color-nucleus-faint)]">/ per-agent log</span>
        </>
      }
      actions={
        <button
          onClick={() => { agents.refetch(); entries.refetch(); }}
          className="flex items-center gap-1.5 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-2.5 py-1 text-xs text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
        >
          <RefreshCw size={12} strokeWidth={1.75} />
          refresh
        </button>
      }
    >
      <div className="mb-5 flex flex-wrap items-center gap-4 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-4 py-2.5">
        <Select label="agent" options={options} value={agent} onChange={setAgent} />
        <div className="ml-auto text-xs text-[var(--color-nucleus-faint)]">
          {entries.data
            ? `${entries.data.length} ${entries.data.length === 1 ? "entry" : "entries"}`
            : entries.loading
              ? "fetching…"
              : (entries.error ?? "")}
        </div>
      </div>

      {entries.error ? (
        <div className="rounded border border-[var(--color-status-down)] bg-[var(--color-nucleus-surface)] px-3 py-2 text-sm text-[var(--color-status-down)]">
          {entries.error}
        </div>
      ) : !entries.data ? (
        <div className="text-sm text-[var(--color-nucleus-faint)]">fetching…</div>
      ) : entries.data.length === 0 ? (
        <div className="flex items-center gap-2 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-3 py-6 text-sm text-[var(--color-nucleus-faint)]">
          <BookOpen size={14} strokeWidth={1.75} />
          no entries{agent !== ALL ? ` for ${agent}` : ""}
        </div>
      ) : (
        <ul className="space-y-2">
          {entries.data.map((e) => (
            <li key={`${e.agent}-${e.date}`}>
              <DiaryEntryCard entry={e} />
            </li>
          ))}
        </ul>
      )}
    </PageShell>
  );
}
