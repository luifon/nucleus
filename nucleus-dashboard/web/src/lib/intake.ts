// Pure helpers for the Intake page (ADR-036): stage colours, and which
// operator actions an item offers. The server stays the authority for every
// action; these rules only hide an action the listing already shows would
// be refused.

import type { StatusKind } from "@/components/StatusPill";
import type {
  IntakeDecision,
  IntakeHiddenFinding,
  IntakeItem,
  IntakeMessage,
  IntakePlanVersion,
  IntakeQuestion,
  IntakeReplyKind,
  IntakeReplyOutcome,
  IntakeReplyResult,
  IntakeStage,
} from "@/lib/api/intake";
import { answeredIds, describeResponse, parseMessage, type CanvasBlockData, type ParsedResponse } from "@/lib/canvas";

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

/** The line above the composer: what happens to a message in this stage;
 *  null during refinement (the agent reads it, or Nucleus takes a decision
 *  written in words). */
export function composerHint(stage: IntakeStage): string | null {
  if (stage === "refinement") return null;
  if (stage === "held") {
    return "Nucleus reads your message: a decision in your own words (release, cancel) is taken as on the board, with a confirmation first; anything else is saved in the thread.";
  }
  return `saved in the thread; the agent reads replies only during refinement, and this item is in ${stage}`;
}

/** What a reply attempt ended in. `sent` carries what the server did
 *  (`outcome`) and the text the composer shows (`note`, null when nothing
 *  needs saying: a message the refinement agent reads). `error` is any
 *  non-2xx answer (an empty message is a 409). */
export type ReplyOutcome =
  | { kind: "sent"; item: IntakeItem; outcome: IntakeReplyOutcome; note: string | null }
  | { kind: "error"; message: string };

export async function sendReply(
  id: number,
  text: string,
  post: (id: number, text: string, kind?: IntakeReplyKind) => Promise<IntakeReplyResult>,
  kind: IntakeReplyKind = "text",
): Promise<ReplyOutcome> {
  try {
    const r = await post(id, text, kind);
    return { kind: "sent", item: r.item, outcome: r.outcome, note: replyNotice(r) };
  } catch (e) {
    return { kind: "error", message: e instanceof Error ? e.message : String(e) };
  }
}

/** What the composer shows after the server took a message. */
export function replyNotice(r: Pick<IntakeReplyResult, "outcome" | "decision" | "reaches_agent" | "note">): string | null {
  switch (r.outcome) {
    case "discussion":
      return r.reaches_agent ? null : (r.note ?? "Saved in the thread. No agent reads it now.");
    case "decision":
      return decisionDone(r.decision);
    case "question":
      return "Nucleus asks you to confirm first: answer Yes or No on the board.";
    default:
      return r.note ?? "Nothing was done.";
  }
}

/** One line for a decision that ran. */
export function decisionDone(d: IntakeDecision | null): string {
  switch (d) {
    case "approve_plan":
      return "Plan approved. Implementation starts.";
    case "release":
      return "Released. The item continues where it was held.";
    case "cancel":
      return "Item cancelled.";
    default:
      return "Done.";
  }
}

// ── the decision board ──────────────────────────────────────────────────
//
// When the item waits for the operator, the bottom of the conversation
// shows the options code derives from the item's stage and data (never
// from model text), in place of the composer. The server stays the
// authority: every option calls an explicit route that refuses what the
// item cannot take.

export type BoardOptionKey = "approve" | "release" | "retry" | "discuss" | "cancel" | "write" | "yes" | "no";

export interface BoardOption {
  key: BoardOptionKey;
  label: string;
  /** One line under the label: what choosing it does. */
  hint?: string;
  /** `down` for a destructive choice. */
  tone?: "accent" | "down";
}

/** What the bottom of the conversation shows for an item:
 *  - `closed`: neither the board nor the composer (a finished item);
 *  - `composer`: the composer only (nothing to decide; the agent waits for
 *    a reply);
 *  - `board`: a title (what the item waits for, or what runs now) and the
 *    options. */
export type Board =
  | { kind: "closed" }
  | { kind: "composer" }
  | { kind: "board"; title: string; options: BoardOption[] };

const DISCUSS: BoardOption = { key: "discuss", label: "Continue discussing", hint: "write a message instead" };
const CANCEL: BoardOption = { key: "cancel", label: "Cancel item", hint: "the item stops; asks once more", tone: "down" };
const WRITE: BoardOption = { key: "write", label: "Write a message", hint: "saved in the thread" };

/** What runs while an agent or Nucleus works on the item. */
export function workingStatus(item: Pick<IntakeItem, "stage" | "plan_version">): string {
  switch (item.stage) {
    case "queued":
      return "Nucleus is preparing the item.";
    case "eval":
      return "The agent is evaluating the issue.";
    case "refinement":
      return item.plan_version > 0
        ? `The agent is writing a reply (plan v${item.plan_version} is proposed).`
        : "The agent is writing a reply.";
    case "implementation":
      return "The agent is implementing the approved plan.";
    case "pr":
      return "Nucleus is running the tests and opening the draft pull request.";
    default:
      return `The item is in ${item.stage}.`;
  }
}

/** The board for an item, from its stage and data. */
export function boardFor(
  item: Pick<IntakeItem, "id" | "stage" | "plan_version" | "plan_draft" | "current_task_id" | "hold_hash" | "failed_stage">,
): Board {
  switch (item.stage) {
    case "closed":
    case "cancelled":
    case "stale":
      return { kind: "closed" };
    case "refinement":
      if (item.current_task_id) return { kind: "board", title: workingStatus(item), options: [WRITE] };
      if (item.plan_version > 0 && item.plan_draft !== null) {
        return {
          kind: "board",
          title: `Plan v${item.plan_version} waits for your decision.`,
          options: [
            { key: "approve", label: `Approve plan v${item.plan_version}`, hint: "implementation starts from this version", tone: "accent" },
            DISCUSS,
            CANCEL,
          ],
        };
      }
      return { kind: "composer" };
    case "held":
      return {
        kind: "board",
        title: "Held: the issue has content GitHub's page does not show, listed below. Review it, then decide.",
        options: [
          { key: "release", label: `Release (hold ${holdCode(item.hold_hash)})`, hint: "the agent reads the hidden content as data", tone: "accent" },
          DISCUSS,
          CANCEL,
        ],
      };
    case "failed":
    case "blocked":
      return {
        kind: "board",
        title: `${item.stage === "failed" ? "Failed" : "Blocked"} in ${item.failed_stage ?? "?"}. Retry once the cause is fixed, or cancel.`,
        options: [{ key: "retry", label: "Retry", hint: `resume in ${item.failed_stage ?? "the stage it stopped in"}`, tone: "accent" }, CANCEL],
      };
    default:
      return { kind: "board", title: workingStatus(item), options: [WRITE] };
  }
}

/** Changes whenever what the board offers changes; the thread shows the
 *  board again (instead of the composer) when it does. */
export function boardKey(item: Pick<IntakeItem, "stage" | "plan_version" | "current_task_id" | "hold_hash">): string {
  return [item.stage, item.plan_version, item.current_task_id ?? "", item.hold_hash ?? ""].join("|");
}

/** The second step of "Cancel item". */
export function cancelStep(id: number): { title: string; options: BoardOption[] } {
  return {
    title: `Cancel item #${id}? Its running task stops. A cancelled item cannot be resumed.`,
    options: [
      { key: "yes", label: "Yes, cancel it", tone: "down" },
      { key: "no", label: "No, keep it" },
    ],
  };
}

/** The Yes / No step for a confirmation question the server asked after
 *  text typed on the page. The title is built from the decision and what it
 *  binds to, never from model text. */
export function questionStep(id: number, q: Pick<IntakeQuestion, "decision" | "plan_version" | "hold_hash">): { title: string; options: BoardOption[] } {
  const title =
    q.decision === "approve_plan"
      ? `Approve plan v${q.plan_version ?? "?"} of item #${id}? Implementation starts from that version.`
      : q.decision === "release"
        ? `Release item #${id} (hold ${holdCode(q.hold_hash)})? The agent reads the hidden content as data.`
        : `Cancel item #${id}? Its running task stops. A cancelled item cannot be resumed.`;
  return {
    title,
    options: [
      { key: "yes", label: "Yes", tone: q.decision === "cancel" ? "down" : "accent" },
      { key: "no", label: "No", hint: "nothing is done" },
    ],
  };
}

/** Keyboard selection on the board, as in Claude Code's option prompts:
 *  ArrowUp / ArrowLeft move up, ArrowDown / ArrowRight move down (both wrap
 *  around), Home and End jump to the ends. Returns the new highlighted
 *  index, or null when the key does not move the highlight. */
export function moveHighlight(current: number, key: string, count: number): number | null {
  if (count <= 0) return null;
  switch (key) {
    case "ArrowUp":
    case "ArrowLeft":
      return (current - 1 + count) % count;
    case "ArrowDown":
    case "ArrowRight":
      return (current + 1) % count;
    case "Home":
      return 0;
    case "End":
      return count - 1;
    default:
      return null;
  }
}

// ── canvas questions in agent replies (ADR-012) ─────────────────────────

/** The ids of the canvas blocks the operator answered: a later operator
 *  message carries a canvas response for the id (the chat's derivation). */
export function threadAnsweredIds(messages: readonly Pick<IntakeMessage, "author" | "body">[]): Set<string> {
  return answeredIds(messages.map((m) => ({ role: m.author === "operator" ? "user" : "assistant", content: m.body })));
}

/** One line for an operator's canvas response: the option labels of the
 *  block it answers when that block is known, the raw keys otherwise. */
export function canvasAnswerText(r: ParsedResponse, block: CanvasBlockData | undefined): string {
  const v = r.value as Record<string, unknown> | null;
  const label = (key: unknown) => block?.options?.find((o) => o.key === key)?.label ?? String(key);
  const title = block?.title ?? block?.prompt ?? r.id;
  if (v && typeof v === "object") {
    if (typeof v["choice"] === "string") return `${title}: ${label(v["choice"])}`;
    if (Array.isArray(v["selected"])) {
      const picked = (v["selected"] as unknown[]).map(label);
      return `${title}: ${picked.length > 0 ? picked.join(", ") : "(none)"}`;
    }
    if (typeof v["confirmed"] === "boolean") return `${title}: ${v["confirmed"] ? "yes" : "no"}`;
  }
  return describeResponse(r);
}

/** Every canvas block in the agent's replies, by id. */
export function threadBlocks(messages: readonly Pick<IntakeMessage, "author" | "body">[]): Map<string, CanvasBlockData> {
  const out = new Map<string, CanvasBlockData>();
  for (const m of messages) {
    if (m.author !== "agent" || !m.body.includes("<canvas")) continue;
    for (const s of parseMessage(m.body)) if (s.kind === "canvas") out.set(s.block.id, s.block);
  }
  return out;
}
