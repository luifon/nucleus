import { Link } from "react-router-dom";
import { ChevronRight, Clock, GitPullRequest } from "lucide-react";
import StatusPill from "@/components/StatusPill";
import { type WorkItem } from "@/lib/api";
import { itemHref, stageKind, waitingOn } from "@/lib/work";
import { shortTime } from "@/lib/tasks";

// One row per pipeline item: number, title, stage; below it the repo, the
// eval class, what the operator is expected to do, the PR. The row links
// to the item page (`/work?item=<n>`), where every action lives.
// `compact` is the narrow form of the list column beside an open item.

export default function ItemRow({
  item,
  selected = false,
  compact = false,
  search = null,
}: {
  item: WorkItem;
  selected?: boolean;
  compact?: boolean;
  /** The list's query: the link keeps its filters. */
  search?: URLSearchParams | null;
}) {
  const waiting = waitingOn(item);
  return (
    <Link
      to={itemHref(item.id, search)}
      aria-current={selected ? "page" : undefined}
      className={[
        "flex items-start gap-2 rounded border transition-colors",
        compact ? "px-3 py-2" : "px-4 py-3",
        selected
          ? "border-[var(--color-nucleus-accent)] bg-[color-mix(in_srgb,var(--color-nucleus-accent)_8%,var(--color-nucleus-surface))]"
          : "border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] hover:border-[var(--color-nucleus-faint)]",
      ].join(" ")}
    >
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <code className="shrink-0 text-xs text-[var(--color-nucleus-faint)]">#{item.id}</code>
          <div className={`min-w-0 flex-1 truncate text-[var(--color-nucleus-text)] ${compact ? "text-xs" : "text-sm"}`} title={item.title}>
            {item.title}
          </div>
          <StatusPill kind={stageKind(item)}>{item.stage.toUpperCase()}</StatusPill>
        </div>
        <div className="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[11px] text-[var(--color-nucleus-faint)]">
          {!compact && <span className="rounded border border-[var(--color-nucleus-border)] px-1.5 py-px text-[10px]">{item.repo}</span>}
          {!compact && item.classification && <span>{item.classification}</span>}
          {waiting && <span className="text-[var(--color-nucleus-accent)]">{waiting}</span>}
          {item.pr_url && (
            <span className="flex items-center gap-1">
              <GitPullRequest size={9} strokeWidth={2} />
              draft PR
            </span>
          )}
          {!compact && (
            <span className="flex items-center gap-1" title={item.created_at}>
              <Clock size={9} strokeWidth={2} />
              {shortTime(item.created_at)}
            </span>
          )}
        </div>
      </div>
      {!compact && <ChevronRight size={14} strokeWidth={1.75} className="mt-1 shrink-0 text-[var(--color-nucleus-faint)]" />}
    </Link>
  );
}
