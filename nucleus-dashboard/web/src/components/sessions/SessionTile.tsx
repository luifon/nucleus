import { useState } from "react";
import { Link } from "react-router-dom";
import {
  Terminal,
  Clock,
  Copy,
  Check,
  Activity,
  CircleDashed,
  AppWindow,
  ChevronDown,
  ChevronRight,
  RefreshCw,
  BookOpen,
} from "lucide-react";
import { type TmuxSession, captureSessionPane } from "@/lib/api";

// One tile per tmux session. Header row is always visible (name +
// state + idle/uptime + attach button). Click the chevron / header
// to expand: shows window list + a 20-line tmux capture-pane preview
// of the session's active pane.
//
// Read-only — operator copies the attach command and runs it in their
// own terminal. Per ADR-015 the dashboard never attaches/kills.

const IDLE_WARN_HOURS = 24;

export default function SessionTile({ session }: { session: TmuxSession }) {
  const [copied, setCopied] = useState(false);
  const [expanded, setExpanded] = useState(false);
  const [capture, setCapture] = useState<string | null>(null);
  const [captureErr, setCaptureErr] = useState<string | null>(null);
  const [reloading, setReloading] = useState(false);

  const idleSec = Math.max(0, Math.floor(Date.now() / 1000) - session.activity_unix);
  const idleClass =
    session.attached === 1
      ? "text-[var(--color-status-ok)]"
      : idleSec > IDLE_WARN_HOURS * 3600
        ? "text-[var(--color-status-warn)]"
        : "text-[var(--color-nucleus-faint)]";

  const attachCmd = `tmux attach -t ${session.name}`;
  const copy = async (e: React.MouseEvent) => {
    e.stopPropagation();
    try {
      await navigator.clipboard.writeText(attachCmd);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard API needs a secure context */
    }
  };

  const loadCapture = async () => {
    setReloading(true);
    setCaptureErr(null);
    try {
      setCapture(await captureSessionPane(session.name, 20));
    } catch (e) {
      setCaptureErr(String(e));
    } finally {
      setReloading(false);
    }
  };

  const toggle = async () => {
    const next = !expanded;
    setExpanded(next);
    if (next && capture === null && !captureErr) {
      await loadCapture();
    }
  };

  // Strip the `nucleus-` prefix for display — every tile is a
  // nucleus-* session by construction.
  const short = session.name.replace(/^nucleus-/, "");

  return (
    <article className="overflow-hidden rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)]">
      <button
        onClick={toggle}
        className="flex w-full items-start gap-3 p-3.5 text-left transition-colors hover:bg-[var(--color-nucleus-bg)]"
      >
        {expanded ? (
          <ChevronDown size={14} strokeWidth={1.75} className="mt-0.5 shrink-0 text-[var(--color-nucleus-faint)]" />
        ) : (
          <ChevronRight size={14} strokeWidth={1.75} className="mt-0.5 shrink-0 text-[var(--color-nucleus-faint)]" />
        )}
        <Terminal
          size={14}
          strokeWidth={1.75}
          className="mt-0.5 shrink-0 text-[var(--color-nucleus-accent)]"
        />
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="truncate text-sm text-[var(--color-nucleus-text)]" title={session.name}>
              {short}
            </span>
            {session.attached === 1 ? (
              <span className="flex items-center gap-1 text-[10px] text-[var(--color-status-ok)]">
                <Activity size={9} strokeWidth={2} />
                attached
              </span>
            ) : (
              <span className="flex items-center gap-1 text-[10px] text-[var(--color-nucleus-faint)]">
                <CircleDashed size={9} strokeWidth={2} />
                detached
              </span>
            )}
          </div>
          <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[11px]">
            <span className={`flex items-center gap-1 ${idleClass}`} title={fullTime(session.activity_unix)}>
              <Clock size={9} strokeWidth={2} />
              idle {relDuration(idleSec)}
            </span>
            <span
              className="flex items-center gap-1 text-[var(--color-nucleus-faint)]"
              title={`created ${fullTime(session.created_unix)}`}
            >
              <CircleDashed size={9} strokeWidth={2} />
              up {relDuration(Math.floor(Date.now() / 1000) - session.created_unix)}
            </span>
            <span className="flex items-center gap-1 text-[var(--color-nucleus-faint)]">
              <AppWindow size={9} strokeWidth={2} />
              {session.windows.length}w
            </span>
          </div>
        </div>
        <span
          onClick={copy}
          role="button"
          tabIndex={0}
          title={attachCmd}
          className="flex shrink-0 items-center gap-1 rounded border border-[var(--color-nucleus-border)] px-2 py-1 text-[11px] text-[var(--color-nucleus-faint)] hover:border-[var(--color-nucleus-accent)] hover:text-[var(--color-nucleus-accent)]"
        >
          {copied ? (
            <>
              <Check size={11} strokeWidth={2} className="text-[var(--color-status-ok)]" />
              copied
            </>
          ) : (
            <>
              <Copy size={11} strokeWidth={1.75} />
              attach
            </>
          )}
        </span>
      </button>

      {expanded && (
        <div className="border-t border-[var(--color-nucleus-border)] px-4 py-3 text-[12px]">
          <div className="mb-3">
            <div className="mb-1 flex items-center gap-1 text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)] opacity-70">
              <AppWindow size={10} strokeWidth={1.75} />
              windows · {session.windows.length}
            </div>
            <ul className="space-y-0.5">
              {session.windows.map((w) => (
                <li
                  key={w.index}
                  className="flex items-center gap-2 text-[11px] text-[var(--color-nucleus-faint)]"
                >
                  <span className="tabular-nums opacity-70">#{w.index}</span>
                  <span className="text-[var(--color-nucleus-text)]">{w.name}</span>
                  <span className="opacity-70">· {w.panes} {w.panes === 1 ? "pane" : "panes"}</span>
                  <span className="ml-auto" title={fullTime(w.activity_unix)}>
                    active {relDuration(Math.max(0, Math.floor(Date.now() / 1000) - w.activity_unix))} ago
                  </span>
                </li>
              ))}
            </ul>
          </div>

          <div>
            <div className="mb-1 flex items-center justify-between gap-2">
              <span className="flex items-center gap-1 text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)] opacity-70">
                <Terminal size={10} strokeWidth={1.75} />
                pane preview · last 20 lines
              </span>
              <button
                onClick={(e) => { e.stopPropagation(); void loadCapture(); }}
                disabled={reloading}
                title="re-capture"
                className="text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)] disabled:opacity-40"
              >
                <RefreshCw size={10} strokeWidth={1.75} className={reloading ? "animate-spin" : ""} />
              </button>
            </div>
            {captureErr ? (
              <div className="text-[11px] text-[var(--color-status-down)]">{captureErr}</div>
            ) : capture === null ? (
              <div className="text-[11px] text-[var(--color-nucleus-faint)]">loading…</div>
            ) : isEffectivelyEmpty(capture) ? (
              <EmptyPaneHint sessionName={session.name} />
            ) : (
              <pre className="max-h-64 overflow-auto rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] p-2 text-[11px] leading-snug text-[var(--color-nucleus-text)]">
                {capture}
              </pre>
            )}
          </div>
        </div>
      )}
    </article>
  );
}

// Most Nucleus tmux sessions are one-shot command vehicles — they
// fire a `claude` invocation, the output streams through the pane
// while running, the prompt returns when the command exits. So the
// preview is only useful WHILE a fire is in flight. Outside of that
// window the pane is just the shell prompt — show a useful hint
// instead of an empty <pre>.
//
// Heuristic: ≤2 non-empty lines AND no occurrence of common
// activity markers (claude prompt chars, error keywords). Tight
// enough to not hide real output if it's just brief.
function isEffectivelyEmpty(text: string): boolean {
  const nonEmpty = text.split("\n").map((l) => l.trim()).filter(Boolean);
  if (nonEmpty.length > 2) return false;
  const joined = nonEmpty.join(" ");
  if (/error|panic|warn|claude|\$|>/i.test(joined) && joined.length > 40) return false;
  return true;
}

function EmptyPaneHint({ sessionName }: { sessionName: string }) {
  const agent = sessionName.replace(/^nucleus-/, "");
  return (
    <div className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] p-3 text-[11px] leading-relaxed text-[var(--color-nucleus-faint)]">
      <p className="mb-2">
        Pane is idle — nothing in scrollback beyond the shell prompt. Nucleus
        tmux sessions are one-shot command vehicles; their actual output goes
        to the per-agent diary, not the pane.
      </p>
      <Link
        to={`/diary?agent=${encodeURIComponent(agent)}`}
        className="inline-flex items-center gap-1.5 text-[var(--color-nucleus-accent)] hover:text-[var(--color-nucleus-text)]"
      >
        <BookOpen size={11} strokeWidth={1.75} />
        see <code>{agent}</code> diary →
      </Link>
    </div>
  );
}

function relDuration(sec: number): string {
  if (sec < 60) return `${sec}s`;
  if (sec < 3600) return `${Math.floor(sec / 60)}m`;
  if (sec < 86400) return `${Math.floor(sec / 3600)}h`;
  return `${Math.floor(sec / 86400)}d`;
}

function fullTime(unix: number): string {
  if (!unix) return "—";
  return new Date(unix * 1000).toLocaleString("en-GB");
}
