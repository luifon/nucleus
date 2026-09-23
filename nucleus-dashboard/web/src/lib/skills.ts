// Pure helpers for the Skills page: tier labels, action availability,
// collision text, path shortening, and result/confirmation wording.
// The server stays the authority for every write; these rules only
// pre-disable actions whose refusal the UI can already predict from the
// listing (pinned, symlink, same-named copy in another active tier).

import type {
  ArchiveTier,
  MutableTier,
  ReminderRef,
  Skill,
  SkillActionResp,
  SkillTier,
} from "@/lib/api/skills";

/** Operator-facing tier name. `personal` shows as "private" (the
 *  `.nucleus` tree is operator-private). */
export function tierLabel(tier: SkillTier): string {
  switch (tier) {
    case "personal":
      return "private";
    case "repo":
      return "repo";
    case "global":
      return "global";
    case "personal-archive":
      return "private archive";
    case "global-archive":
      return "global archive";
  }
}

export function isArchiveTier(tier: SkillTier): tier is ArchiveTier {
  return tier === "personal-archive" || tier === "global-archive";
}

export function isMutableTier(tier: SkillTier): tier is MutableTier {
  return tier === "personal" || tier === "global";
}

export type ActionKind = "move" | "archive" | "restore" | "delete";

export type ActionSpec = {
  kind: ActionKind;
  label: string;
  /** Why the action is unavailable; null when it may be sent. */
  disabledReason: string | null;
  danger: boolean;
};

function tierList(tiers: SkillTier[]): string {
  return tiers.map(tierLabel).join(", ");
}

/** Actions a row offers, in display order. Repo rows and active symlinked
 *  rows get none. An archived symlink keeps `delete` (the server removes
 *  only the link) and shows `restore` disabled. */
export function skillActions(skill: Skill): ActionSpec[] {
  const symlink = skill.symlink_target !== null;
  if (isMutableTier(skill.tier)) {
    if (symlink) return [];
    const target: MutableTier = skill.tier === "personal" ? "global" : "personal";
    return [
      {
        kind: "move",
        label: `move to ${tierLabel(target)}`,
        disabledReason:
          skill.also_in.length > 0
            ? `a skill with this name is also in ${tierList(skill.also_in)}; moving would create a duplicate`
            : null,
        danger: false,
      },
      {
        kind: "archive",
        label: "archive",
        disabledReason: skill.pinned ? "pinned (pinned: true); unpin it before archiving" : null,
        danger: false,
      },
    ];
  }
  if (isArchiveTier(skill.tier)) {
    let restoreReason: string | null = null;
    if (symlink) restoreReason = "symlinked entry; restore is refused for symlinks";
    else if (skill.also_in.length > 0)
      restoreReason = `a skill with this name is active in ${tierList(skill.also_in)}; restoring would create a duplicate`;
    return [
      { kind: "restore", label: "restore", disabledReason: restoreReason, danger: false },
      { kind: "delete", label: "delete", disabledReason: null, danger: true },
    ];
  }
  return [];
}

/** Warning about same-named copies, or null. */
export function collisionWarning(skill: Skill): string | null {
  if (isArchiveTier(skill.tier)) {
    return skill.also_in.length > 0
      ? `restore blocked: a skill with this name is active in ${tierList(skill.also_in)}`
      : null;
  }
  if (skill.shadowed_by) return `not loaded: the ${tierLabel(skill.shadowed_by)} copy wins`;
  if (skill.also_in.length > 0) return `also in ${tierList(skill.also_in)}; this copy loads`;
  return null;
}

/** Path relative to the tier's documented location:
 *  `.nucleus/.claude/skills/…`, `.nucleus/.claude/skills/.archive/…`,
 *  `.claude/skills/…`, `~/.claude/skills/…`, `~/.claude/skills-archive/…`.
 *  Falls back to the input when the marker segment is missing. */
export function shortPath(path: string, tier: SkillTier): string {
  const cut = (marker: string, prefix: string) => {
    const i = path.lastIndexOf(marker);
    return i === -1 ? path : prefix + path.slice(i + marker.length);
  };
  switch (tier) {
    case "personal":
      return cut("/.nucleus/.claude/skills/", ".nucleus/.claude/skills/");
    case "personal-archive":
      return cut("/.nucleus/.claude/skills/.archive/", ".nucleus/.claude/skills/.archive/");
    case "repo":
      return cut("/.claude/skills/", ".claude/skills/");
    case "global":
      return cut("/.claude/skills/", "~/.claude/skills/");
    case "global-archive":
      return cut("/.claude/skills-archive/", "~/.claude/skills-archive/");
  }
}

/** Text shown in the inline delete confirmation. */
export function deleteConfirmText(skill: Skill): string {
  const link =
    skill.symlink_target !== null ? " Only the symlink is removed; its target is not touched." : "";
  if (skill.tier === "personal-archive") {
    return `Delete ${skill.dir_name} from the private archive? Recoverable from the .nucleus git history if it was committed there.${link}`;
  }
  return `Delete ${skill.dir_name} from the global archive? This is permanent: the global archive is not under version control.${link}`;
}

export type ResultSummary = {
  kind: "ok" | "warn";
  lines: string[];
};

/** Operator-facing summary of a successful write. A failed git commit
 *  turns the summary into a warning. */
export function describeResult(resp: SkillActionResp): ResultSummary {
  const from = tierLabel(resp.from);
  const to = resp.to ? tierLabel(resp.to) : null;
  const renamed = resp.new_dir_name && resp.new_dir_name !== resp.dir_name ? ` as ${resp.new_dir_name}` : "";
  let head: string;
  switch (resp.action) {
    case "move":
      head = `moved ${resp.dir_name} from ${from} to ${to}${renamed}`;
      break;
    case "archive":
      head = `archived ${resp.dir_name} from ${from} to ${to}${renamed}`;
      break;
    case "restore":
      head = `restored ${resp.dir_name} from ${from} to ${to}${renamed}`;
      break;
    case "delete":
      head = `deleted ${resp.dir_name} from ${from}`;
      break;
  }
  const lines = [head];
  let kind: ResultSummary["kind"] = "ok";
  switch (resp.git.status) {
    case "committed":
      lines.push(`git: committed ${resp.git.sha.slice(0, 7)} in .nucleus`);
      break;
    case "skipped":
      lines.push(`git: no commit (${resp.git.reason})`);
      break;
    case "failed":
      kind = "warn";
      lines.push(
        `git commit failed: ${resp.git.error}. The change is on disk and uncommitted in .nucleus.`,
      );
      break;
    case "not-applicable":
      break;
  }
  if (resp.note) lines.push(resp.note);
  return { kind, lines };
}

/** One line per blocking reminder: `#12 daily digest (system_prompt)`. */
export function reminderLine(r: ReminderRef): string {
  return `#${r.id} ${r.title ?? "(no title)"} (${r.field})`;
}
