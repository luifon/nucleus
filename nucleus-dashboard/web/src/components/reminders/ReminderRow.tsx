import { useState } from "react";
import {
  Clock,
  Sparkles,
  Hash,
  Pause,
  Play,
  X,
  Pencil,
  Check,
  AlertCircle,
} from "lucide-react";
import StatusPill, { type StatusKind } from "@/components/StatusPill";
import {
  type ReminderView,
  pauseReminder,
  resumeReminder,
  cancelReminder,
  setReminderTitle,
} from "@/lib/api";

// One row per reminder. Title (or body, or derived) gets the
// prominent slot; cron, next-fire, channels in a secondary row.
// Per-row actions: pause/resume (terminal-aware), cancel (with
// confirm), inline edit-title (pencil → input → save).

export default function ReminderRow({
  reminder,
  onChange,
}: {
  reminder: ReminderView;
  /** Called with the updated reminder after any successful action so
   *  the parent list can replace the row in-place without refetching. */
  onChange: (updated: ReminderView) => void;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(reminder.title ?? "");
  const [pending, setPending] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const isSkill = !!reminder.system_prompt;
  const display = pickDisplayName(reminder);
  const statusKind = statusToKind(reminder.status);

  const run = async (label: string, fn: () => Promise<ReminderView>) => {
    setPending(label);
    setErr(null);
    try {
      const next = await fn();
      onChange(next);
      if (label === "title") setEditing(false);
    } catch (e) {
      setErr(String(e));
    } finally {
      setPending(null);
    }
  };

  const isTerminal = reminder.status === "fired" || reminder.status === "cancelled";

  return (
    <article className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-4 py-3">
      <div className="flex items-start gap-3">
        {isSkill && (
          <Sparkles
            size={14}
            strokeWidth={1.75}
            className="mt-1 shrink-0 text-[var(--color-nucleus-accent)]"
            aria-label="skill-fire"
          />
        )}
        <div className="min-w-0 flex-1">
          {editing ? (
            <form
              onSubmit={(e) => {
                e.preventDefault();
                void run("title", () => setReminderTitle(reminder.id, draft.trim() || null));
              }}
              className="flex items-center gap-1.5"
            >
              <input
                autoFocus
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Escape") setEditing(false);
                }}
                placeholder="title (empty clears)"
                className="flex-1 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-2 py-0.5 text-sm text-[var(--color-nucleus-text)] focus:border-[var(--color-nucleus-accent)] focus:outline-none"
              />
              <button
                type="submit"
                title="save"
                className="text-[var(--color-status-ok)] hover:text-[var(--color-nucleus-accent)]"
              >
                <Check size={14} strokeWidth={2} />
              </button>
              <button
                type="button"
                onClick={() => { setEditing(false); setDraft(reminder.title ?? ""); }}
                title="cancel"
                className="text-[var(--color-nucleus-faint)] hover:text-[var(--color-status-down)]"
              >
                <X size={14} strokeWidth={2} />
              </button>
            </form>
          ) : (
            <div className="flex items-center gap-2">
              <div
                className={`min-w-0 flex-1 truncate text-sm ${display.derived ? "italic text-[var(--color-nucleus-faint)]" : "text-[var(--color-nucleus-text)]"}`}
                title={display.tooltip}
              >
                {display.text}
              </div>
              {display.derived && (
                <span
                  className="flex items-center gap-1 text-[10px] text-[var(--color-status-warn)]"
                  title="No title set — click the pencil to add one"
                >
                  <AlertCircle size={10} strokeWidth={2} />
                  no title
                </span>
              )}
              <button
                onClick={() => { setDraft(reminder.title ?? ""); setEditing(true); }}
                title="edit title"
                className="shrink-0 text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
              >
                <Pencil size={12} strokeWidth={1.75} />
              </button>
              <StatusPill kind={statusKind}>{reminder.status.toUpperCase()}</StatusPill>
            </div>
          )}

          <div className="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[11px] text-[var(--color-nucleus-faint)]">
            <span className="flex items-center gap-1">
              <Hash size={9} strokeWidth={2} />
              {reminder.id}
            </span>
            {reminder.next_fire_at ? (
              <span className="flex items-center gap-1">
                <Clock size={9} strokeWidth={2} />
                next {fireTime(reminder.next_fire_at)}
              </span>
            ) : (
              <span>no next fire</span>
            )}
            {reminder.cron && <code className="text-[var(--color-nucleus-faint)]">{reminder.cron}</code>}
            {reminder.one_shot && <span className="text-[var(--color-status-warn)]">one-shot</span>}
            {reminder.created_by === "system" && <span>system-seeded</span>}
            {reminder.paused_until && (
              <span className="text-[var(--color-status-warn)]">
                paused until {fireTime(reminder.paused_until)}
              </span>
            )}
            {reminder.channels.map((c) => (
              <span
                key={c.channel}
                className="rounded border border-[var(--color-nucleus-border)] px-1.5 py-px text-[10px]"
                title={c.last_error ?? undefined}
              >
                {c.channel}
                {c.last_error && (
                  <AlertCircle
                    size={9}
                    strokeWidth={2}
                    className="ml-1 inline align-middle text-[var(--color-status-down)]"
                  />
                )}
              </span>
            ))}
          </div>

          {err && (
            <div className="mt-2 text-xs text-[var(--color-status-down)]">{err}</div>
          )}
        </div>

        {!isTerminal && !editing && (
          <div className="flex shrink-0 items-center gap-1">
            {reminder.status === "paused" ? (
              <ActionButton
                onClick={() => run("resume", () => resumeReminder(reminder.id))}
                disabled={pending !== null}
                title="resume"
                Icon={Play}
              />
            ) : (
              <ActionButton
                onClick={() => run("pause", () => pauseReminder(reminder.id))}
                disabled={pending !== null}
                title="pause"
                Icon={Pause}
              />
            )}
            <ActionButton
              onClick={() => {
                if (confirm(`Cancel reminder #${reminder.id}? Cancellation is sticky (system seeder won't re-create it).`)) {
                  void run("cancel", () => cancelReminder(reminder.id));
                }
              }}
              disabled={pending !== null}
              title="cancel"
              Icon={X}
              danger
            />
          </div>
        )}
      </div>
    </article>
  );
}

function ActionButton({
  onClick,
  title,
  Icon,
  disabled,
  danger,
}: {
  onClick: () => void;
  title: string;
  Icon: React.ComponentType<{ size?: number; strokeWidth?: number; className?: string }>;
  disabled?: boolean;
  danger?: boolean;
}) {
  return (
    <button
      onClick={onClick}
      disabled={disabled}
      title={title}
      className={[
        "rounded border border-[var(--color-nucleus-border)] px-1.5 py-1 text-[var(--color-nucleus-faint)] transition-colors disabled:opacity-40",
        danger
          ? "hover:border-[var(--color-status-down)] hover:text-[var(--color-status-down)]"
          : "hover:border-[var(--color-nucleus-accent)] hover:text-[var(--color-nucleus-accent)]",
      ].join(" ")}
    >
      <Icon size={12} strokeWidth={1.75} />
    </button>
  );
}

function statusToKind(status: string): StatusKind {
  switch (status) {
    case "active":
    case "pending":
      return "ok";
    case "paused":
      return "warn";
    case "fired":
      return "idle";
    case "cancelled":
      return "down";
    default:
      return "idle";
  }
}

// Same priority order as the cron rows: title > body > derived label
// from system_prompt.
function pickDisplayName(r: ReminderView): { text: string; derived: boolean; tooltip?: string } {
  if (r.title && r.title.trim()) {
    return { text: r.title, derived: false, tooltip: r.body || r.system_prompt || undefined };
  }
  if (r.body && r.body.trim()) {
    return { text: r.body, derived: false };
  }
  if (r.system_prompt) {
    const flat = r.system_prompt.trim().split("\n")[0];
    return {
      text: flat.length > 80 ? `${flat.slice(0, 77)}…` : flat,
      derived: true,
      tooltip: r.system_prompt,
    };
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
