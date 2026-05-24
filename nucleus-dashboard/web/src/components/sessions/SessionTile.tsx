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
  BookOpen,
} from "lucide-react";
import { type TmuxSession } from "@/lib/api";

// One tile per tmux session. Honest minimum per ADR-015 follow-up:
// liveness + attach affordance + link to where the real output lives
// (the diary). No pane preview — claude windows get killed on exit
// (claude_session.rs:149), so the bare session keeps only its default
// `zsh` window which has nothing useful in it. ADR-016 (drafted)
// proposes the unified agent registry + log capture that would let
// this surface show meaningful live state.

const IDLE_WARN_HOURS = 24;

export default function SessionTile({ session }: { session: TmuxSession }) {
  const [copied, setCopied] = useState(false);
  const idleSec = Math.max(0, Math.floor(Date.now() / 1000) - session.activity_unix);
  const idleClass =
    session.attached === 1
      ? "text-[var(--color-status-ok)]"
      : idleSec > IDLE_WARN_HOURS * 3600
        ? "text-[var(--color-status-warn)]"
        : "text-[var(--color-nucleus-faint)]";

  const attachCmd = `tmux attach -t ${session.name}`;
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(attachCmd);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard API needs a secure context */
    }
  };

  // Strip the `nucleus-` prefix for display + diary mapping. Some
  // sessions (e.g. dashboard-title, test-smoke) won't have a
  // matching diary; the diary page handles "no entries" cleanly.
  const short = session.name.replace(/^nucleus-/, "");

  return (
    <article className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] p-3.5">
      <div className="flex items-start gap-3">
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
        <div className="flex shrink-0 flex-col items-stretch gap-1">
          <button
            onClick={copy}
            title={attachCmd}
            className="flex items-center justify-center gap-1 rounded border border-[var(--color-nucleus-border)] px-2 py-1 text-[11px] text-[var(--color-nucleus-faint)] hover:border-[var(--color-nucleus-accent)] hover:text-[var(--color-nucleus-accent)]"
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
          </button>
          <Link
            to={`/diary?agent=${encodeURIComponent(short)}`}
            title={`see ${short} diary`}
            className="flex items-center justify-center gap-1 rounded border border-[var(--color-nucleus-border)] px-2 py-1 text-[11px] text-[var(--color-nucleus-faint)] hover:border-[var(--color-nucleus-accent)] hover:text-[var(--color-nucleus-accent)]"
          >
            <BookOpen size={11} strokeWidth={1.75} />
            diary
          </Link>
        </div>
      </div>
    </article>
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
