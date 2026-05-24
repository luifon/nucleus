import { useState } from "react";
import { ChevronDown, ChevronRight, FileText } from "lucide-react";
import { type DiaryEntry } from "@/lib/api";

// One card per diary entry. Header shows agent badge + date + size;
// click to expand the body inline. Pre-formatted text for v1 — when
// the body becomes a primary read surface, swap in a markdown renderer.

export default function DiaryEntryCard({ entry }: { entry: DiaryEntry }) {
  const [expanded, setExpanded] = useState(false);

  return (
    <article className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)]">
      <button
        onClick={() => setExpanded((v) => !v)}
        className="flex w-full items-center gap-3 px-4 py-2.5 text-left transition-colors hover:bg-[var(--color-nucleus-bg)]"
      >
        {expanded ? (
          <ChevronDown size={14} strokeWidth={1.75} className="shrink-0 text-[var(--color-nucleus-faint)]" />
        ) : (
          <ChevronRight size={14} strokeWidth={1.75} className="shrink-0 text-[var(--color-nucleus-faint)]" />
        )}
        <span className="rounded border border-[var(--color-nucleus-border)] px-2 py-0.5 text-[11px] text-[var(--color-nucleus-accent)]">
          {entry.agent}
        </span>
        <span className="text-sm text-[var(--color-nucleus-text)] tabular-nums">{entry.date}</span>
        <span className="ml-auto flex items-center gap-1 text-[11px] text-[var(--color-nucleus-faint)]">
          <FileText size={10} strokeWidth={1.75} />
          {humanBytes(entry.bytes)}
        </span>
      </button>
      {expanded && (
        <div className="border-t border-[var(--color-nucleus-border)] px-4 py-3">
          <pre className="overflow-x-auto whitespace-pre-wrap text-[12px] leading-relaxed text-[var(--color-nucleus-text)]">
            {entry.body}
          </pre>
        </div>
      )}
    </article>
  );
}

function humanBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}
