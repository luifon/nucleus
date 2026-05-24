import { useState } from "react";
import {
  ChevronRight,
  ChevronDown,
  Sparkles,
  BookText,
  AlertOctagon,
  Bell,
  Tag,
  Cpu,
  FileText,
} from "lucide-react";
import StatusPill from "@/components/StatusPill";
import { type Skill, getSkillBody } from "@/lib/api";

// One row per skill. Collapsed shows name + description + key meta
// chips. Click expands to show the full SKILL.md body (rendered as
// preformatted text for v1 — markdown renderer can come later if
// the body becomes a primary read surface).

export default function SkillRow({ skill }: { skill: Skill }) {
  const [expanded, setExpanded] = useState(false);
  const [body, setBody] = useState<string | null>(null);
  const [bodyErr, setBodyErr] = useState<string | null>(null);

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

  return (
    <article className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)]">
      <button
        onClick={toggle}
        className="flex w-full items-start gap-3 px-4 py-3 text-left transition-colors hover:bg-[var(--color-nucleus-bg)]"
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
          <div className="flex items-baseline gap-2">
            <span className="text-base text-[var(--color-nucleus-text)]">{skill.name}</span>
            {skill.flavor && (
              <span className="text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)]">
                {skill.flavor}
              </span>
            )}
            {failures > 0 && <StatusPill kind="down">{failures} FAILS</StatusPill>}
          </div>
          {skill.description && (
            <p className="mt-1 line-clamp-2 text-sm leading-relaxed text-[var(--color-nucleus-faint)]">
              {skill.description}
            </p>
          )}
          <MetaRow skill={skill} />
        </div>
      </button>

      {expanded && (
        <div className="border-t border-[var(--color-nucleus-border)] px-4 py-3">
          <div className="mb-2 flex items-center gap-2 text-[11px] text-[var(--color-nucleus-faint)]">
            <FileText size={11} strokeWidth={1.75} />
            <code title={skill.path} className="truncate">{shortPath(skill.path)}</code>
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

function shortPath(p: string): string {
  // Show as `~/.claude/skills/<name>/SKILL.md` or `.claude/skills/<name>/SKILL.md`.
  const home = "/Users/";
  const idx = p.indexOf(home);
  if (idx !== -1) {
    const tail = p.slice(idx);
    return tail.replace(/^\/Users\/[^/]+/, "~");
  }
  return p;
}

function shortDate(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleDateString("en-GB", { day: "2-digit", month: "2-digit", year: "2-digit" });
}
