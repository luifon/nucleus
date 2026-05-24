import { useMemo, useState } from "react";
import { RefreshCw, User, GitBranch, Sparkles } from "lucide-react";
import PageShell from "@/components/PageShell";
import Tabs from "@/components/Tabs";
import SkillRow from "@/components/skills/SkillRow";
import { useFetch } from "@/lib/hooks";
import { listSkills, type Skill, type SkillTier } from "@/lib/api";

// Two-tab catalog per operator request:
//   - personal: ~/.claude/skills/  (operator-only, never committed)
//   - repo:     .claude/skills/    (committed, ships with the repo
//                                   for other people who clone it)

export default function SkillsPage() {
  const skills = useFetch(listSkills);
  const [tier, setTier] = useState<SkillTier>("personal");

  const { personal, repo } = useMemo(() => splitByTier(skills.data ?? []), [skills.data]);
  const shown = tier === "personal" ? personal : repo;

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
          {
            value: "personal" as SkillTier,
            label: (
              <span className="flex items-center gap-1.5">
                <User size={12} strokeWidth={1.75} />
                personal
              </span>
            ),
            count: personal.length,
          },
          {
            value: "repo" as SkillTier,
            label: (
              <span className="flex items-center gap-1.5">
                <GitBranch size={12} strokeWidth={1.75} />
                repo
              </span>
            ),
            count: repo.length,
          },
        ]}
        value={tier}
        onChange={setTier}
      />

      <TierHint tier={tier} />

      {skills.error ? (
        <div className="rounded border border-[var(--color-status-down)] bg-[var(--color-nucleus-surface)] px-3 py-2 text-sm text-[var(--color-status-down)]">
          {skills.error}
        </div>
      ) : !skills.data ? (
        <div className="text-sm text-[var(--color-nucleus-faint)]">fetching…</div>
      ) : shown.length === 0 ? (
        <EmptyTier tier={tier} />
      ) : (
        <ul className="space-y-2">
          {shown.map((s) => (
            <li key={`${s.tier}-${s.name}`}>
              <SkillRow skill={s} />
            </li>
          ))}
        </ul>
      )}
    </PageShell>
  );
}

function TierHint({ tier }: { tier: SkillTier }) {
  return (
    <p className="mb-4 text-xs leading-relaxed text-[var(--color-nucleus-faint)]">
      {tier === "personal" ? (
        <>
          Operator-only skills at <code>~/.claude/skills/</code>. Not committed; encode
          your routines, contacts, third-party tools.
        </>
      ) : (
        <>
          Committed skills under <code>.claude/skills/</code>. Ship with the repo for
          anyone who clones it — keep them generic (no operator-identifying values).
        </>
      )}
    </p>
  );
}

function EmptyTier({ tier }: { tier: SkillTier }) {
  return (
    <div className="flex items-center gap-3 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-4 py-6 text-sm text-[var(--color-nucleus-faint)]">
      <Sparkles size={14} strokeWidth={1.75} className="text-[var(--color-nucleus-accent)]" />
      <div>
        No {tier} skills yet. Author one with{" "}
        <code className="rounded border border-[var(--color-nucleus-border)] px-1 py-px">
          /skill-creator create &lt;name&gt; at {tier === "personal" ? "~/.claude/skills/<name>" : ".claude/skills/<name>"}
        </code>{" "}
        (see Rule 11).
      </div>
    </div>
  );
}

function splitByTier(skills: Skill[]): { personal: Skill[]; repo: Skill[] } {
  const personal: Skill[] = [];
  const repo: Skill[] = [];
  for (const s of skills) {
    if (s.tier === "personal") personal.push(s);
    else repo.push(s);
  }
  return { personal, repo };
}
