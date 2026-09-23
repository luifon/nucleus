import { useState } from "react";
import {
  ChevronRight,
  ChevronDown,
  Sparkles,
  BookText,
  AlertOctagon,
  AlertTriangle,
  Archive,
  ArrowLeftRight,
  Bell,
  Link2,
  Pin,
  RotateCcw,
  Tag,
  Cpu,
  FileText,
  Trash2,
} from "lucide-react";
import StatusPill from "@/components/StatusPill";
import { type Skill, getSkillBody } from "@/lib/api";
import {
  type ActionKind,
  type ActionSpec,
  collisionWarning,
  deleteConfirmText,
  isArchiveTier,
  shortPath,
  skillActions,
  tierLabel,
} from "@/lib/skills";

// One row per skill. Collapsed shows name + description + key meta
// chips, plus the tier actions on the right. Click expands to show the
// full SKILL.md body (preformatted text). Delete asks for an inline
// confirmation inside the row.

const ACTION_ICON: Record<ActionKind, typeof Archive> = {
  move: ArrowLeftRight,
  archive: Archive,
  restore: RotateCcw,
  delete: Trash2,
};

export default function SkillRow({
  skill,
  busy,
  onAction,
}: {
  skill: Skill;
  /** True while any skill write is in flight; disables every action. */
  busy: boolean;
  onAction: (kind: ActionKind, skill: Skill) => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const [body, setBody] = useState<string | null>(null);
  const [bodyErr, setBodyErr] = useState<string | null>(null);
  const [confirmingDelete, setConfirmingDelete] = useState(false);

  const toggle = async () => {
    const next = !expanded;
    setExpanded(next);
    if (next && body === null && !bodyErr) {
      try {
        setBody(await getSkillBody(skill.path));
      } catch (e) {
        setBodyErr(String(e));
      }
    }
  };

  const FlavorIcon = skill.flavor === "learned" ? BookText : Sparkles;
  const failures = skill.failure_count_30d ?? 0;
  const actions = skillActions(skill);
  const collision = collisionWarning(skill);
  const archived = isArchiveTier(skill.tier);

  const trigger = (a: ActionSpec) => {
    if (a.kind === "delete") setConfirmingDelete(true);
    else onAction(a.kind, skill);
  };

  return (
    <article className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)]">
      <div className="flex items-start">
        <button
          onClick={toggle}
          className="flex min-w-0 flex-1 items-start gap-3 px-4 py-3 text-left transition-colors hover:bg-[var(--color-nucleus-bg)]"
        >
          {expanded ? (
            <ChevronDown size={14} strokeWidth={1.75} className="mt-1 shrink-0 text-[var(--color-nucleus-faint)]" />
          ) : (
            <ChevronRight size={14} strokeWidth={1.75} className="mt-1 shrink-0 text-[var(--color-nucleus-faint)]" />
          )}
          <FlavorIcon
            size={14}
            strokeWidth={1.75}
            className="mt-1 shrink-0 text-[var(--color-nucleus-accent)]"
            aria-label={skill.flavor ?? "recipe"}
          />
          <div className="min-w-0 flex-1">
            <div className="flex flex-wrap items-baseline gap-x-2 gap-y-0.5">
              <span className="text-base text-[var(--color-nucleus-text)]">{skill.name}</span>
              {skill.flavor && (
                <span className="text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)]">
                  {skill.flavor}
                </span>
              )}
              {skill.pinned && (
                <span className="flex items-center gap-1 text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)]">
                  <Pin size={9} strokeWidth={2} />
                  pinned
                </span>
              )}
              {failures > 0 && <StatusPill kind="down">{failures} FAILS</StatusPill>}
            </div>
            {skill.description && (
              <p className="mt-1 line-clamp-2 text-sm leading-relaxed text-[var(--color-nucleus-faint)]">
                {skill.description}
              </p>
            )}
            <LocationRow skill={skill} archived={archived} />
            {collision && (
              <div className="mt-1.5 flex items-center gap-1 text-[11px] text-[var(--color-status-warn)]">
                <AlertTriangle size={10} strokeWidth={2} />
                {collision}
              </div>
            )}
            <MetaRow skill={skill} />
          </div>
        </button>

        {actions.length > 0 && (
          <div className="flex shrink-0 flex-col items-end gap-1 px-3 py-3">
            {actions.map((a) => (
              <ActionButton
                key={a.kind}
                spec={a}
                disabled={busy || confirmingDelete || a.disabledReason !== null}
                onClick={() => trigger(a)}
              />
            ))}
          </div>
        )}
      </div>

      {confirmingDelete && (
        <div className="flex flex-wrap items-center gap-2 border-t border-[var(--color-status-down)] px-4 py-2 text-xs">
          <span className="min-w-0 flex-1 text-[var(--color-status-down)]">{deleteConfirmText(skill)}</span>
          <button
            onClick={() => {
              setConfirmingDelete(false);
              onAction("delete", skill);
            }}
            disabled={busy}
            className="rounded border border-[var(--color-status-down)] px-2 py-0.5 text-[var(--color-status-down)] transition-colors hover:bg-[var(--color-nucleus-bg)] disabled:opacity-40"
          >
            {skill.tier === "global-archive" ? "delete permanently" : "delete"}
          </button>
          <button
            onClick={() => setConfirmingDelete(false)}
            className="rounded border border-[var(--color-nucleus-border)] px-2 py-0.5 text-[var(--color-nucleus-faint)] transition-colors hover:text-[var(--color-nucleus-text)]"
          >
            keep
          </button>
        </div>
      )}

      {expanded && (
        <div className="border-t border-[var(--color-nucleus-border)] px-4 py-3">
          <div className="mb-2 flex items-center gap-2 text-[11px] text-[var(--color-nucleus-faint)]">
            <FileText size={11} strokeWidth={1.75} />
            <code title={skill.path} className="truncate">{shortPath(skill.path, skill.tier)}</code>
          </div>
          {bodyErr ? (
            <div className="text-xs text-[var(--color-status-down)]">{bodyErr}</div>
          ) : body === null ? (
            <div className="text-xs text-[var(--color-nucleus-faint)]">loading…</div>
          ) : (
            <pre className="overflow-x-auto whitespace-pre-wrap text-[12px] leading-relaxed text-[var(--color-nucleus-text)]">
              {body}
            </pre>
          )}
        </div>
      )}
    </article>
  );
}

function ActionButton({
  spec,
  disabled,
  onClick,
}: {
  spec: ActionSpec;
  disabled: boolean;
  onClick: () => void;
}) {
  const Icon = ACTION_ICON[spec.kind];
  // The title sits on a wrapper: some browsers show no tooltip for a
  // disabled button, and the reason is what the operator needs to see.
  return (
    <span title={spec.disabledReason ?? undefined} className={spec.disabledReason ? "cursor-not-allowed" : undefined}>
      <button
        onClick={onClick}
        disabled={disabled}
        className={[
          "flex items-center gap-1.5 rounded border border-[var(--color-nucleus-border)] px-2 py-0.5 text-[11px] text-[var(--color-nucleus-faint)] transition-colors disabled:pointer-events-none disabled:opacity-40",
          spec.danger
            ? "enabled:hover:border-[var(--color-status-down)] enabled:hover:text-[var(--color-status-down)]"
            : "enabled:hover:border-[var(--color-nucleus-accent)] enabled:hover:text-[var(--color-nucleus-accent)]",
        ].join(" ")}
      >
        <Icon size={11} strokeWidth={1.75} />
        {spec.label}
      </button>
    </span>
  );
}

/** Tier-specific location facts: archive origin, restore name, symlink. */
function LocationRow({ skill, archived }: { skill: Skill; archived: boolean }) {
  const restoreDiffers = archived && skill.restore_name !== null && skill.restore_name !== skill.dir_name;
  if (!archived && skill.symlink_target === null) return null;
  return (
    <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[11px] text-[var(--color-nucleus-faint)]">
      {archived && (
        <span
          className="rounded border border-[var(--color-nucleus-border)] px-1.5 py-px text-[10px]"
          title={skill.tier}
        >
          {tierLabel(skill.tier)}
        </span>
      )}
      {archived && <code>{skill.dir_name}</code>}
      {restoreDiffers && <span>restores as <code>{skill.restore_name}</code></span>}
      {skill.symlink_target !== null && (
        <span
          className="flex items-center gap-1 rounded border border-[var(--color-nucleus-border)] px-1.5 py-px text-[10px]"
          title="symlinked skill directory, managed outside the dashboard; read-only here"
        >
          <Link2 size={9} strokeWidth={2} />
          read-only symlink → <code>{skill.symlink_target}</code>
        </span>
      )}
    </div>
  );
}

function MetaRow({ skill }: { skill: Skill }) {
  const items: { Icon: typeof Tag; text: string; kind?: "ok" | "warn" | "down" }[] = [];
  if (skill.trigger) items.push({ Icon: Bell, text: skill.trigger });
  if (skill.mcp_needed && skill.mcp_needed.length > 0) {
    items.push({ Icon: Cpu, text: `mcp: ${skill.mcp_needed.join(", ")}` });
  }
  if (skill.tags && skill.tags.length > 0) {
    items.push({ Icon: Tag, text: skill.tags.join(" · ") });
  }
  items.push({
    Icon: skill.last_used ? Bell : AlertOctagon,
    text: skill.last_used ? `last used ${shortDate(skill.last_used)}` : "never fired",
    kind: skill.last_used ? "ok" : undefined,
  });
  if (skill.notify_on_failure && skill.notify_on_failure.length > 0) {
    items.push({
      Icon: Bell,
      text: `notify: ${skill.notify_on_failure.join(", ")}`,
    });
  }
  return (
    <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[11px] text-[var(--color-nucleus-faint)]">
      {items.map((it, i) => (
        <span key={i} className="flex items-center gap-1">
          <it.Icon size={9} strokeWidth={2} />
          {it.text}
        </span>
      ))}
    </div>
  );
}

function shortDate(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleDateString("en-GB", { day: "2-digit", month: "2-digit", year: "2-digit" });
}
