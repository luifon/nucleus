import { RefreshCw, Terminal } from "lucide-react";
import PageShell from "@/components/PageShell";
import SessionTile from "@/components/sessions/SessionTile";
import { usePolling } from "@/lib/hooks";
import { listSessions } from "@/lib/api";

// Poll every 30s so attach/detach state and idle durations drift in
// real time without manual refresh. Visibility-aware (paused while
// the tab is hidden) per usePolling's semantics.
const POLL_MS = 30_000;

export default function SessionsPage() {
  const sessions = usePolling(listSessions, POLL_MS);

  return (
    <PageShell
      title={
        <>
          sessions <span className="text-[var(--color-nucleus-faint)]">/ tmux</span>
        </>
      }
      subtitle="Long-lived nucleus-* tmux sessions hosting bot Claude sessions (Rule 4). Read-only; copy the attach command and run it in your terminal."
      actions={
        <button
          onClick={sessions.refetch}
          className="flex items-center gap-1.5 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-2.5 py-1 text-xs text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
        >
          <RefreshCw size={12} strokeWidth={1.75} />
          refresh
        </button>
      }
    >
      {sessions.error ? (
        <div className="rounded border border-[var(--color-status-down)] bg-[var(--color-nucleus-surface)] px-3 py-2 text-sm text-[var(--color-status-down)]">
          {sessions.error}
        </div>
      ) : !sessions.data ? (
        <div className="text-sm text-[var(--color-nucleus-faint)]">fetching…</div>
      ) : sessions.data.length === 0 ? (
        <div className="flex items-center gap-2 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-3 py-6 text-sm text-[var(--color-nucleus-faint)]">
          <Terminal size={14} strokeWidth={1.75} />
          no nucleus-* tmux sessions found
        </div>
      ) : (
        <div className="grid grid-cols-1 gap-2 md:grid-cols-2 xl:grid-cols-3">
          {sessions.data.map((s) => (
            <SessionTile key={s.name} session={s} />
          ))}
        </div>
      )}
    </PageShell>
  );
}
