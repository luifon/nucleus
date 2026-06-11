// Skills API — operator-personal + repo-committed skill catalogs.
// Mirrors `nucleus-dashboard/api/src/handlers/skills.rs`. Tiers per
// ADR-008 storage convention: `personal` lives at ~/.claude/skills/,
// `repo` lives at .claude/skills/ (committed).
// Wire types are ts-rs-generated from the Rust structs (./generated/).

import { jsonGet, qs } from "./client";
import type { Skill as SkillWire } from "./generated/Skill";

/** UI-layer refinement: the wire shape (generated Skill) carries
 *  `tier: string`; this union narrows it to the two storage trees the
 *  scanner actually emits. */
export type SkillTier = "personal" | "repo";

/** Wire shape is generated; `tier` narrowing is a UI-layer refinement. */
export type Skill = Omit<SkillWire, "tier"> & {
  tier: SkillTier;
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
