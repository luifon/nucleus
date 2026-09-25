import { type ReactNode } from "react";
import { useFetch } from "@/lib/hooks";
import { getTaskDetail, type TaskEvent } from "@/lib/api";
import { clockTime, deliveryState, shortTime } from "@/lib/tasks";

// Expanded view of one task: brief, progress log, links, result or
// error, and the session the worker ran it in. Fetched when the row
// opens and again whenever `version` changes (the parent derives it
// from the list row, so a list refresh that changes the task also
// refreshes this panel).

const EVENT_TONE: Partial<Record<TaskEvent["kind"], string>> = {
  failed: "text-[var(--color-status-down)]",
  interrupted: "text-[var(--color-status-down)]",
  delivery_failed: "text-[var(--color-status-down)]",
  runtime_guard: "text-[var(--color-status-warn)]",
  done: "text-[var(--color-status-ok)]",
  delivered: "text-[var(--color-status-ok)]",
};

export default function TaskDetailPanel({ taskId, version }: { taskId: string; version: string }) {
  const detail = useFetch((signal) => getTaskDetail(taskId, signal), [taskId, version]);

  if (detail.error && !detail.data) {
    return (
      <div className="border-t border-[var(--color-nucleus-border)] px-4 py-3 text-xs text-[var(--color-status-down)]">
        {detail.error}
      </div>
    );
  }
  if (!detail.data) {
    return (
      <div className="border-t border-[var(--color-nucleus-border)] px-4 py-3 text-xs text-[var(--color-nucleus-faint)]">
        fetching…
      </div>
    );
  }

  const { task, links } = detail.data;
  // The API returns events in insertion order; sort defensively so the
  // newest entry is always last.
  const events = [...detail.data.events].sort((a, b) => a.at.localeCompare(b.at) || a.id - b.id);

  return (
    <div className="space-y-4 border-t border-[var(--color-nucleus-border)] px-4 py-3 text-xs">
      <Field label="id">
        <code className="break-all text-[var(--color-nucleus-text)]">{task.id}</code>
      </Field>

      <Field label="brief">
        {task.brief.trim() ? (
          <Pre>{task.brief}</Pre>
        ) : (
          <span className="text-[var(--color-nucleus-faint)]">—</span>
        )}
      </Field>

      <Field label={`progress (${events.length})`}>
        {events.length === 0 ? (
          <span className="text-[var(--color-nucleus-faint)]">no events recorded</span>
        ) : (
          <ul className="space-y-0.5">
            {events.map((e) => (
              <li key={e.id} className="flex gap-2">
                <span className="shrink-0 text-[var(--color-nucleus-faint)]" title={e.at}>
                  {clockTime(e.at)}
                </span>
                <span className={`w-32 shrink-0 ${EVENT_TONE[e.kind] ?? "text-[var(--color-nucleus-accent)]"}`}>
                  {e.kind}
                </span>
                <span className="min-w-0 flex-1 whitespace-pre-wrap break-words text-[var(--color-nucleus-text)]">
                  {e.message}
                </span>
              </li>
            ))}
          </ul>
        )}
      </Field>

      {links.length > 0 && (
        <Field label="links">
          <ul className="space-y-0.5">
            {links.map((l) => (
              <li key={`${l.rel}|${l.target}`} className="flex gap-2">
                <span className="w-32 shrink-0 text-[var(--color-nucleus-faint)]">{l.rel}</span>
                <span className="min-w-0 flex-1 break-all text-[var(--color-nucleus-text)]">{l.target}</span>
              </li>
            ))}
          </ul>
        </Field>
      )}

      {task.error && (
        <Field label="error">
          <Pre tone="text-[var(--color-status-down)]">{task.error}</Pre>
        </Field>
      )}

      {task.result && (
        <Field label="result">
          <Pre>{task.result}</Pre>
        </Field>
      )}

      <Field label="session">
        <div className="flex flex-wrap gap-x-4 gap-y-0.5 text-[var(--color-nucleus-faint)]">
          <span>
            window <code className="text-[var(--color-nucleus-text)]">{task.tmux_window ?? "—"}</code>
          </span>
          <span>
            session <code className="text-[var(--color-nucleus-text)]">{task.session_id ?? "—"}</code>
          </span>
          {task.delivered_at ? (
            <span>delivered {shortTime(task.delivered_at)}</span>
          ) : (
            !task.delivery_failed_at &&
            task.delivery_queued_at && <span>queued for delivery {shortTime(task.delivery_queued_at)}</span>
          )}
        </div>
      </Field>

      {deliveryState(task) === "given-up" && (
        <Field label="delivery given up">
          <div className="mb-1 text-[11px] text-[var(--color-nucleus-faint)]">
            {shortTime(task.delivery_failed_at ?? "")} — not sent again; the operator was sent one note. The
            result above is the full text.
          </div>
          <Pre tone="text-[var(--color-status-down)]">{task.delivery_error ?? "no reason recorded"}</Pre>
        </Field>
      )}
    </div>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <section>
      <div className="mb-1 text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)] opacity-70">
        {label}
      </div>
      {children}
    </section>
  );
}

function Pre({ children, tone = "text-[var(--color-nucleus-text)]" }: { children: string; tone?: string }) {
  return (
    <pre
      className={`max-h-80 overflow-auto whitespace-pre-wrap break-words rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-3 py-2 font-mono text-xs ${tone}`}
    >
      {children}
    </pre>
  );
}
