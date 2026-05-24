// Skills API — operator-personal + repo-committed skill catalogs.
// Mirrors `nucleus-dashboard/api/src/handlers/skills.rs`. Tiers per
// ADR-008 storage convention: `personal` lives at ~/.claude/skills/,
// `repo` lives at .claude/skills/ (committed).

import { jsonGet, qs } from "./client";

export type SkillTier = "personal" | "repo";

export type Skill = {
  /** Display name. Frontmatter `name` if present, otherwise the
   *  skill's directory name (Claude Code convention). */
  name: string;
  /** One-line summary from frontmatter. */
  description: string;
  tier: SkillTier;
  /** Absolute path to the SKILL.md. Useful for an "open in $EDITOR"
   *  hint (operator can copy/paste the path). */
  path: string;
  flavor: string | null;
  mcp_needed: string[] | null;
  last_used: string | null;
  last_failure: string | null;
  failure_count_30d: number | null;
  notify_on_failure: string[] | null;
  tags: string[] | null;
  trigger: string | null;
};

export const listSkills = () => jsonGet<Skill[]>("/skills/api/list");

/** Raw SKILL.md content (frontmatter + body markdown). The backend
 *  guards path traversal — only paths under the two known roots
 *  resolve. */
export const getSkillBody = (path: string) =>
  fetch(`/skills/api/body${qs({ path })}`).then(async (r) => {
    if (!r.ok) throw new Error(`/skills/api/body → ${r.status}`);
    return r.text();
  });
