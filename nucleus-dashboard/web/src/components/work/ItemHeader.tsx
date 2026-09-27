import { useState } from "react";
import { Link } from "react-router-dom";
import { ChevronLeft, ExternalLink, GitBranch, GitPullRequest, RotateCcw, X } from "lucide-react";
import InlineConfirm from "@/components/InlineConfirm";
import StatusPill from "@/components/StatusPill";
import { cancelItem, retryItem, type WorkEvent, type WorkItem } from "@/lib/api";
import { canCancelItem, canRetry, stageKind, waitingOn } from "@/lib/work";
import { ActionButton } from "./parts";

// Top of the item page: number, title and stage; the source issue, the
// pull request, the branch and the test result; what the operator is
// expected to do; the item-wide actions (retry, cancel) and the last
// error. On a phone it carries the way back to the list.

export default function ItemHeader({
  item,
  event,
  busy,
  act,
}: {
  item: WorkItem;
  event: WorkEvent;
  busy: boolean;
  act: (fn: () => Promise<WorkItem>) => Promise<boolean>;
}) {
  const [confirmCancel, setConfirmCancel] = useState(false);
  const waiting = waitingOn(item);
  const retry = canRetry(item);
  const cancel = canCancelItem(item);

  return (
    <header className="shrink-0 border-b border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)]">
      <div className="px-4 pb-2.5 pt-3 md:px-5">
        <div className="flex items-start gap-2">
          <Link
            to="/work"
            aria-label="back to items"
            className="-ml-1 mt-0.5 shrink-0 text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)] lg:hidden"
          >
            <ChevronLeft size={18} strokeWidth={1.75} />
          </Link>
          <h1 className="min-w-0 flex-1 text-sm leading-snug text-[var(--color-nucleus-text)] md:text-base">
            <code className="mr-2 text-[var(--color-nucleus-faint)]">#{item.id}</code>
            <span className="break-words">{item.title}</span>
          </h1>
          <span className="shrink-0 pt-0.5">
            <StatusPill kind={stageKind(item)}>{item.stage.toUpperCase()}</StatusPill>
          </span>
        </div>

        <div className="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-[var(--color-nucleus-faint)]">
          <span className="rounded border border-[var(--color-nucleus-border)] px-1.5 py-px text-[10px]">{item.repo}</span>
          {event.url && (
            <a href={event.url} target="_blank" rel="noreferrer" className="flex items-center gap-1 hover:text-[var(--color-nucleus-accent)]">
              <ExternalLink size={10} strokeWidth={2} />
              issue {event.external_id}
            </a>
          )}
          {item.pr_url && (
            <a href={item.pr_url} target="_blank" rel="noreferrer" className="flex items-center gap-1 text-[var(--color-nucleus-accent)] hover:underline">
              <GitPullRequest size={10} strokeWidth={2} />
              pull request
            </a>
          )}
          {item.branch && (
            <span className="flex min-w-0 items-center gap-1" title={item.branch}>
              <GitBranch size={10} strokeWidth={2} />
              <code className="truncate">{item.branch}</code>
            </span>
          )}
          {item.tests_status && (
            <span>
              tests{" "}
              <span className={item.tests_status === "passed" ? "text-[var(--color-status-ok)]" : "text-[var(--color-status-down)]"}>
                {item.tests_status}
              </span>
            </span>
          )}
          {item.classification && <span>{item.classification}</span>}
        </div>

        {(waiting || retry || cancel) && (
          <div className="mt-2 flex flex-wrap items-center gap-2 text-xs">
            {waiting && <span className="min-w-0 flex-1 text-[var(--color-nucleus-accent)]">{waiting}</span>}
            <div className="ml-auto flex shrink-0 items-center gap-1.5">
              {retry && (
                <ActionButton onClick={() => void act(() => retryItem(item.id))} disabled={busy}>
                  <RotateCcw size={11} strokeWidth={1.75} />
                  retry
                </ActionButton>
              )}
              {cancel && (
                <ActionButton tone="down" onClick={() => setConfirmCancel(true)} disabled={busy || confirmCancel}>
                  <X size={11} strokeWidth={1.75} />
                  cancel
                </ActionButton>
              )}
            </div>
          </div>
        )}

        {item.stale_reason ? (
          <ErrorText>{`${item.stale_reason}\nNothing more is done for this item. Remove and add the label again on the issue to start a new item from its current text.`}</ErrorText>
        ) : (
          item.error && (
            <ErrorText
              label={
                item.stage === "failed"
                  ? `failed in ${item.failed_stage ?? "?"}`
                  : item.stage === "blocked"
                    ? `blocked in ${item.failed_stage ?? "?"}`
                    : "last error"
              }
            >
              {item.error}
            </ErrorText>
          )
        )}
      </div>

      {confirmCancel && cancel && (
        <InlineConfirm
          className="px-4 py-2 md:px-5"
          message={`Cancel item #${item.id}? Its running task stops. A cancelled item cannot be resumed.`}
          confirmLabel={busy ? "cancelling…" : "cancel item"}
          busy={busy}
          onConfirm={() => void act(() => cancelItem(item.id)).then((ok) => ok && setConfirmCancel(false))}
          onCancel={() => setConfirmCancel(false)}
        />
      )}
    </header>
  );
}

function ErrorText({ label, children }: { label?: string; children: string }) {
  return (
    <div className="mt-2 text-xs">
      {label && <div className="mb-0.5 text-[10px] uppercase tracking-widest text-[var(--color-status-down)] opacity-80">{label}</div>}
      <pre className="max-h-28 overflow-auto whitespace-pre-wrap break-words font-mono text-[var(--color-status-down)]">{children}</pre>
    </div>
  );
}
