import { useEffect, useMemo, useState } from "react";
import { RefreshCw, ListChecks, MessagesSquare } from "lucide-react";
import PageShell from "@/components/PageShell";
import Tabs from "@/components/Tabs";
import TaskRow from "@/components/tasks/TaskRow";
import TurnRowItem from "@/components/tasks/TurnRowItem";
import { useFetch, usePollWhile, type FetchState } from "@/lib/hooks";
import { listTasks, listTurns, type Task, type TurnRow } from "@/lib/api";
import { isActiveTask } from "@/lib/tasks";

// Background tasks (ADR-033 task ledger) and the WhatsApp turn log.
// Both lists refresh every POLL_MS only while something in them is still
// in progress; finished lists stay static until the operator refreshes.
const POLL_MS = 5_000;

type TabValue = "tasks" | "turns";

export default function TasksPage() {
  const [tab, setTab] = useState<TabValue>("tasks");

  const tasks = useFetch((signal) => listTasks({ all: true }, signal));
  const turns = useFetch((signal) => listTurns(signal));

  // Row returned by a cancel, shown until the next list response.
  const [optimistic, setOptimistic] = useState<Record<string, Task>>({});
  useEffect(() => setOptimistic({}), [tasks.data]);
  const onChange = (t: Task) => {
    setOptimistic((m) => ({ ...m, [t.id]: t }));
    tasks.refetch();
  };

  const merged = useMemo(
    () => (tasks.data ?? []).map((t) => optimistic[t.id] ?? t),
    [tasks.data, optimistic],
  );

  const activeCount = merged.filter((t) => isActiveTask(t.status)).length;
  const turnsRunning = (turns.data ?? []).some((t) => t.status === "running");

  usePollWhile(tasks.refetch, activeCount > 0, POLL_MS);
  usePollWhile(turns.refetch, turnsRunning, POLL_MS);

  // Read at render time; the 5s polls re-render while anything runs, so
  // running durations advance with each refresh.
  const now = Date.now();

  return (
    <PageShell
      title={
        <>
          tasks <span className="text-[var(--color-nucleus-faint)]">/ background work</span>
        </>
      }
      actions={
        <button
          onClick={() => {
            tasks.refetch();
            turns.refetch();
          }}
          className="flex items-center gap-1.5 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-2.5 py-1 text-xs text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
        >
          <RefreshCw size={12} strokeWidth={1.75} />
          refresh
        </button>
      }
    >
      <Tabs
        tabs={[
          { value: "tasks", label: "tasks", count: tasks.data ? merged.length : null },
          { value: "turns", label: "turns", count: turns.data?.length ?? null },
        ]}
        value={tab}
        onChange={setTab}
      />

      {tab === "tasks" ? (
        <TasksTab tasks={tasks} merged={merged} activeCount={activeCount} now={now} onChange={onChange} />
      ) : (
        <TurnsTab turns={turns} now={now} />
      )}
    </PageShell>
  );
}

function TasksTab({
  tasks,
  merged,
  activeCount,
  now,
  onChange,
}: {
  tasks: FetchState<Task[]>;
  merged: Task[];
  activeCount: number;
  now: number;
  onChange: (t: Task) => void;
}) {
  if (tasks.error && !tasks.data) return <ErrorBox message={tasks.error} />;
  if (!tasks.data) return <div className="text-sm text-[var(--color-nucleus-faint)]">fetching…</div>;
  if (merged.length === 0) {
    return (
      <EmptyBox Icon={ListChecks}>no tasks recorded yet</EmptyBox>
    );
  }
  return (
    <>
      <div className="mb-5 flex flex-wrap items-center gap-2 text-xs text-[var(--color-nucleus-faint)]">
        <span>
          <span className={activeCount > 0 ? "text-[var(--color-status-warn)]" : ""}>{activeCount}</span> queued or
          running
        </span>
        <span>·</span>
        <span>{merged.length} total</span>
        {activeCount > 0 && <span className="ml-auto">refreshing every {POLL_MS / 1000}s</span>}
        {tasks.error && <span className="ml-auto text-[var(--color-status-down)]">{tasks.error}</span>}
      </div>
      <ul className="space-y-2">
        {merged.map((t) => (
          <li key={t.id}>
            <TaskRow task={t} now={now} onChange={onChange} />
          </li>
        ))}
      </ul>
    </>
  );
}

function TurnsTab({ turns, now }: { turns: FetchState<TurnRow[]>; now: number }) {
  if (turns.error && !turns.data) return <ErrorBox message={turns.error} />;
  if (!turns.data) return <div className="text-sm text-[var(--color-nucleus-faint)]">fetching…</div>;
  if (turns.data.length === 0) {
    return <EmptyBox Icon={MessagesSquare}>no turns recorded yet</EmptyBox>;
  }
  return (
    <ul className="space-y-1.5">
      {turns.data.map((t) => (
        <TurnRowItem key={t.id} turn={t} now={now} />
      ))}
    </ul>
  );
}

function ErrorBox({ message }: { message: string }) {
  return (
    <div className="rounded border border-[var(--color-status-down)] bg-[var(--color-nucleus-surface)] px-3 py-2 text-sm text-[var(--color-status-down)]">
      {message}
    </div>
  );
}

function EmptyBox({
  Icon,
  children,
}: {
  Icon: React.ComponentType<{ size?: number; strokeWidth?: number }>;
  children: React.ReactNode;
}) {
  return (
    <div className="flex items-center gap-2 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-3 py-6 text-sm text-[var(--color-nucleus-faint)]">
      <Icon size={14} strokeWidth={1.75} />
      {children}
    </div>
  );
}
