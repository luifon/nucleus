// Skills API — mirrors `nucleus-dashboard/api/src/handlers/skills.rs`.
// Tiers (`SkillTier`): `personal` (.nucleus/.claude/skills, gitignored),
// `repo` (.claude/skills, committed), `global` (~/.claude/skills), and the
// `personal-archive` / `global-archive` trees.
// Wire types are ts-rs-generated from the Rust structs (./generated/).

import { ApiError, jsonGet, qs } from "./client";
import type { ArchiveSkillReq } from "./generated/ArchiveSkillReq";
import type { ArchivedSkillReq } from "./generated/ArchivedSkillReq";
import type { ArchiveTier } from "./generated/ArchiveTier";
import type { GitOutcome } from "./generated/GitOutcome";
import type { MoveSkillReq } from "./generated/MoveSkillReq";
import type { MutableTier } from "./generated/MutableTier";
import type { ReminderRef } from "./generated/ReminderRef";
import type { Skill } from "./generated/Skill";
import type { SkillAction } from "./generated/SkillAction";
import type { SkillActionResp } from "./generated/SkillActionResp";
import type { SkillLibrary } from "./generated/SkillLibrary";
import type { SkillsErrorBody } from "./generated/SkillsErrorBody";
import type { SkillTier } from "./generated/SkillTier";

export type {
  ArchiveTier,
  GitOutcome,
  MutableTier,
  ReminderRef,
  Skill,
  SkillAction,
  SkillActionResp,
  SkillLibrary,
  SkillsErrorBody,
  SkillTier,
};

/** The whole library, grouped by tier (personal, repo, global, archived). */
export const getSkillLibrary = (signal?: AbortSignal) =>
  jsonGet<SkillLibrary>("/skills/api/list", signal);

/** Raw SKILL.md content (frontmatter + body markdown). Pass `Skill.path`
 *  unchanged; the backend only resolves SKILL.md files inside the tier
 *  roots. */
export const getSkillBody = (path: string) =>
  fetch(`/skills/api/body${qs({ path })}`).then(async (r) => {
    if (!r.ok) throw new Error(`/skills/api/body → ${r.status}`);
    return r.text();
  });

/** Thrown by the write endpoints on a non-2xx response. `message` is the
 *  server's `error`; `reminders` lists the live reminders that block a
 *  move or archive (409), empty otherwise. */
export class SkillActionError extends ApiError {
  constructor(
    path: string,
    status: number,
    message: string,
    public readonly reminders: ReminderRef[],
  ) {
    super(path, status, message);
    this.name = "SkillActionError";
  }
}

async function postSkillAction<B>(path: string, body: B): Promise<SkillActionResp> {
  const res = await fetch(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (res.ok) return res.json() as Promise<SkillActionResp>;
  let parsed: Partial<SkillsErrorBody> | null = null;
  try {
    parsed = (await res.json()) as Partial<SkillsErrorBody>;
  } catch {
    /* not JSON — fall through to the status line */
  }
  const message =
    parsed && typeof parsed.error === "string"
      ? parsed.error
      : `${path} → ${res.status} ${res.statusText}`;
  const reminders = Array.isArray(parsed?.reminders) ? parsed.reminders : [];
  throw new SkillActionError(path, res.status, message, reminders);
}

/** Move an active skill between `personal` and `global` (the other one). */
export const moveSkill = (req: MoveSkillReq) => postSkillAction("/skills/api/move", req);

/** Move an active skill into its tier's archive. */
export const archiveSkill = (req: ArchiveSkillReq) => postSkillAction("/skills/api/archive", req);

/** Move an archived skill back to its active tier, as `restore_name`. */
export const restoreSkill = (req: ArchivedSkillReq) => postSkillAction("/skills/api/restore", req);

/** Remove an archived skill. */
export const deleteSkill = (req: ArchivedSkillReq) => postSkillAction("/skills/api/delete", req);
