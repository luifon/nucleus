// Pure helpers for the Work page (ADR-036): stage colours, and which
// operator actions an item offers. The server stays the authority for every
// action; these rules only hide an action the listing already shows would
// be refused.

import type { StatusKind } from "@/components/StatusPill";
import type {
  WorkDecision,
  WorkHiddenFinding,
  WorkItem,
  WorkMessage,
  WorkPlanVersion,
  WorkQuestion,
  WorkReplyKind,
  WorkReplyOutcome,
  WorkReplyResult,
  WorkStage,
} from "@/lib/api/work";
import { answeredIds, describeResponse, parseMessage, type CanvasBlockData, type ParsedResponse } from "@/lib/canvas";

/** The stages in pipeline order, for the stage track. */
export const STAGE_TRACK: readonly WorkStage[] = ["queued", "eval", "refinement", "implementation", "pr", "in_review", "merged"];

/** Finished: nothing changes any more. `stale` is finished (only a new
 *  label starts new work). */
export const TERMINAL_STAGES: readonly WorkStage[] = ["merged", "not_merged", "closed", "cancelled", "stale"];

/** A stage as the operator reads it. */
export function stageLabel(stage: string): string {
  switch (stage) {
    case "queued":
      return "Queued";
    case "eval":
      return "Evaluation";
    case "refinement":
      return "Refinement";
    case "held":
      return "Held";
    case "implementation":
      return "Implementation";
    case "pr":
      return "Opening PR";
    case "in_review":
      return "In review";
    case "merged":
      return "Merged";
    case "not_merged":
      return "Not merged";
    case "blocked":
      return "Blocked";
    case "failed":
      return "Failed";
    case "cancelled":
      return "Cancelled";
    case "stale":
      return "Stale";
    case "closed":
      return "Closed";
    default:
      return stage;
  }
}

/** Not finished: the pipeline still works on it or waits for the
 *  operator. `stale` is finished (only a new label starts new work). */
export function isOpenItem(stage: WorkStage): boolean {
  return !TERMINAL_STAGES.includes(stage);
}

/** Stages where an agent or Nucleus is working and the list should refresh. */
export function isWorking(item: Pick<WorkItem, "stage" | "current_task_id">): boolean {
  if (item.stage === "queued" || item.stage === "eval" || item.stage === "implementation" || item.stage === "pr") {
    return true;
  }
  return item.stage === "refinement" && item.current_task_id !== null;
}

/** Amber while work runs, red for a failure, green when closed with a PR,
 *  accent while the operator is expected to act, faint otherwise. */
export function stageKind(item: Pick<WorkItem, "stage" | "pr_url" | "current_task_id">): StatusKind {
  switch (item.stage) {
    case "failed":
    case "blocked":
    case "stale":
      return "down";
    case "merged":
      return "ok";
    case "closed":
      return item.pr_url ? "ok" : "idle";
    case "cancelled":
    case "not_merged":
      return "idle";
    case "in_review":
      return "warn";
    case "held":
      return "warn";
    case "refinement":
      return item.current_task_id ? "warn" : "idle";
    default:
      return "warn";
  }
}

/** What the operator is expected to do next, or null. */
export function waitingOn(item: WorkItem): string | null {
  if (item.stage === "refinement" && !item.current_task_id) {
    return item.plan_version > 0 ? `plan v${item.plan_version} waits for approval or a reply` : "the agent asked for a reply";
  }
  if (item.stage === "failed") return "failed — retry or cancel";
  if (item.stage === "blocked") return "blocked by the secret guard — fix, then retry or cancel";
  if (item.stage === "stale") return "stale — the issue changed; add the label again for a new item";
  if (item.stage === "held") return "held — the issue has content GitHub's page does not show; review, then release or cancel";
  if (item.stage === "in_review") return "the draft PR waits for your review";
  return null;
}

/** A plan can be approved during refinement, when one exists and no
 *  refinement turn is running (its reply may replace the plan). */
export function canApprovePlan(item: Pick<WorkItem, "stage" | "plan_version" | "current_task_id">): boolean {
  return item.stage === "refinement" && item.plan_version > 0 && item.current_task_id === null;
}

/** The dashboard reply box is open during refinement only. */
export function canReply(item: Pick<WorkItem, "stage">): boolean {
  return item.stage === "refinement";
}

export function canRetry(item: Pick<WorkItem, "stage">): boolean {
  return item.stage === "failed" || item.stage === "blocked";
}

/** A held item can be released (after reading its findings) or cancelled. */
export function canRelease(item: Pick<WorkItem, "stage">): boolean {
  return item.stage === "held";
}

/** Operator words for a finding kind (the values core/src/work/hidden.rs writes). */
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
export function markRanges(text: string, ranges: readonly Pick<WorkHiddenFinding, "start" | "end">[]): SourcePiece[] {
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

/** The short hold code (`nucleus work release <n> --hold <code>`). */
export function holdCode(hash: string | null): string {
  return (hash ?? "").slice(0, 6);
}

/** `body 3:5` — where a finding is. */
export function findingPlace(f: Pick<WorkHiddenFinding, "location" | "line" | "column">): string {
  return `${f.location} ${f.line}:${f.column}`;
}

export function canCancelItem(item: Pick<WorkItem, "stage">): boolean {
  return isOpenItem(item.stage);
}

/** Thread messages oldest first (the API already orders them; sort
 *  defensively by id). */
export function threadOrder(messages: readonly WorkMessage[]): WorkMessage[] {
  return [...messages].sort((a, b) => a.id - b.id);
}

/** Who a thread message is from, as shown in the thread. */
export function authorLabel(m: Pick<WorkMessage, "author" | "via">): string {
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
export function surfaceLabel(item: Pick<WorkItem, "surface" | "id">): string {
  return item.surface === "dm" ? `WhatsApp DM, marked #${item.id}` : "not on WhatsApp yet";
}

/** Closed, cancelled or stale: nothing changes any more, so the item page
 *  stops refreshing and offers no actions. */
export function isTerminal(stage: WorkStage): boolean {
  return !isOpenItem(stage);
}

/** The deep link to an item's page (`/work?item=<n>`), which the
 *  WhatsApp notices carry. */
export function itemHref(id: number, search: URLSearchParams | null = null): string {
  const p = new URLSearchParams(search ?? undefined);
  p.set("item", String(id));
  // `item` first: the address WhatsApp notices use.
  const rest = [...p.entries()].filter(([k]) => k !== "item");
  const tail = rest.map(([k, v]) => `&${encodeURIComponent(k)}=${encodeURIComponent(v)}`).join("");
  return `/work?item=${id}${tail}`;
}

// ── list filters ────────────────────────────────────────────────────────
//
// The list filters by status (a group of stages) and by source (the kind
// of source an item's event came from, with its repo). Both selections are
// kept in the URL query (`status=open,in_review`, `source=github:acme/x`),
// so a reload or a link keeps them.

export type StatusFilter = "open" | "in_review" | "merged" | "not_merged" | "cancelled" | "stale";

export const STATUS_FILTERS: readonly { value: StatusFilter; label: string }[] = [
  { value: "open", label: "Open" },
  { value: "in_review", label: "In review" },
  { value: "merged", label: "Merged" },
  { value: "not_merged", label: "Not merged" },
  { value: "cancelled", label: "Cancelled" },
  { value: "stale", label: "Stale" },
];

export const DEFAULT_STATUS: readonly StatusFilter[] = ["open", "in_review"];

/** The status group of a stage. `closed` (closed at its source before a
 *  pull request existed) counts as cancelled: no work reached review. */
export function statusFilterOf(stage: WorkStage): StatusFilter {
  switch (stage) {
    case "in_review":
      return "in_review";
    case "merged":
      return "merged";
    case "not_merged":
      return "not_merged";
    case "cancelled":
    case "closed":
      return "cancelled";
    case "stale":
      return "stale";
    default:
      return "open";
  }
}

/** The source filter's key for an item: `<source>:<repo>`. */
export function sourceKey(item: { source: string; repo: string }): string {
  return `${item.source}:${item.repo}`;
}

/** One entry per source and repo the items have, sorted: a GitHub repo
 *  shows as its `owner/name`, another kind of source as `<source> · <repo>`. */
export function sourceOptions(items: readonly { source: string; repo: string }[]): { value: string; label: string }[] {
  const seen = new Map<string, string>();
  for (const i of items) {
    const key = sourceKey(i);
    if (!seen.has(key)) seen.set(key, i.source === "github" ? i.repo : `${i.source} · ${i.repo}`);
  }
  return [...seen.entries()].map(([value, label]) => ({ value, label })).sort((a, b) => a.label.localeCompare(b.label));
}

/** The list's filter: `null` means no filter (every value). */
export interface ListFilter {
  status: StatusFilter[] | null;
  source: string[] | null;
}

/** The filter a URL query holds. No `status`: the default (Open and In
 *  review); `status=all`: every status. No `source`: every source. */
export function filterFromSearch(params: URLSearchParams): ListFilter {
  const raw = params.get("status");
  const known = new Set<string>(STATUS_FILTERS.map((s) => s.value));
  const status =
    raw === null
      ? [...DEFAULT_STATUS]
      : raw === "all"
        ? null
        : raw.split(",").filter((s): s is StatusFilter => known.has(s));
  const src = params.get("source");
  const source = src === null || src === "" ? null : src.split(",").filter((s) => s !== "");
  return { status, source };
}

/** `params` with the filter written into it; other keys (`item`) stay. */
export function filterToSearch(params: URLSearchParams, filter: ListFilter): URLSearchParams {
  const p = new URLSearchParams(params);
  const isDefault =
    filter.status !== null &&
    filter.status.length === DEFAULT_STATUS.length &&
    DEFAULT_STATUS.every((s) => filter.status!.includes(s));
  if (isDefault) p.delete("status");
  else p.set("status", filter.status === null || filter.status.length === 0 ? "all" : filter.status.join(","));
  if (filter.source === null || filter.source.length === 0) p.delete("source");
  else p.set("source", filter.source.join(","));
  return p;
}

/** The items the filter lets through. */
export function applyFilter<T extends { stage: WorkStage; source: string; repo: string }>(items: readonly T[], filter: ListFilter): T[] {
  return items.filter(
    (i) =>
      (filter.status === null || filter.status.includes(statusFilterOf(i.stage))) &&
      (filter.source === null || filter.source.includes(sourceKey(i))),
  );
}

/** The dropdown's summary: the labels when two or fewer are chosen. */
export function filterSummary(labels: readonly string[], total: number, allLabel = "all"): string {
  if (labels.length === 0 || labels.length === total) return allLabel;
  if (labels.length <= 2) return labels.join(", ");
  return `${labels.length} of ${total}`;
}

/** The item a `/work` URL opens: a positive integer `item` parameter,
 *  or null for the list. */
export function itemFromSearch(params: URLSearchParams): number | null {
  const raw = params.get("item")?.trim();
  if (!raw || !/^\d+$/.test(raw)) return null;
  const n = Number(raw);
  return Number.isSafeInteger(n) && n > 0 ? n : null;
}

/** Every accepted plan version, oldest first (the server already orders
 *  them; sort defensively by version). */
export function planVersions(detail: { plans: readonly WorkPlanVersion[] }): WorkPlanVersion[] {
  return [...detail.plans].sort((a, b) => a.version - b.version);
}

/** How a plan version stands: approved, the proposal waiting for a
 *  decision, or an earlier proposal a later one replaced. */
export function planStatus(version: number, item: Pick<WorkItem, "approved_version" | "plan_version">): "approved" | "proposed" | "replaced" {
  if (item.approved_version === version) return "approved";
  if (item.approved_version === null && version === item.plan_version) return "proposed";
  return "replaced";
}

/** The approve button shows only for the version on screen, when that is
 *  the item's current proposal and the item can take an approval. */
export function canApproveShown(
  item: Pick<WorkItem, "stage" | "plan_version" | "current_task_id">,
  shownVersion: number | null,
): boolean {
  return shownVersion !== null && shownVersion === item.plan_version && canApprovePlan(item);
}

/** The composer is open while the item is not finished. A reply is saved
 *  in every stage; outside refinement no agent reads it, and the server
 *  says so in `note`. */
export function canCompose(item: Pick<WorkItem, "stage">): boolean {
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
export function composerHint(stage: WorkStage): string | null {
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
  | { kind: "sent"; item: WorkItem; outcome: WorkReplyOutcome; note: string | null }
  | { kind: "error"; message: string };

export async function sendReply(
  id: number,
  text: string,
  post: (id: number, text: string, kind?: WorkReplyKind) => Promise<WorkReplyResult>,
  kind: WorkReplyKind = "text",
): Promise<ReplyOutcome> {
  try {
    const r = await post(id, text, kind);
    return { kind: "sent", item: r.item, outcome: r.outcome, note: replyNotice(r) };
  } catch (e) {
    return { kind: "error", message: e instanceof Error ? e.message : String(e) };
  }
}

/** What the composer shows after the server took a message. */
export function replyNotice(r: Pick<WorkReplyResult, "outcome" | "decision" | "reaches_agent" | "note">): string | null {
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
export function decisionDone(d: WorkDecision | null): string {
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

export type BoardOptionKey = "approve" | "release" | "retry" | "discuss" | "cancel" | "write" | "yes" | "no" | "open_pr";

export interface BoardOption {
  key: BoardOptionKey;
  label: string;
  /** One line under the label: what choosing it does. */
  hint?: string;
  /** `down` for a destructive choice. */
  tone?: "accent" | "down";
  /** A link: the option opens this URL in a new tab. */
  href?: string;
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
export function workingStatus(item: Pick<WorkItem, "stage" | "plan_version">): string {
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
  item: Pick<WorkItem, "id" | "stage" | "plan_version" | "plan_draft" | "current_task_id" | "hold_hash" | "failed_stage" | "pr_url">,
): Board {
  switch (item.stage) {
    case "closed":
    case "cancelled":
    case "stale":
    case "merged":
    case "not_merged":
      return { kind: "closed" };
    case "in_review": {
      const n = prNumber(item.pr_url);
      const open: BoardOption[] = item.pr_url
        ? [{ key: "open_pr", label: n ? `Open PR #${n} on GitHub` : "Open the PR on GitHub", hint: "opens in a new tab", tone: "accent", href: item.pr_url }]
        : [];
      return { kind: "board", title: "The draft PR waits for your review on GitHub.", options: [...open, WRITE] };
    }
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
export function boardKey(item: Pick<WorkItem, "stage" | "plan_version" | "current_task_id" | "hold_hash">): string {
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
export function questionStep(id: number, q: Pick<WorkQuestion, "decision" | "plan_version" | "hold_hash">): { title: string; options: BoardOption[] } {
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
export function threadAnsweredIds(messages: readonly Pick<WorkMessage, "author" | "body">[]): Set<string> {
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
export function threadBlocks(messages: readonly Pick<WorkMessage, "author" | "body">[]): Map<string, CanvasBlockData> {
  const out = new Map<string, CanvasBlockData>();
  for (const m of messages) {
    if (m.author !== "agent" || !m.body.includes("<canvas")) continue;
    for (const s of parseMessage(m.body)) if (s.kind === "canvas") out.set(s.block.id, s.block);
  }
  return out;
}

// ── the next-step line ──────────────────────────────────────────────────

/** The line under the item header that says whose turn it is. `tone`:
 *  `mine` (the operator's turn, highlighted), `working`, `down` (blocked,
 *  failed, stale) or `done`. */
export interface NextStep {
  text: string;
  tone: "mine" | "working" | "down" | "done";
}

/** `…/pull/11` → `11`, or null. */
export function prNumber(url: string | null): string | null {
  const m = url ? /\/pull\/(\d+)\/?$/.exec(url) : null;
  return m ? m[1] : null;
}

/** The first line of a reason, at most `max` characters. */
function oneLine(text: string, max = 120): string {
  const line = text.trim().split("\n")[0].trim();
  return line.length > max ? `${line.slice(0, max - 1).trimEnd()}…` : line;
}

/** Whose turn it is, from the item's stage and data (never from model
 *  text). `since` is when the item entered its current stage (the last
 *  transition to it), for the working time. */
export function nextStep(
  item: Pick<WorkItem, "stage" | "plan_version" | "plan_draft" | "current_task_id" | "pr_url" | "error" | "stale_reason" | "failed_stage">,
  opts: { now: number; since: string | null; question: { decision: WorkDecision } | null },
): NextStep {
  if (opts.question && isOpenItem(item.stage)) return { text: "Your turn: answer the question below with Yes or No", tone: "mine" };
  const minutes = opts.since ? Math.floor((opts.now - Date.parse(opts.since)) / 60_000) : NaN;
  const took = Number.isFinite(minutes) && minutes >= 1 ? `, ${minutes} min` : "";
  switch (item.stage) {
    case "refinement":
      if (item.current_task_id) return { text: `Working: the agent is writing a reply${took}`, tone: "working" };
      if (item.plan_version > 0 && item.plan_draft !== null) {
        return { text: `Your turn: approve plan v${item.plan_version} or reply`, tone: "mine" };
      }
      return { text: "Your turn: answer the agent's question", tone: "mine" };
    case "held":
      return { text: "Your turn: review the hidden content, then release or cancel", tone: "mine" };
    case "in_review": {
      const n = prNumber(item.pr_url);
      return { text: `Your turn: review ${n ? `PR #${n}` : "the pull request"}`, tone: "mine" };
    }
    case "queued":
      return { text: `Working: preparing${took}`, tone: "working" };
    case "eval":
      return { text: `Working: evaluation${took}`, tone: "working" };
    case "implementation":
      return { text: `Working: implementation${took}`, tone: "working" };
    case "pr":
      return { text: `Working: tests and the draft pull request${took}`, tone: "working" };
    case "blocked":
      return { text: `Blocked: ${oneLine(item.error ?? "a check stopped a publishing step")} — retry or cancel`, tone: "down" };
    case "failed":
      return { text: `Failed in ${item.failed_stage ?? "?"}: ${oneLine(item.error ?? "a step failed")} — retry or cancel`, tone: "down" };
    case "stale":
      return { text: `Stale: ${oneLine(item.stale_reason ?? "the issue changed")}`, tone: "down" };
    case "merged":
      return { text: "Merged", tone: "done" };
    case "not_merged":
      return { text: "Not merged", tone: "done" };
    case "cancelled":
      return { text: "Cancelled", tone: "done" };
    case "closed":
      return { text: "Closed", tone: "done" };
  }
}

/** When the item entered its current stage: the last transition to it. */
export function stageSince(transitions: readonly { to_stage: string; at: string }[], stage: WorkStage): string | null {
  for (let i = transitions.length - 1; i >= 0; i--) if (transitions[i].to_stage === stage) return transitions[i].at;
  return null;
}

// ── compact thread entries ──────────────────────────────────────────────

const PLAN_BLOCK = /── plan v(\d+) ──[\s\S]*?── end of plan v\1 ──/;

/** An agent reply without its plan: the text, and the plan version it
 *  proposed (the message's `plan_version`, or, for a reply stored before
 *  plans left the thread, the version of the plan block in its text). */
export function agentReplyParts(m: Pick<WorkMessage, "body" | "plan_version">): { text: string; planVersion: number | null } {
  const found = PLAN_BLOCK.exec(m.body);
  const text = (found ? m.body.replace(PLAN_BLOCK, "") : m.body).replace(/\n{3,}/g, "\n\n").trim();
  return { text, planVersion: m.plan_version ?? (found ? Number(found[1]) : null) };
}

/** The kind of a Nucleus note, for its icon: from the code-owned text's
 *  leading symbol. */
export type NoteKind = "approved" | "started" | "pr" | "message" | "stopped" | "failed" | "blocked" | "held" | "released" | "plan" | "merged" | "note";

const NOTE_SYMBOLS: readonly [string, NoteKind][] = [
  ["✅", "approved"],
  ["🛠", "started"],
  ["📬", "pr"],
  ["💬", "message"],
  ["⏹", "stopped"],
  ["⛔", "stopped"],
  ["⚠️", "failed"],
  ["🛑", "blocked"],
  ["🔍", "held"],
  ["▶️", "released"],
  ["🧭", "plan"],
  ["📋", "plan"],
];

/** A Nucleus note as one timeline row: its kind (for the icon), one short
 *  line without the leading symbol, and the rest (collapsed). A note stored
 *  before notes had details shows its first line; the rest is the details. */
export function noteParts(m: Pick<WorkMessage, "body" | "details">): { kind: NoteKind; line: string; details: string | null } {
  let line = m.body.trim();
  let details = m.details?.trim() || null;
  if (m.details === null || m.details === undefined) {
    const [first, ...rest] = line.split("\n");
    line = first.trim();
    const more = rest.join("\n").trim();
    if (line.length > 160) {
      const cut = line.lastIndexOf(" ", 160);
      const at = cut > 80 ? cut : 160;
      details = [line.slice(at).trim(), more].filter(Boolean).join("\n\n") || null;
      line = `${line.slice(0, at).trimEnd()}…`;
    } else {
      details = more || null;
    }
  }
  let kind: NoteKind = "note";
  for (const [sym, k] of NOTE_SYMBOLS) {
    if (line.startsWith(sym)) {
      kind = k;
      line = line.slice(sym.length).replace(/^️/, "").trim();
      break;
    }
  }
  if (kind === "approved" && /merged/i.test(line)) kind = "merged";
  // The PR-link note was a sentence before it became one line.
  if (/^The draft PR link is posted on /.test(line)) line = "PR link posted on the issue";
  return { kind, line, details };
}

/** A note line split so that `PR #n` can be a link to `url` (the PR the
 *  note is about): the text before, the `#n` part, the text after. `null`
 *  when the line names no PR number or there is no URL for it. */
export function prNumberLink(line: string, url: string | null): { before: string; label: string; after: string; href: string } | null {
  const m = /#(\d+)/.exec(line);
  if (!m || !url || prNumber(url) !== m[1]) return null;
  return { before: line.slice(0, m.index), label: m[0], after: line.slice(m.index + m[0].length), href: url };
}

/** The pull request URL a note's details start with, if any. */
export function detailsPrUrl(details: string | null): string | null {
  const m = details ? /https?:\/\/\S+\/pull\/\d+/.exec(details) : null;
  return m ? m[0] : null;
}
