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
import type { AcceptOutcome } from "./generated/AcceptOutcome";
import type { AcceptSuggestionReq } from "./generated/AcceptSuggestionReq";
import type { EditSuggestionReq } from "./generated/EditSuggestionReq";
import type { RejectSuggestionReq } from "./generated/RejectSuggestionReq";
import type { BaseState } from "./generated/BaseState";
import type { Evidence } from "./generated/Evidence";
import type { EvidenceSource } from "./generated/EvidenceSource";
import type { LiveSkill } from "./generated/LiveSkill";
import type { Suggestion } from "./generated/Suggestion";
import type { SuggestionAcceptResp } from "./generated/SuggestionAcceptResp";
import type { SuggestionKind } from "./generated/SuggestionKind";
import type { SuggestionOrigin } from "./generated/SuggestionOrigin";
import type { SuggestionReview } from "./generated/SuggestionReview";
import type { SuggestionStatus } from "./generated/SuggestionStatus";
import type { SuggestionSummary } from "./generated/SuggestionSummary";

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
  AcceptOutcome,
  BaseState,
  Evidence,
  EvidenceSource,
  LiveSkill,
  Suggestion,
  SuggestionAcceptResp,
  SuggestionKind,
  SuggestionOrigin,
  SuggestionReview,
  SuggestionStatus,
  SuggestionSummary,
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
 *  move or archive (409), empty otherwise; `issues` lists each problem of a
 *  refused suggestion draft (422), empty otherwise. */
export class SkillActionError extends ApiError {
  constructor(
    path: string,
    status: number,
    message: string,
    public readonly reminders: ReminderRef[],
    public readonly issues: string[] = [],
  ) {
    super(path, status, message);
    this.name = "SkillActionError";
  }
}

async function postSkillAction<B, R = SkillActionResp>(path: string, body: B): Promise<R> {
  const res = await fetch(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (res.ok) return res.json() as Promise<R>;
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
  const issues = Array.isArray(parsed?.issues) ? parsed.issues.filter((i) => typeof i === "string") : [];
  throw new SkillActionError(path, res.status, message, reminders, issues);
}

/** Move an active skill between `personal` and `global` (the other one). */
export const moveSkill = (req: MoveSkillReq) => postSkillAction("/skills/api/move", req);

/** Move an active skill into its tier's archive. */
export const archiveSkill = (req: ArchiveSkillReq) => postSkillAction("/skills/api/archive", req);

/** Move an archived skill back to its active tier, as `restore_name`. */
export const restoreSkill = (req: ArchivedSkillReq) => postSkillAction("/skills/api/restore", req);

/** Remove an archived skill. */
export const deleteSkill = (req: ArchivedSkillReq) => postSkillAction("/skills/api/delete", req);

// ─── suggestions (ADR-038) ─────────────────────────────────────────────────
// Every decision is made by `nucleus_core::skill_learner` on the server: the
// draft checks, the patch base-hash binding, the library lock. A refusal
// arrives as `SkillActionError` with core's text (and `issues` for a draft).

/** Every suggestion, newest first, every status. */
export const getSuggestions = (signal?: AbortSignal) =>
  jsonGet<SuggestionSummary[]>("/skills/api/suggestions", signal);

/** One suggestion with its draft, evidence, the live skill a patch
 *  replaces, and why accept would be refused now. */
export const getSuggestion = (id: number, signal?: AbortSignal) =>
  jsonGet<SuggestionReview>(`/skills/api/suggestions/detail${qs({ id })}`, signal);

/** Write the draft into the personal tree and mark it accepted.
 *  `draft_hash` is `SuggestionReview.draft_hash` of the draft on screen;
 *  the server refuses (409) when the stored draft changed since. */
export const acceptSuggestion = (id: number, draft_hash: string) =>
  postSkillAction<AcceptSuggestionReq, SuggestionAcceptResp>("/skills/api/suggestions/accept", { id, draft_hash });

/** Reject a pending suggestion; later reviews see the reason. */
export const rejectSuggestion = (id: number, reason: string | null) =>
  postSkillAction<RejectSuggestionReq, SuggestionReview>("/skills/api/suggestions/reject", {
    id,
    reason,
  });

/** Replace the draft of a pending suggestion (checked again by core). */
export const editSuggestion = (id: number, draft: string) =>
  postSkillAction<EditSuggestionReq, SuggestionReview>("/skills/api/suggestions/edit", { id, draft });
