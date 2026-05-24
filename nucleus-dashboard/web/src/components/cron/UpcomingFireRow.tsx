import { Clock, Sparkles, Hash, AlertCircle } from "lucide-react";
import { type UpcomingFire } from "@/lib/api";

// Single row in the upcoming-fires list. Title (or body, or derived
// label) gets the prominent slot; cron / channels / id sit in a faint
// metadata row below. Skill-fires without a title get a subtle "needs
// title" hint so the operator notices and runs `reminders set-title`.

export default function UpcomingFireRow({ fire }: { fire: UpcomingFire }) {
  const isSkill = !!fire.system_prompt;
  const channels = fire.channels?.split(" | ").filter(Boolean) ?? [];
  const display = pickDisplayName(fire);

  return (
    <li className="flex items-start gap-3 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-3 py-2.5">
      <div className="shrink-0 text-right tabular-nums">
        <div className="flex items-center gap-1.5 text-sm text-[var(--color-nucleus-text)]">
          <Clock size={12} strokeWidth={1.75} className="text-[var(--color-nucleus-accent)]" />
          {fireTime(fire.next_fire_at)}
        </div>
        <div className="mt-0.5 text-[11px] text-[var(--color-nucleus-faint)]">
          {relTime(fire.next_fire_at)}
        </div>
      </div>

      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          {isSkill && (
            <Sparkles
              size={12}
              strokeWidth={1.75}
              className="shrink-0 text-[var(--color-nucleus-accent)]"
              aria-label="skill-fire"
            />
          )}
          <div
            className={`truncate text-sm ${display.derived ? "italic text-[var(--color-nucleus-faint)]" : "text-[var(--color-nucleus-text)]"}`}
            title={display.tooltip}
          >
            {display.text}
          </div>
          {display.derived && (
            <span
              className="flex items-center gap-1 text-[10px] text-[var(--color-status-warn)]"
              title="No title set. Run: reminders set-title <id> <title>"
            >
              <AlertCircle size={10} strokeWidth={2} />
              no title
            </span>
          )}
        </div>
        <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[11px] text-[var(--color-nucleus-faint)]">
          <span className="flex items-center gap-1">
            <Hash size={9} strokeWidth={2} />
            {fire.id}
          </span>
          {fire.cron && <code className="text-[var(--color-nucleus-faint)]">{fire.cron}</code>}
          {fire.one_shot === 1 && <span className="text-[var(--color-status-warn)]">one-shot</span>}
          {fire.created_by === "system" && <span>system-seeded</span>}
          {channels.map((c) => (
            <span
              key={c}
              className="rounded border border-[var(--color-nucleus-border)] px-1.5 py-px text-[10px]"
            >
              {c}
            </span>
          ))}
        </div>
      </div>
    </li>
  );
}

// Resolve what to render as the row's headline. Three tiers:
//   1. operator-set title           (preferred — explicit name)
//   2. body                         (text reminders — body IS the name)
//   3. derived from system_prompt   (skill-fires without a title;
//                                    flagged with `derived: true` so
//                                    the caller can render the "no
//                                    title" warning)
function pickDisplayName(fire: UpcomingFire): { text: string; derived: boolean; tooltip?: string } {
  if (fire.title && fire.title.trim()) {
    return { text: fire.title, derived: false, tooltip: fire.body ?? fire.system_prompt ?? undefined };
  }
  if (fire.body && fire.body.trim()) {
    return { text: fire.body, derived: false };
  }
  if (fire.system_prompt) {
    const flat = fire.system_prompt.trim().split("\n")[0];
    const truncated = flat.length > 80 ? `${flat.slice(0, 77)}…` : flat;
    return { text: truncated, derived: true, tooltip: fire.system_prompt };
  }
  return { text: "—", derived: false };
}

function fireTime(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const now = new Date();
  const sameDay =
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate();
  const time = d.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" });
  if (sameDay) return `today ${time}`;
  const day = d.toLocaleDateString("en-GB", { day: "2-digit", month: "2-digit" });
  return `${day} ${time}`;
}

function relTime(iso: string): string {
  const then = new Date(iso).getTime();
  if (!Number.isFinite(then)) return "";
  const sec = Math.floor((then - Date.now()) / 1000);
  if (sec < 60) return `in <1m`;
  if (sec < 3600) return `in ${Math.floor(sec / 60)}m`;
  if (sec < 86400) return `in ${Math.floor(sec / 3600)}h`;
  return `in ${Math.floor(sec / 86400)}d`;
}
