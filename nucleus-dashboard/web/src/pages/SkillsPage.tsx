import { useState, type ReactNode } from "react";
import { RefreshCw, User, GitBranch, Globe, Archive, Sparkles, X } from "lucide-react";
import PageShell from "@/components/PageShell";
import Tabs from "@/components/Tabs";
import SkillRow from "@/components/skills/SkillRow";
import { useFetch } from "@/lib/hooks";
import {
  archiveSkill,
  deleteSkill,
  getSkillLibrary,
  moveSkill,
  restoreSkill,
  SkillActionError,
  type ReminderRef,
  type Skill,
  type SkillActionResp,
  type SkillLibrary,
} from "@/lib/api";
import {
  type ActionKind,
  describeResult,
  isArchiveTier,
  isMutableTier,
  reminderLine,
  tierLabel,
} from "@/lib/skills";

type TabValue = keyof SkillLibrary;

/** Outcome of the last write, shown above the list until dismissed or
 *  replaced by the next write. */
type Outcome =
  | { kind: "ok" | "warn"; lines: string[] }
  | { kind: "error"; lines: string[]; reminders: ReminderRef[] };

export default function SkillsPage() {
  const skills = useFetch(getSkillLibrary);
  const [tab, setTab] = useState<TabValue>("personal");
  const [busy, setBusy] = useState(false);
  const [outcome, setOutcome] = useState<Outcome | null>(null);

  const shown = skills.data?.[tab] ?? [];

  const onAction = async (kind: ActionKind, skill: Skill) => {
    setBusy(true);
    setOutcome(null);
    try {
      const resp = await send(kind, skill);
      setOutcome(describeResult(resp));
      skills.refetch();
    } catch (e) {
      if (e instanceof SkillActionError) {
        setOutcome({ kind: "error", lines: [e.message], reminders: e.reminders });
      } else {
        setOutcome({ kind: "error", lines: [String(e)], reminders: [] });
      }
      // The listing may be stale (the reason for a refusal); reload it.
      skills.refetch();
    } finally {
      setBusy(false);
    }
  };

  const tabIcon = (Icon: typeof User, text: string): ReactNode => (
    <span className="flex items-center gap-1.5">
      <Icon size={12} strokeWidth={1.75} />
      {text}
    </span>
  );

  return (
    <PageShell
      title={
        <>
          skills <span className="text-[var(--color-nucleus-faint)]">/ procedural memory</span>
        </>
      }
      actions={
        <button
          onClick={skills.refetch}
          className="flex items-center gap-1.5 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-2.5 py-1 text-xs text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
        >
          <RefreshCw size={12} strokeWidth={1.75} />
          refresh
        </button>
      }
    >
      <Tabs
        tabs={[
          { value: "personal" as TabValue, label: tabIcon(User, "private"), count: skills.data?.personal.length ?? null },
          { value: "repo" as TabValue, label: tabIcon(GitBranch, "repo"), count: skills.data?.repo.length ?? null },
          { value: "global" as TabValue, label: tabIcon(Globe, "global"), count: skills.data?.global.length ?? null },
          { value: "archived" as TabValue, label: tabIcon(Archive, "archived"), count: skills.data?.archived.length ?? null },
        ]}
        value={tab}
        onChange={setTab}
      />

      <TierHint tab={tab} />

      {busy && <div className="mb-3 text-xs text-[var(--color-nucleus-faint)]">working…</div>}
      {outcome && <OutcomePanel outcome={outcome} onDismiss={() => setOutcome(null)} />}

      {skills.error ? (
        <div className="rounded border border-[var(--color-status-down)] bg-[var(--color-nucleus-surface)] px-3 py-2 text-sm text-[var(--color-status-down)]">
          {skills.error}
        </div>
      ) : !skills.data ? (
        <div className="text-sm text-[var(--color-nucleus-faint)]">fetching…</div>
      ) : shown.length === 0 ? (
        <EmptyTier tab={tab} />
      ) : (
        <ul className="space-y-2">
          {shown.map((s) => (
            <li key={`${s.tier}/${s.dir_name}`}>
              <SkillRow skill={s} busy={busy} onAction={onAction} />
            </li>
          ))}
        </ul>
      )}
    </PageShell>
  );
}

/** Dispatch one row action to its endpoint. Tier narrowing mirrors
 *  `skillActions`: move/archive for active mutable tiers, restore/delete
 *  for archive tiers. */
function send(kind: ActionKind, skill: Skill): Promise<SkillActionResp> {
  const dir_name = skill.dir_name;
  if ((kind === "move" || kind === "archive") && isMutableTier(skill.tier)) {
    return kind === "move"
      ? moveSkill({ dir_name, from: skill.tier })
      : archiveSkill({ dir_name, tier: skill.tier });
  }
  if ((kind === "restore" || kind === "delete") && isArchiveTier(skill.tier)) {
    return kind === "restore"
      ? restoreSkill({ dir_name, tier: skill.tier })
      : deleteSkill({ dir_name, tier: skill.tier });
  }
  return Promise.reject(new Error(`${kind} is not available for ${tierLabel(skill.tier)} skills`));
}

const OUTCOME_TONE: Record<Outcome["kind"], { border: string; text: string }> = {
  ok: { border: "border-[var(--color-status-ok)]", text: "text-[var(--color-status-ok)]" },
  warn: { border: "border-[var(--color-status-warn)]", text: "text-[var(--color-status-warn)]" },
  error: { border: "border-[var(--color-status-down)]", text: "text-[var(--color-status-down)]" },
};

function OutcomePanel({ outcome, onDismiss }: { outcome: Outcome; onDismiss: () => void }) {
  const tone = OUTCOME_TONE[outcome.kind];
  const [head, ...rest] = outcome.lines;
  return (
    <div
      className={`mb-4 flex items-start gap-3 rounded border bg-[var(--color-nucleus-surface)] px-3 py-2 text-xs ${tone.border}`}
    >
      <div className="min-w-0 flex-1 space-y-1">
        <div className={tone.text}>{head}</div>
        {rest.map((l, i) => (
          <div key={i} className="text-[var(--color-nucleus-faint)]">
            {l}
          </div>
        ))}
        {outcome.kind === "error" && outcome.reminders.length > 0 && (
          <div className="pt-1">
            <div className="text-[var(--color-nucleus-faint)]">
              These reminders refer to the skill. Change or cancel them first:
            </div>
            <ul className="mt-1 space-y-0.5 text-[var(--color-nucleus-text)]">
              {outcome.reminders.map((r) => (
                <li key={`${r.id}-${r.field}`}>
                  <code>{reminderLine(r)}</code>
                </li>
              ))}
            </ul>
          </div>
        )}
      </div>
      <button
        onClick={onDismiss}
        title="dismiss"
        className="shrink-0 text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
      >
        <X size={12} strokeWidth={2} />
      </button>
    </div>
  );
}

const PRECEDENCE =
  "When two active tiers hold a skill with the same name, Claude Code loads one copy: global first, then repo, then private.";

function TierHint({ tab }: { tab: TabValue }) {
  let text: ReactNode;
  switch (tab) {
    case "personal":
      text = (
        <>
          Operator-private skills at <code>.nucleus/.claude/skills/</code>. Gitignored in this
          repository; <code>.nucleus</code> is a separate local git repository, and dashboard
          changes here are committed there. Loaded only into Nucleus sessions, which add the
          directory with <code>--add-dir</code>. {PRECEDENCE}
        </>
      );
      break;
    case "repo":
      text = (
        <>
          Committed skills at <code>.claude/skills/</code>. Loaded in every session started in
          this repository and published with it, so they must not contain operator-identifying
          values. Read-only here: change them with git. {PRECEDENCE}
        </>
      );
      break;
    case "global":
      text = (
        <>
          Skills at <code>~/.claude/skills/</code>. Loaded in every Claude Code session on this
          machine, in any project. Not under version control. Symlinked entries are installed by
          another tool and are read-only here. {PRECEDENCE}
        </>
      );
      break;
    case "archived":
      text = (
        <>
          Archived skills are not loaded. The private archive is{" "}
          <code>.nucleus/.claude/skills/.archive/</code> and is kept in the <code>.nucleus</code>{" "}
          git history. The global archive is <code>~/.claude/skills-archive/</code> and is not
          under version control, so deleting from it is permanent.
        </>
      );
      break;
  }
  return <p className="mb-4 text-xs leading-relaxed text-[var(--color-nucleus-faint)]">{text}</p>;
}

function EmptyTier({ tab }: { tab: TabValue }) {
  const createAt: Partial<Record<TabValue, string>> = {
    personal: ".nucleus/.claude/skills/<name>",
    repo: ".claude/skills/<name>",
    global: "~/.claude/skills/<name>",
  };
  const at = createAt[tab];
  return (
    <div className="flex items-center gap-3 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-4 py-6 text-sm text-[var(--color-nucleus-faint)]">
      <Sparkles size={14} strokeWidth={1.75} className="text-[var(--color-nucleus-accent)]" />
      {at ? (
        <div>
          No {tab === "personal" ? "private" : tab} skills yet. Author one with{" "}
          <code className="rounded border border-[var(--color-nucleus-border)] px-1 py-px">
            /skill-creator create &lt;name&gt; at {at}
          </code>{" "}
          (see Rule 11).
        </div>
      ) : (
        <div>No archived skills.</div>
      )}
    </div>
  );
}
