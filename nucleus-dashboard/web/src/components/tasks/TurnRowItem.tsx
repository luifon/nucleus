import { Clock, Timer } from "lucide-react";
import StatusPill from "@/components/StatusPill";
import { type TurnRow } from "@/lib/api";
import { shortTime, spanBetween, turnShowsError, turnStatusKind } from "@/lib/tasks";

// One row per WhatsApp conversational turn: pool, kind, status, start
// time, duration, ack, progress count, reply size, inbound count, and a
// preview of the first operator message. Failed and interrupted turns
// show their error under the row.

export default function TurnRowItem({ turn, now }: { turn: TurnRow; now: number }) {
  const duration = spanBetween(turn.started_at, turn.ended_at, now);
  return (
    <li className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-3 py-2 text-[12px]">
      <div className="flex items-center gap-2">
        <StatusPill kind={turnStatusKind(turn.status)}>{turn.status.toUpperCase()}</StatusPill>
        <span className="shrink-0 rounded border border-[var(--color-nucleus-border)] px-1.5 py-px text-[10px] text-[var(--color-nucleus-faint)]">
          {turn.pool}
        </span>
        <span className="shrink-0 text-[var(--color-nucleus-faint)]">{turn.kind}</span>
        <span
          className={`min-w-0 flex-1 truncate ${turn.first_text ? "text-[var(--color-nucleus-text)]" : "italic text-[var(--color-nucleus-faint)]"}`}
          title={turn.first_text ?? undefined}
        >
          {turn.first_text ?? "no operator text"}
        </span>
      </div>

      <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[11px] text-[var(--color-nucleus-faint)]">
        <span className="flex items-center gap-1" title={turn.started_at}>
          <Clock size={9} strokeWidth={2} />
          {shortTime(turn.started_at)}
        </span>
        <span className="flex items-center gap-1" title={turn.ended_at ?? "still running"}>
          <Timer size={9} strokeWidth={2} />
          {duration ?? "—"}
        </span>
        <span>
          ack{" "}
          <span className={turn.ack_sent ? "text-[var(--color-status-ok)]" : ""}>
            {turn.ack_sent ? "yes" : "no"}
          </span>
        </span>
        <span>progress {turn.progress_count}</span>
        <span>reply {turn.reply_chars === null ? "—" : `${turn.reply_chars} chars`}</span>
        <span>inbound {turn.inbound_count}</span>
      </div>

      {turnShowsError(turn) && (
        <div className="mt-1 whitespace-pre-wrap break-words text-[11px] text-[var(--color-status-down)]">
          {turn.error}
        </div>
      )}
    </li>
  );
}
