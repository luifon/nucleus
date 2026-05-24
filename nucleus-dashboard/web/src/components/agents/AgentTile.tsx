import { useState } from "react";
import { Link } from "react-router-dom";
import {
  Copy,
  Check,
  BookOpen,
  ChevronRight,
  ChevronDown,
  Clock,
  AppWindow,
  ScrollText,
  FileText,
} from "lucide-react";
import StatusPill, { type StatusKind } from "@/components/StatusPill";
import { listRuns, getAgentLog, type AgentView, type RunRow, type AgentLog } from "@/lib/api";

// One tile per registry agent (ADR-016). Liveness is computed server-side
// per the agent's launch mechanism; this just renders it. Expanding fetches
// the run-log (transcript pointers) for tmux agents, or the launchd log tail
// for launchd agents — the raw output that /sessions could never show.

const STATUS_KIND: Record<AgentView["status"], StatusKind> = {
  running: "ok",
  hosted: "ok",
  idle: "idle",
  errored: "down",
  stopped: "down",
  unknown: "warn",
};

export default function AgentTile({ agent }: { agent: AgentView }) {
  const [expanded, setExpanded] = useState(false);
  const [copied, setCopied] = useState(false);
  const [runs, setRuns] = useState<RunRow[] | null>(null);
  const [log, setLog] = useState<AgentLog | null>(null);
  const [detailErr, setDetailErr] = useState<string | null>(null);

  const hasRuns = !!agent.tmux_session;
  const hasLog = !!agent.launchd_label && agent.launch !== "in-process";

  const toggle = async () => {
    const next = !expanded;
    setExpanded(next);
    if (!next) return;
    setDetailErr(null);
    try {
      if (hasRuns && runs === null) setRuns(await listRuns(agent.name));
      if (hasLog && log === null) setLog(await getAgentLog(agent.name));
    } catch (e) {
      setDetailErr(String(e));
    }
  };

  const copyAttach = async () => {
    if (!agent.attach_cmd) return;
    try {
      await navigator.clipboard.writeText(agent.attach_cmd);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard needs a secure context */
    }
  };

  return (
    <article className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] p-3.5">
      <div className="flex items-start gap-3">
        <button
          onClick={toggle}
          className="mt-0.5 shrink-0 text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
          title={expanded ? "collapse" : "expand"}
        >
          {expanded ? (
            <ChevronDown size={14} strokeWidth={1.75} />
          ) : (
            <ChevronRight size={14} strokeWidth={1.75} />
          )}
        </button>

        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-2">
            <StatusPill kind={STATUS_KIND[agent.status]}>
              {statusLabel(agent)}
            </StatusPill>
            <span className="truncate text-sm text-[var(--color-nucleus-text)]" title={agent.name}>
              {agent.name}
            </span>
            {agent.persona_display_name && (
              <span className="text-[11px] text-[var(--color-nucleus-faint)]">
                as {agent.persona_display_name}
              </span>
            )}
          </div>

          <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[11px] text-[var(--color-nucleus-faint)]">
            <span className="text-[var(--color-nucleus-accent)] opacity-80">{agent.launch}</span>
            {agent.runtime && <span>{agent.runtime}</span>}
            {agent.pid != null && <span>pid {agent.pid}</span>}
            {agent.launch === "launchd-cron" && agent.pid == null && agent.last_exit != null && (
              <span className={agent.last_exit === 0 ? "" : "text-[var(--color-status-down)]"}>
                exit {agent.last_exit}
              </span>
            )}
            {agent.live_windows > 0 && (
              <span className="flex items-center gap-1">
                <AppWindow size={9} strokeWidth={2} />
                {agent.live_windows}w live
              </span>
            )}
            {agent.schedule && (
              <span className="flex items-center gap-1" title="schedule (informational; truth is in the plist)">
                <Clock size={9} strokeWidth={2} />
                {agent.schedule}
              </span>
            )}
            {hasRuns && (
              <span title="indexed Claude runs">
                {agent.run_count} run{agent.run_count === 1 ? "" : "s"}
              </span>
            )}
            {agent.last_run_started && (
              <span title={`last run ${fullTime(agent.last_run_started)}`}>
                last {relTime(agent.last_run_started)}
              </span>
            )}
          </div>

          {agent.capabilities.length > 0 && (
            <div className="mt-1 flex flex-wrap gap-1">
              {agent.capabilities.map((c) => (
                <span
                  key={c}
                  className="rounded-sm border border-[var(--color-nucleus-border)] px-1 text-[10px] text-[var(--color-nucleus-faint)]"
                >
                  {c}
                </span>
              ))}
            </div>
          )}
        </div>

        <div className="flex shrink-0 flex-col items-stretch gap-1">
          {agent.attach_cmd && (
            <button
              onClick={copyAttach}
              title={agent.attach_cmd}
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
          )}
          {agent.diary_key && (
            <Link
              to={`/diary?agent=${encodeURIComponent(agent.diary_key)}`}
              title={`see ${agent.diary_key} diary`}
              className="flex items-center justify-center gap-1 rounded border border-[var(--color-nucleus-border)] px-2 py-1 text-[11px] text-[var(--color-nucleus-faint)] hover:border-[var(--color-nucleus-accent)] hover:text-[var(--color-nucleus-accent)]"
            >
              <BookOpen size={11} strokeWidth={1.75} />
              diary
            </Link>
          )}
        </div>
      </div>

      {expanded && (
        <div className="mt-3 border-t border-[var(--color-nucleus-border)] pt-3">
          {detailErr && (
            <div className="text-[11px] text-[var(--color-status-down)]">{detailErr}</div>
          )}
          {hasRuns && (
            <RunsList runs={runs} />
          )}
          {hasLog && (
            <LogTail log={log} hadRuns={hasRuns} />
          )}
          {!hasRuns && !hasLog && (
            <div className="text-[11px] text-[var(--color-nucleus-faint)]">
              no run-log or launchd log for this agent
            </div>
          )}
        </div>
      )}
    </article>
  );
}

function RunsList({ runs }: { runs: RunRow[] | null }) {
  if (runs === null) return <div className="text-[11px] text-[var(--color-nucleus-faint)]">loading runs…</div>;
  if (runs.length === 0)
    return (
      <div className="flex items-center gap-1.5 text-[11px] text-[var(--color-nucleus-faint)]">
        <ScrollText size={11} strokeWidth={1.75} /> no indexed runs yet
      </div>
    );
  return (
    <div>
      <div className="mb-1 flex items-center gap-1.5 text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)] opacity-70">
        <ScrollText size={10} strokeWidth={2} /> recent runs
      </div>
      <ul className="space-y-1">
        {runs.slice(0, 8).map((r) => (
          <li key={r.run_id} className="flex items-center gap-2 text-[11px]">
            <span
              className={
                r.ended_at == null
                  ? "text-[var(--color-status-warn)]"
                  : r.ok
                    ? "text-[var(--color-status-ok)]"
                    : "text-[var(--color-status-down)]"
              }
              title={r.ended_at == null ? "in-flight / outcome unknown" : r.ok ? "closed cleanly" : "errored"}
            >
              {r.ended_at == null ? "▸" : r.ok ? "✓" : "✗"}
            </span>
            <span className="text-[var(--color-nucleus-faint)]" title={fullTime(r.started_at)}>
              {relTime(r.started_at)}
            </span>
            <span
              className="truncate font-mono text-[var(--color-nucleus-faint)] opacity-70"
              title={r.transcript_path}
            >
              {r.session_id.slice(0, 8)}
            </span>
          </li>
        ))}
      </ul>
    </div>
  );
}

function LogTail({ log, hadRuns }: { log: AgentLog | null; hadRuns: boolean }) {
  if (log === null) return <div className="text-[11px] text-[var(--color-nucleus-faint)]">loading log…</div>;
  return (
    <div className={hadRuns ? "mt-3" : ""}>
      <div className="mb-1 flex items-center gap-1.5 text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)] opacity-70">
        <FileText size={10} strokeWidth={2} /> {log.path}
      </div>
      <pre className="max-h-64 overflow-auto rounded bg-[var(--color-nucleus-bg)] p-2 text-[10px] leading-relaxed text-[var(--color-nucleus-faint)]">
        {log.tail || "(empty)"}
      </pre>
    </div>
  );
}

function relTime(iso: string): string {
  const then = Date.parse(iso);
  if (Number.isNaN(then)) return "—";
  const sec = Math.max(0, Math.floor((Date.now() - then) / 1000));
  if (sec < 60) return `${sec}s ago`;
  if (sec < 3600) return `${Math.floor(sec / 60)}m ago`;
  if (sec < 86400) return `${Math.floor(sec / 3600)}h ago`;
  return `${Math.floor(sec / 86400)}d ago`;
}

function fullTime(iso: string): string {
  const t = Date.parse(iso);
  return Number.isNaN(t) ? iso : new Date(t).toLocaleString("en-GB");
}

function statusLabel(a: AgentView): string {
  if (a.status === "errored" && a.last_exit != null) return `ERR ${a.last_exit}`;
  return a.status.toUpperCase();
}
