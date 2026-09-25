// Pure helpers for the Intake page (ADR-036): stage colours, and which
// operator actions an item offers. The server stays the authority for every
// action; these rules only hide an action the listing already shows would
// be refused.

import type { StatusKind } from "@/components/StatusPill";
import type { IntakeItem, IntakeMessage, IntakeStage } from "@/lib/api/intake";

/** The stages in pipeline order, for the stage track. */
export const STAGE_TRACK: readonly IntakeStage[] = ["queued", "eval", "refinement", "implementation", "pr", "review", "closed"];

/** Not finished: the pipeline still works on it or waits for the
 *  operator. `stale` is finished (only a new label starts new work). */
export function isOpenItem(stage: IntakeStage): boolean {
  return stage !== "closed" && stage !== "cancelled" && stage !== "stale";
}

/** Stages where an agent or Nucleus is working and the list should refresh. */
export function isWorking(item: Pick<IntakeItem, "stage" | "current_task_id">): boolean {
  if (item.stage === "queued" || item.stage === "eval" || item.stage === "implementation" || item.stage === "pr") {
    return true;
  }
  return item.stage === "refinement" && item.current_task_id !== null;
}

/** Amber while work runs, red for a failure, green when closed with a PR,
 *  accent while the operator is expected to act, faint otherwise. */
export function stageKind(item: Pick<IntakeItem, "stage" | "pr_url" | "current_task_id">): StatusKind {
  switch (item.stage) {
    case "failed":
    case "blocked":
    case "stale":
      return "down";
    case "closed":
      return item.pr_url ? "ok" : "idle";
    case "cancelled":
      return "idle";
    case "review":
      return "warn";
    case "refinement":
      return item.current_task_id ? "warn" : "idle";
    default:
      return "warn";
  }
}

/** What the operator is expected to do next, or null. */
export function waitingOn(item: IntakeItem): string | null {
  if (item.stage === "refinement" && !item.current_task_id) {
    return item.plan_version > 0 ? `plan v${item.plan_version} waits for approval or a reply` : "the agent asked for a reply";
  }
  if (item.stage === "review" && item.comment_state === "proposed") return "the issue comment waits for approval";
  if (item.stage === "failed") return "failed — retry or cancel";
  if (item.stage === "blocked") return "blocked by the secret guard — fix, then retry or cancel";
  if (item.stage === "stale") return "stale — the issue changed; add the label again for a new item";
  return null;
}

/** A plan can be approved during refinement, when one exists and no
 *  refinement turn is running (its reply may replace the plan). */
export function canApprovePlan(item: Pick<IntakeItem, "stage" | "plan_version" | "current_task_id">): boolean {
  return item.stage === "refinement" && item.plan_version > 0 && item.current_task_id === null;
}

/** The dashboard reply box is open during refinement only. */
export function canReply(item: Pick<IntakeItem, "stage">): boolean {
  return item.stage === "refinement";
}

export function canDecideComment(item: Pick<IntakeItem, "stage" | "comment_state">): boolean {
  return item.stage === "review" && item.comment_state === "proposed";
}

export function canRetry(item: Pick<IntakeItem, "stage">): boolean {
  return item.stage === "failed" || item.stage === "blocked";
}

export function canCancelItem(item: Pick<IntakeItem, "stage">): boolean {
  return isOpenItem(item.stage);
}

/** Thread messages oldest first (the API already orders them; sort
 *  defensively by id). */
export function threadOrder(messages: readonly IntakeMessage[]): IntakeMessage[] {
  return [...messages].sort((a, b) => a.id - b.id);
}

/** Who a thread message is from, as shown in the thread. */
export function authorLabel(m: Pick<IntakeMessage, "author" | "via">): string {
  switch (m.author) {
    case "operator":
      return `you · ${m.via}`;
    case "agent":
      return "agent";
    default:
      return "nucleus";
  }
}

/** Where the item's WhatsApp thread runs, in words. */
export function surfaceLabel(item: Pick<IntakeItem, "surface" | "id">): string {
  switch (item.surface) {
    case "group":
      return "WhatsApp group";
    case "pending":
      return "WhatsApp group (being created)";
    case "dm":
      return `WhatsApp DM, marked #${item.id}`;
    default:
      return "not on WhatsApp yet";
  }
}
