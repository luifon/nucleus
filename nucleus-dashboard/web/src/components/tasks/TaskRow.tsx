import { useState } from "react";
import { ChevronDown, ChevronRight, Clock, Timer, X } from "lucide-react";
import InlineConfirm from "@/components/InlineConfirm";
import StatusPill from "@/components/StatusPill";
import TaskDetailPanel from "@/components/tasks/TaskDetailPanel";
import { cancelTask, type Task } from "@/lib/api";
import { canCancel, deliveryState, shortId, shortTime, taskDuration, taskStatusKind } from "@/lib/tasks";

// One row per background task. The header line shows short id, title and
// status; the secondary line shows origin, requester, runtime and
// creation time. Clicking the row toggles the detail panel. Queued and
// running tasks get a cancel action behind an inline confirmation strip.

export default function TaskRow({
  task,
  now,
  onChange,
}: {
  task: Task;
  /** Epoch ms used for the runtime of tasks that are still running. */
  now: number;
  /** Called with the task returned by a successful cancel so the parent
   *  list can replace the row before its next refresh. */
  onChange: (updated: Task) => void;
}) {
  const [open, setOpen] = useState(false);
  const [confirmingCancel, setConfirmingCancel] = useState(false);
  const [pending, setPending] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const cancellable = canCancel(task);
  const duration = taskDuration(task, now);
  const Chevron = open ? ChevronDown : ChevronRight;

  const runCancel = async () => {
    setPending(true);
    setErr(null);
    try {
      onChange(await cancelTask(task.id));
      setConfirmingCancel(false);
    } catch (e) {
      setErr(String(e));
    } finally {
      setPending(false);
    }
  };

  return (
    <article className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)]">
      <div className="flex items-start gap-3 px-4 py-3">
        <button
          onClick={() => setOpen((v) => !v)}
          aria-expanded={open}
          className="flex min-w-0 flex-1 items-start gap-2 text-left"
        >
          <Chevron
            size={14}
            strokeWidth={1.75}
            className="mt-1 shrink-0 text-[var(--color-nucleus-faint)]"
          />
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2">
              <code className="shrink-0 text-xs text-[var(--color-nucleus-faint)]" title={task.id}>
                {shortId(task.id)}
              </code>
              <div className="min-w-0 flex-1 truncate text-sm text-[var(--color-nucleus-text)]" title={task.title}>
                {task.title || <span className="italic text-[var(--color-nucleus-faint)]">untitled</span>}
              </div>
              {deliveryState(task) === "given-up" && (
                <span title={task.delivery_error ?? undefined}>
                  <StatusPill kind="down">NOT DELIVERED</StatusPill>
                </span>
              )}
              <StatusPill kind={taskStatusKind(task.status)}>{task.status.toUpperCase()}</StatusPill>
            </div>

            <div className="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[11px] text-[var(--color-nucleus-faint)]">
              <span className="rounded border border-[var(--color-nucleus-border)] px-1.5 py-px text-[10px]">
                {task.origin}
              </span>
              <span>by {task.requested_by}</span>
              {task.kind && <span>{task.kind}</span>}
              <span className="flex items-center gap-1" title={task.started_at ?? "not started"}>
                <Timer size={9} strokeWidth={2} />
                {duration ?? "not started"}
              </span>
              <span className="flex items-center gap-1" title={task.created_at}>
                <Clock size={9} strokeWidth={2} />
                created {shortTime(task.created_at)}
              </span>
            </div>

            {err && <div className="mt-2 text-xs text-[var(--color-status-down)]">{err}</div>}
          </div>
        </button>

        {cancellable && (
          <div className="flex shrink-0 items-center gap-1">
            <button
              onClick={() => setConfirmingCancel(true)}
              disabled={pending || confirmingCancel}
              title="cancel"
              className="rounded border border-[var(--color-nucleus-border)] px-1.5 py-1 text-[var(--color-nucleus-faint)] transition-colors hover:border-[var(--color-status-down)] hover:text-[var(--color-status-down)] disabled:opacity-40"
            >
              <X size={12} strokeWidth={1.75} />
            </button>
          </div>
        )}
      </div>

      {confirmingCancel && cancellable && (
        <InlineConfirm
          message={`Cancel task ${shortId(task.id)}? The worker stops its session. A cancelled task cannot be resumed.`}
          confirmLabel={pending ? "cancelling…" : "cancel task"}
          busy={pending}
          onConfirm={() => void runCancel()}
          onCancel={() => setConfirmingCancel(false)}
        />
      )}

      {open && (
        <TaskDetailPanel
          taskId={task.id}
          /* Refetch the detail whenever the list reports a change to the task. */
          version={`${task.status}|${task.heartbeat_at ?? ""}|${task.finished_at ?? ""}|${deliveryState(task) ?? ""}`}
        />
      )}
    </article>
  );
}
