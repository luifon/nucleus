// Brain-dump capture flow (ADR-005a review-before-apply), for the braindump
// group role. Voice memos are always new captures (expire a prior plan,
// transcribe, plan). Text messages route on pending-plan state: a pending
// plan → interpret the reply; none → plan a new capture.
//
// Every message to the chat — acknowledgements, the rundown, outcomes,
// failures, expiry notices — goes through `d.send`, which the bot binds to
// the outbound queue (ADR-033): retried, allowlist-checked and passed
// through the secret filter like every other reply. Nothing here sends on
// the socket.
//
// Idempotency (ADR-033). The bot records an inbound message as handled only
// after this flow returns, so a message whose handling was cut short (the
// bot stopped) is handled again when WhatsApp delivers it again. Handling it
// again creates nothing new:
//   - every message to the chat carries a dedup key: `<msgid>:<purpose>`
//     for replies to the message, `plan:<planid>:<notice>` for notices about
//     a plan; the queue ignores a second row with the same key;
//   - a plan records the message it was planned from (found again instead
//     of planning a second time) and the message that resolved it, with the
//     action; an apply records every filed op, so it resumes instead of
//     filing again, and its outcome, which is sent again under the same key.

import type { WAMessage } from "@whiskeysockets/baileys";
import type { AppliedOp, CaptureOutcome, InterpretResult, PlanForReview, OpPatch } from "./braindump.js";
import { formatRundown, planForReviewFromRow } from "./braindump.js";
import { shortPlanId, type PendingPlanRow, type PendingPlansStore } from "./db.js";
import { fill, type BotTexts } from "./texts.js";

export interface BraindumpDeps {
  plansStore: PendingPlansStore;
  texts: BotTexts;
  /** Queue `body` for `chatId` (the bot formats and enqueues it). A second
   *  send with the same non-null `dedupKey` is ignored. */
  send(chatId: string, body: string, dedupKey: string | null): void;
  presence(chatId: string, state: "composing" | "paused"): Promise<void>;
  extractText(msg: WAMessage): string;
  /** Download and transcribe a voice memo; throws on failure. */
  transcribeVoice(msg: WAMessage): Promise<string>;
  /** Plan a capture; the plan records `sourceMsgId`. */
  planCapture(text: string, inputKind: "text" | "voice", chatId: string, sourceMsgId: string | null): Promise<PlanForReview>;
  interpretResponse(
    pending: ReturnType<PendingPlansStore["mostRecentPending"]> & {},
    replyText: string,
  ): Promise<InterpretResult>;
  /** File a plan's accepted ops (resumes a started apply); `byMsgId` is
   *  the accepting message. */
  applyPlan(planId: string, ids: number[] | "all", patches: OpPatch[], byMsgId: string | null): CaptureOutcome;
  /** Diary line for the braindump agent. */
  diary(line: string, tag: "OBSERVATION"): void;
  log: {
    info(o: object, m: string): void;
    warn(o: object, m: string): void;
    error(o: object, m: string): void;
  };
}

/** Replies to one inbound message: dedup keys `<msgid>:<purpose>`. */
interface Reply {
  chatId: string;
  msgId: string | null;
  say(purpose: string, body: string): void;
}

function replyTo(d: BraindumpDeps, chatId: string, msgId: string | null): Reply {
  return {
    chatId,
    msgId,
    say: (purpose, body) => d.send(chatId, body, msgId ? `${msgId}:${purpose}` : null),
  };
}

export async function handleBrainDump(d: BraindumpDeps, msg: WAMessage, chatId: string): Promise<void> {
  const r = replyTo(d, chatId, msg.key?.id ?? null);
  // A message handled before (the bot stopped before recording it as
  // handled) continues from what it already did.
  if (r.msgId && resumeHandled(d, r)) return;

  // Voice memos can't realistically be a reply to a structured plan, so we
  // treat them as new captures unconditionally. A prior pending plan (if
  // any) is expired with a notice.
  if (msg.message?.audioMessage) {
    r.say("received", d.texts.received);
    const dur = msg.message.audioMessage.seconds ?? 0;
    r.say("transcribing", fill(d.texts.transcribing, { seconds: dur }));

    let text: string;
    try {
      text = await d.transcribeVoice(msg);
      d.log.info({ chatId, transcribedChars: text.length, seconds: dur }, "whatsapp: transcribed");
    } catch (e) {
      const err = (e as Error).message;
      d.log.error({ err }, "whatsapp: transcription failed");
      r.say("transcription-failed", fill(d.texts.transcriptionFailed, { error: err }));
      return;
    }
    if (!text.trim()) return;
    expireAnyPendingPlan(d, chatId, r.msgId);
    await handleNewCapture(d, r, text, "voice");
    return;
  }

  const text = d.extractText(msg);
  if (!text.trim()) return;

  const pending = d.plansStore.mostRecentPending(chatId);
  if (pending) {
    await handlePlanResponse(d, r, text, pending);
  } else {
    r.say("received", d.texts.received);
    await handleNewCapture(d, r, text, "text");
  }
}

/** Continue a message that was handled before. Returns true when nothing
 *  is left to do. A plan planned from the message is shown again (the
 *  queue drops the copy if the rundown was queued); a plan the message
 *  rejected or applied gets its reply again, and an apply cut short
 *  resumes. A message that only superseded a plan goes on as a new
 *  capture. */
function resumeHandled(d: BraindumpDeps, r: Reply): boolean {
  const own = d.plansStore.bySourceMsg(r.chatId, r.msgId!);
  if (own) {
    const plan = planForReviewFromRow(own);
    if (own.status === "pending") r.say("rundown", formatRundown(plan));
    else if (plan.ops.length === 0) r.say("nothing-to-file", d.texts.nothingToFile);
    d.log.warn({ chatId: r.chatId, planId: plan.shortId, status: own.status }, "whatsapp: braindump message handled again — plan already exists");
    return true;
  }
  for (const p of d.plansStore.resolvedByMsg(r.chatId, r.msgId!)) {
    const shortId = shortPlanId(p.id);
    if (p.resolvedAction === "reject") {
      r.say("cancelled", fill(d.texts.planCancelled, { id: shortId }));
      return true;
    }
    if (p.resolvedAction === "apply") {
      d.log.warn({ chatId: r.chatId, planId: shortId, status: p.status }, "whatsapp: braindump apply handled again — resuming");
      finishApply(d, r, p, [], []);
      return true;
    }
    if (p.resolvedAction === "supersede") d.send(r.chatId, fill(d.texts.planSuperseded, { id: shortId }), `plan:${p.id}:superseded`);
  }
  return false;
}
/** New-capture path: spawn planning Claude session (which sends its own
 *  🧠 ack via src/ack.ts), receive plan, send rundown. The plan row is
 *  persisted inside planCapture; the operator's reply will be handled by
 *  a subsequent inbound message → handlePlanResponse. */
async function handleNewCapture(
  d: BraindumpDeps,
  r: Reply,
  text: string,
  inputKind: "text" | "voice",
): Promise<void> {
  const chatId = r.chatId;
  await d.presence(chatId, "composing");
  let plan: PlanForReview;
  try {
    plan = await d.planCapture(text, inputKind, chatId, r.msgId);
  } catch (e) {
    const err = (e as Error).message;
    d.log.error({ err }, "whatsapp: braindump planning failed");
    r.say("plan-failed", fill(d.texts.planFailed, { error: err }));
    await d.presence(chatId, "paused");
    return;
  }

  // No-op plan: Claude decided nothing needs filing (e.g. capture was a
  // meta-test, or the operator said "ignore this"). Skip the rundown +
  // review cycle entirely — there's nothing to approve. Just confirm
  // and resolve. Avoids a "plan #X / (no ops) / reply in free text"
  // message that asks for a reply with nothing to reply about.
  if (plan.ops.length === 0) {
    d.plansStore.resolve(plan.planId, "applied", "no-op plan (claude returned 0 ops)");
    r.say("nothing-to-file", d.texts.nothingToFile);
    await d.presence(chatId, "paused");
    d.log.info(
      { chatId, planId: plan.shortId, summary: plan.summary, elapsedMs: plan.elapsedMs },
      "whatsapp: braindump no-op plan auto-resolved",
    );
    d.diary(
      `plan #${plan.shortId} no-op from ${inputKind} (${text.length}c): ${plan.summary || "(no summary)"}`,
      "OBSERVATION",
    );
    return;
  }

  r.say("rundown", formatRundown(plan));
  await d.presence(chatId, "paused");

  d.log.info(
    {
      chatId,
      planId: plan.shortId,
      ops: plan.ops.length,
      confidence: plan.confidence,
      elapsedMs: plan.elapsedMs,
    },
    "whatsapp: braindump plan ready, awaiting review",
  );
  d.diary(
    `plan #${plan.shortId} from ${inputKind} (${text.length}c) → ${plan.summary} (${(plan.confidence * 100).toFixed(0)}% conf, ${(plan.elapsedMs / 1000).toFixed(1)}s, ${plan.ops.length} ops; awaiting review)`,
    "OBSERVATION",
  );
}

/** Plan-response path: spawn response-interpreter, branch on action. */
async function handlePlanResponse(
  d: BraindumpDeps,
  r: Reply,
  replyText: string,
  pending: PendingPlanRow,
): Promise<void> {
  const chatId = r.chatId;
  r.say("interpreting", d.texts.interpreting);
  await d.presence(chatId, "composing");

  let result: InterpretResult;
  try {
    result = await d.interpretResponse(pending, replyText);
  } catch (e) {
    const err = (e as Error).message;
    d.log.error({ err, planId: shortPlanId(pending.id) }, "whatsapp: interpret failed");
    r.say("interpret-failed", fill(d.texts.interpretFailed, { error: err }));
    await d.presence(chatId, "paused");
    return;
  }

  const shortId = shortPlanId(pending.id);
  d.log.info({ chatId, planId: shortId, action: result.action, ids: result.ids }, "whatsapp: braindump interpret");

  if (result.action === "ambiguous") {
    const note = result.note ?? d.texts.notUnderstood;
    r.say("not-understood", note);
    await d.presence(chatId, "paused");
    return;
  }

  if (result.action === "reject") {
    d.plansStore.resolve(pending.id, "rejected", result.note ?? "operator rejected", { msgId: r.msgId, action: "reject" });
    r.say("cancelled", fill(d.texts.planCancelled, { id: shortId }));
    await d.presence(chatId, "paused");
    d.diary(
      `plan #${shortId} rejected by operator${result.note ? ` (${result.note})` : ""}`,
      "OBSERVATION",
    );
    return;
  }

  if (result.action === "new_capture") {
    // Operator sent fresh content instead of replying. Expire this plan
    // and re-process the message as a new capture.
    d.plansStore.resolve(pending.id, "expired", "superseded by new capture", { msgId: r.msgId, action: "supersede" });
    d.send(chatId, fill(d.texts.planSuperseded, { id: shortId }), `plan:${pending.id}:superseded`);
    await d.presence(chatId, "paused");
    await handleNewCapture(d, r, replyText, "text");
    return;
  }

  // action === "apply" | "modify". `modify` carries field-level patches
  // (a placement/naming correction the operator made at review) which
  // applyPlan applies to the ops before filing — so a date/bucket/rename
  // fix files the same turn instead of cancelling the plan.
  r.say("applying", result.action === "modify" ? d.texts.correcting : d.texts.applying);
  finishApply(d, r, pending, result.ids ?? "all", result.patches ?? []);
  await d.presence(chatId, "paused");
}

/** File a plan (or resume a started apply) and queue the outcome. A plan
 *  already applied only gets its stored outcome queued again. */
function finishApply(
  d: BraindumpDeps,
  r: Reply,
  plan: PendingPlanRow,
  ids: number[] | "all",
  patches: OpPatch[],
): void {
  const shortId = shortPlanId(plan.id);
  let outcome: CaptureOutcome;
  if ((plan.status === "applied" || plan.status === "partial") && plan.outcomeJson) {
    outcome = JSON.parse(plan.outcomeJson) as CaptureOutcome;
  } else {
    try {
      outcome = d.applyPlan(plan.id, ids, patches, r.msgId);
    } catch (e) {
      const err = (e as Error).message;
      d.log.error({ err, planId: shortId }, "whatsapp: applyPlan failed");
      r.say("apply-failed", fill(d.texts.applyFailed, { error: err }));
      return;
    }
    d.log.info(
      {
        chatId: r.chatId,
        planId: shortId,
        ops: outcome.ops.length,
        ok: outcome.ops.filter((o) => o.status === "ok").length,
        rejected: outcome.ops.filter((o) => o.status === "rejected").length,
        elapsedMs: outcome.elapsedMs,
      },
      "whatsapp: braindump plan applied",
    );
    d.diary(
      `plan #${shortId} applied (${outcome.ops.filter((o) => o.status === "ok").length}/${outcome.ops.length} ok)`,
      "OBSERVATION",
    );
  }
  r.say("outcome", formatOutcomeReply(outcome.summary, outcome.confidence, outcome.ops));
}

/** Expire any pending plan for this chat, notifying the operator. Called
 *  on voice-memo arrival (and as a defensive sweep before new captures)
 *  so a stale plan doesn't compete with the new one. */
function expireAnyPendingPlan(d: BraindumpDeps, chatId: string, byMsgId: string | null): void {
  const expired = d.plansStore.expirePendingForChat(chatId, "superseded by new capture", byMsgId);
  for (const id of expired) {
    const sid = shortPlanId(id);
    d.send(chatId, fill(d.texts.planSuperseded, { id: sid }), `plan:${id}:superseded`);
  }
}

/** Expire `pending_plans` rows older than `timeoutMs` and tell each
 *  affected chat. Handles the "operator walked away" case where no inbound
 *  traffic triggers the on-entry sweep; the bot runs it on a timer. */
export function sweepExpiredPlans(d: BraindumpDeps, timeoutMs: number): void {
  let rows: ReturnType<PendingPlansStore["sweepExpired"]>;
  try {
    rows = d.plansStore.sweepExpired(timeoutMs);
  } catch (e) {
    d.log.warn({ err: (e as Error).message }, "whatsapp: plan sweep failed");
    return;
  }
  if (rows.length === 0) return;
  d.log.info({ count: rows.length }, "whatsapp: swept expired braindump plans");
  for (const row of rows) d.send(row.chatId, fill(d.texts.planExpired, { id: shortPlanId(row.id) }), `plan:${row.id}:expired`);
}

/** Format a multi-op outcome as a human-readable WhatsApp reply.
 *
 *  Format:
 *    <summary> (<conf>% confidence)
 *
 *    + 3-Projects/Example-Project/contract.md
 *    + 3-Projects/Example-Project/team.md
 *    ↑ 4-Areas/Career/relationships.md (appended)
 *    → 3-Projects/Example-Project/overview.md (moved from 0-Inbox/old.md)
 *    ✗ 3-Projects/X (rejected: sub-folder X doesn't exist)
 *
 *  Glyphs are a small dialect: + = create, ↑ = append, → = move,
 *  ✗ = rejected. Reads well in WhatsApp's monospace renderer.
 */
export function formatOutcomeReply(
  summary: string,
  confidence: number,
  ops: AppliedOp[],
): string {
  const conf = (confidence * 100).toFixed(0);
  const lines: string[] = [`${summary} (${conf}% confidence)`, ""];
  for (const op of ops) {
    if (op.status === "rejected") {
      lines.push(`✗ ${op.op} rejected: ${op.rejection ?? "(no reason)"}`);
      continue;
    }
    switch (op.op) {
      case "create":
        lines.push(`+ ${op.resultPath}`);
        break;
      case "append":
        lines.push(`↑ ${op.resultPath} (appended)`);
        break;
      case "move":
        lines.push(`→ ${op.resultPath} (moved from ${op.fromPath})`);
        break;
    }
  }
  return lines.join("\n");
}
