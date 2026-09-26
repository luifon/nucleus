// Pure helpers for the Intake page (ADR-036): stage colours, and which
// operator actions an item offers. The server stays the authority for every
// action; these rules only hide an action the listing already shows would
// be refused.

import type { StatusKind } from "@/components/StatusPill";
import type { IntakeHiddenFinding, IntakeItem, IntakeMessage, IntakePlanVersion, IntakeReplyResult, IntakeStage } from "@/lib/api/intake";

/** The stages in pipeline order, for the stage track. */
export const STAGE_TRACK: readonly IntakeStage[] = ["queued", "eval", "refinement", "implementation", "pr", "closed"];

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
    case "held":
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
  if (item.stage === "failed") return "failed — retry or cancel";
  if (item.stage === "blocked") return "blocked by the secret guard — fix, then retry or cancel";
  if (item.stage === "stale") return "stale — the issue changed; add the label again for a new item";
  if (item.stage === "held") return "held — the issue has content GitHub's page does not show; review, then release or cancel";
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

export function canRetry(item: Pick<IntakeItem, "stage">): boolean {
  return item.stage === "failed" || item.stage === "blocked";
}

/** A held item can be released (after reading its findings) or cancelled. */
export function canRelease(item: Pick<IntakeItem, "stage">): boolean {
  return item.stage === "held";
}

/** Operator words for a finding kind (the values core/src/intake/hidden.rs writes). */
export function findingKindLabel(kind: string): string {
  switch (kind) {
    case "html_comment":
      return "HTML comment";
    case "invisible_characters":
      return "invisible characters";
    case "invisible_entity":
      return "entity for an invisible character";
    case "details":
      return "collapsed <details> block";
    case "html_tag":
      return "raw HTML";
    case "link_definition":
      return "link reference definition";
    case "footnote_definition":
      return "footnote definition";
    case "image_alt":
      return "image alt text";
    case "link_title":
      return "link title";
    case "link_destination":
      return "link destination";
    case "image_source":
      return "image address";
    case "fence_info":
      return "code fence info string";
    case "table_extra_cells":
      return "table cells beyond the header";
    case "math_styling":
      return "math macro outside the visible-only list";
    case "rendered_block":
      return "diagram or map block";
    default:
      return kind;
  }
}

/** A piece of a raw source: flagged when a finding covers it. */
export interface SourcePiece {
  text: string;
  flagged: boolean;
}

/** Split a raw source into pieces at the finding ranges (`start`/`end` in
 *  code points, end exclusive), so the whole source can be shown with the
 *  hidden parts marked. Overlapping ranges merge. */
export function markRanges(text: string, ranges: readonly Pick<IntakeHiddenFinding, "start" | "end">[]): SourcePiece[] {
  const cps = Array.from(text);
  const flag = new Array<boolean>(cps.length).fill(false);
  for (const r of ranges) {
    for (let i = Math.max(0, r.start); i < Math.min(cps.length, r.end); i++) flag[i] = true;
  }
  const out: SourcePiece[] = [];
  for (let i = 0; i < cps.length; i++) {
    const last = out[out.length - 1];
    if (last && last.flagged === flag[i]) last.text += cps[i];
    else out.push({ text: cps[i], flagged: flag[i] });
  }
  return out;
}

/** The short hold code (`nucleus intake release <n> --hold <code>`). */
export function holdCode(hash: string | null): string {
  return (hash ?? "").slice(0, 6);
}

/** `body 3:5` — where a finding is. */
export function findingPlace(f: Pick<IntakeHiddenFinding, "location" | "line" | "column">): string {
  return `${f.location} ${f.line}:${f.column}`;
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
  return item.surface === "dm" ? `WhatsApp DM, marked #${item.id}` : "not on WhatsApp yet";
}

/** Closed, cancelled or stale: nothing changes any more, so the item page
 *  stops refreshing and offers no actions. */
export function isTerminal(stage: IntakeStage): boolean {
  return !isOpenItem(stage);
}

/** The deep link to an item's page (`/intake?item=<n>`), which the
 *  WhatsApp notices carry. */
export function itemHref(id: number): string {
  return `/intake?item=${id}`;
}

/** The item a `/intake` URL opens: a positive integer `item` parameter,
 *  or null for the list. */
export function itemFromSearch(params: URLSearchParams): number | null {
  const raw = params.get("item")?.trim();
  if (!raw || !/^\d+$/.test(raw)) return null;
  const n = Number(raw);
  return Number.isSafeInteger(n) && n > 0 ? n : null;
}

/** Every accepted plan version, oldest first (the server already orders
 *  them; sort defensively by version). */
export function planVersions(detail: { plans: readonly IntakePlanVersion[] }): IntakePlanVersion[] {
  return [...detail.plans].sort((a, b) => a.version - b.version);
}

/** How a plan version stands: approved, the proposal waiting for a
 *  decision, or an earlier proposal a later one replaced. */
export function planStatus(version: number, item: Pick<IntakeItem, "approved_version" | "plan_version">): "approved" | "proposed" | "replaced" {
  if (item.approved_version === version) return "approved";
  if (item.approved_version === null && version === item.plan_version) return "proposed";
  return "replaced";
}

/** The approve button shows only for the version on screen, when that is
 *  the item's current proposal and the item can take an approval. */
export function canApproveShown(
  item: Pick<IntakeItem, "stage" | "plan_version" | "current_task_id">,
  shownVersion: number | null,
): boolean {
  return shownVersion !== null && shownVersion === item.plan_version && canApprovePlan(item);
}

/** The composer is open while the item is not finished. A reply is saved
 *  in every stage; outside refinement no agent reads it, and the server
 *  says so in `note`. */
export function canCompose(item: Pick<IntakeItem, "stage">): boolean {
  return isOpenItem(item.stage);
}

/** Enter sends on a keyboard with a fine pointer; Shift+Enter, an IME
 *  composition, or a touch keyboard (coarse pointer) add a new line. */
export function enterSends(e: { key: string; shiftKey: boolean; isComposing?: boolean }, coarsePointer: boolean): boolean {
  return e.key === "Enter" && !e.shiftKey && !e.isComposing && !coarsePointer;
}

/** The composer placeholder: the keyboard hint only where Enter sends
 *  (the same pointer check as `enterSends`). */
export function composerPlaceholder(coarsePointer: boolean): string {
  return coarsePointer ? "reply…" : "reply…  (Enter sends · Shift+Enter new line)";
}

/** What a reply attempt ended in. `sent` carries the server's `note` when
 *  the message was saved but no agent reads it now, and null when the agent
 *  does. `error` is any non-2xx answer (an empty message is a 409). */
export type ReplyOutcome =
  | { kind: "sent"; item: IntakeItem; note: string | null }
  | { kind: "error"; message: string };

export async function sendReply(
  id: number,
  text: string,
  post: (id: number, text: string) => Promise<IntakeReplyResult>,
): Promise<ReplyOutcome> {
  try {
    const r = await post(id, text);
    const note = r.reaches_agent ? null : (r.note ?? "Saved in the thread. No agent reads it now.");
    return { kind: "sent", item: r.item, note };
  } catch (e) {
    return { kind: "error", message: e instanceof Error ? e.message : String(e) };
  }
}
