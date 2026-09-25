import { useState } from "react";
import { ChevronDown, ChevronRight, Clock, GitPullRequest, RotateCcw, X } from "lucide-react";
import InlineConfirm from "@/components/InlineConfirm";
import StatusPill from "@/components/StatusPill";
import ItemDetailPanel from "@/components/intake/ItemDetailPanel";
import { cancelItem, retryItem, type IntakeItem } from "@/lib/api";
import { canCancelItem, canRetry, stageKind, waitingOn } from "@/lib/intake";
import { shortTime } from "@/lib/tasks";

// One row per pipeline item: number, title, stage; below it the repo, the
// eval class, what the operator is expected to do, the PR. Clicking the row
// opens the detail panel. Open items can be cancelled (inline
// confirmation); failed items can be retried.

export default function ItemRow({
  item,
  now,
  onChange,
}: {
  item: IntakeItem;
  now: number;
  onChange: (updated: IntakeItem) => void;
}) {
  const [open, setOpen] = useState(false);
  const [confirmingCancel, setConfirmingCancel] = useState(false);
  const [pending, setPending] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const Chevron = open ? ChevronDown : ChevronRight;
  const waiting = waitingOn(item);

  const run = async (fn: () => Promise<IntakeItem>) => {
    setPending(true);
    setErr(null);
    try {
      onChange(await fn());
      setConfirmingCancel(false);
    } catch (e) {
      setErr(String(e instanceof Error ? e.message : e));
    } finally {
      setPending(false);
    }
  };

  return (
    <article className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)]">
      <div className="flex items-start gap-3 px-4 py-3">
        <button onClick={() => setOpen((v) => !v)} aria-expanded={open} className="flex min-w-0 flex-1 items-start gap-2 text-left">
          <Chevron size={14} strokeWidth={1.75} className="mt-1 shrink-0 text-[var(--color-nucleus-faint)]" />
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2">
              <code className="shrink-0 text-xs text-[var(--color-nucleus-faint)]">#{item.id}</code>
              <div className="min-w-0 flex-1 truncate text-sm text-[var(--color-nucleus-text)]" title={item.title}>
                {item.title}
              </div>
              <StatusPill kind={stageKind(item)}>{item.stage.toUpperCase()}</StatusPill>
            </div>
            <div className="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[11px] text-[var(--color-nucleus-faint)]">
              <span className="rounded border border-[var(--color-nucleus-border)] px-1.5 py-px text-[10px]">{item.repo}</span>
              {item.classification && <span>{item.classification}</span>}
              {waiting && <span className="text-[var(--color-nucleus-accent)]">{waiting}</span>}
              {item.pr_url && (
                <a
                  href={item.pr_url}
                  target="_blank"
                  rel="noreferrer"
                  onClick={(e) => e.stopPropagation()}
                  className="flex items-center gap-1 hover:text-[var(--color-nucleus-accent)]"
                >
                  <GitPullRequest size={9} strokeWidth={2} />
                  draft PR
                </a>
              )}
              <span className="flex items-center gap-1" title={item.created_at}>
                <Clock size={9} strokeWidth={2} />
                {shortTime(item.created_at)}
              </span>
            </div>
            {err && <div className="mt-2 text-xs text-[var(--color-status-down)]">{err}</div>}
          </div>
        </button>

        <div className="flex shrink-0 items-center gap-1">
          {canRetry(item) && (
            <button
              onClick={() => void run(() => retryItem(item.id))}
              disabled={pending}
              title="retry"
              className="rounded border border-[var(--color-nucleus-border)] px-1.5 py-1 text-[var(--color-nucleus-faint)] transition-colors hover:border-[var(--color-nucleus-accent)] hover:text-[var(--color-nucleus-accent)] disabled:opacity-40"
            >
              <RotateCcw size={12} strokeWidth={1.75} />
            </button>
          )}
          {canCancelItem(item) && (
            <button
              onClick={() => setConfirmingCancel(true)}
              disabled={pending || confirmingCancel}
              title="cancel"
              className="rounded border border-[var(--color-nucleus-border)] px-1.5 py-1 text-[var(--color-nucleus-faint)] transition-colors hover:border-[var(--color-status-down)] hover:text-[var(--color-status-down)] disabled:opacity-40"
            >
              <X size={12} strokeWidth={1.75} />
            </button>
          )}
        </div>
      </div>

      {confirmingCancel && canCancelItem(item) && (
        <InlineConfirm
          message={`Cancel item #${item.id}? Its running task stops and its WhatsApp group is left. A cancelled item cannot be resumed.`}
          confirmLabel={pending ? "cancelling…" : "cancel item"}
          busy={pending}
          onConfirm={() => void run(() => cancelItem(item.id))}
          onCancel={() => setConfirmingCancel(false)}
        />
      )}

      {open && (
        <ItemDetailPanel
          itemId={item.id}
          version={`${item.stage}|${item.updated_at}|${item.current_task_id ?? ""}`}
          now={now}
          onChange={onChange}
        />
      )}
    </article>
  );
}
