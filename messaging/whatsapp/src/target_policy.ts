// Which chats a message may go to (ADR-033). Every path that puts a message
// on WhatsApp applies this one policy, whoever the caller is:
//
//   - the outbound queue drain (outbound_drain.ts, through index.ts),
//   - send.ts, which sends on its own connection,
//   - the queue writers ack.ts and enqueue-media.ts, at enqueue time.
//
// A target is allowed when it is the operator's DM (a number or JID whose
// digits are in WHATSAPP_ALLOWED_DM_JIDS) or a configured group: a group JID
// in WHATSAPP_ALLOWED_CHAT_IDS / WHATSAPP_BRAINDUMP_CHAT_IDS, or a group
// whose name is in WHATSAPP_ALLOWED_GROUP_NAMES /
// WHATSAPP_BRAINDUMP_GROUP_NAMES. Nothing else is sendable, so a caller that
// evades caller detection (caller_guard.ts) still cannot reach another chat.

import { normalizeSenderId } from "./config.js";

/** The configuration the policy reads (a subset of Config). */
export interface TargetConfig {
  allowedDmSenders: ReadonlySet<string>;
  /** The operator's LIDs (WHATSAPP_OPERATOR_LIDS, and in the bot also the
   *  LIDs its mapping resolved to the operator): sendable `@lid` DMs. */
  operatorLids?: ReadonlySet<string>;
  allowedChatIds: readonly string[];
  brainDumpChatIds: readonly string[];
  allowedGroupNames: readonly string[];
  brainDumpGroupNames: readonly string[];
}

/** A group the paired account participates in. */
export interface GroupInfo {
  jid: string;
  subject: string;
}

export type GroupRole = "whatsapp-group" | "braindump";

/** The allowed groups: configured JIDs, plus participating groups whose
 *  name is configured (case-insensitive). A JID in both lists is a
 *  brain-dump chat. */
export class GroupAllowlist {
  readonly roles = new Map<string, GroupRole>();
  readonly byName = new Map<string, string>();

  constructor(config: TargetConfig, groups: readonly GroupInfo[] = []) {
    for (const jid of config.allowedChatIds) this.roles.set(jid, "whatsapp-group");
    for (const jid of config.brainDumpChatIds) this.roles.set(jid, "braindump");
    const wantGroup = new Set(config.allowedGroupNames.map((n) => n.toLowerCase()));
    const wantBrainDump = new Set(config.brainDumpGroupNames.map((n) => n.toLowerCase()));
    for (const g of groups) {
      const name = g.subject.trim();
      if (!name || !g.jid.endsWith("@g.us")) continue;
      const lower = name.toLowerCase();
      if (wantBrainDump.has(lower)) {
        this.roles.set(g.jid, "braindump");
        this.byName.set(lower, g.jid);
      } else if (wantGroup.has(lower)) {
        this.roles.set(g.jid, "whatsapp-group");
        this.byName.set(lower, g.jid);
      }
    }
  }
}

/** True when `target` names the operator's DM. */
export function isOperatorDm(target: string, config: TargetConfig): boolean {
  if (target.endsWith("@g.us")) return false;
  if (!(target.endsWith("@lid") || target.includes("@s.whatsapp.net") || /^\d{8,15}$/.test(target))) return false;
  const digits = normalizeSenderId(target);
  if (digits.length > 0 && target.endsWith("@lid") && config.operatorLids?.has(digits)) return true;
  return digits.length > 0 && config.allowedDmSenders.has(digits);
}

/** True when a queued message may only go to the operator (ADR-036): the
 *  `dm` shorthand, and every message from the issue pipeline, reminders
 *  and vault checks, whatever target it names. Chat-engine replies (and
 *  other replies in the chat that wrote) are not operator-only: they go to
 *  the allowed contact who wrote. */
export function isOperatorOnly(target: string, source: string): boolean {
  return target === "dm" || isWorkSource(source) || source === "reminders" || source === "vault-check";
}

/** A queue source of the work pipeline (ADR-036): `work` or `work:*`, and
 *  `intake` or `intake:*` for rows queued before the feature was renamed
 *  from intake to work. */
export function isWorkSource(source: string): boolean {
  return /^(work|intake)(:|$)/.test(source);
}

/** A work queue source with the pre-rename prefix replaced: `intake:4`
 *  reads as `work:4`. Other sources are returned unchanged. */
export function workSource(source: string): string {
  return source.replace(/^intake(?=:|$)/, "work");
}

/** The operator's DM chat for the `dm` shorthand: the most recently active
 *  chat (newest first in `chats`) that `isOperatorChat` accepts, else his
 *  phone JID. Another allowed contact's chat is never chosen, however
 *  recent. Pure. */
export function pickOperatorDm(
  chats: readonly string[],
  isOperatorChat: (chatId: string) => boolean,
  operatorPhone: string | null,
): string | null {
  const hit = chats.find((c) => !c.endsWith("@g.us") && isOperatorChat(c));
  if (hit) return hit;
  return operatorPhone ? `${operatorPhone}@s.whatsapp.net` : null;
}

/** The target policy for a queued message (the drain): `resolveTarget`,
 *  and for an operator-only message (`isOperatorOnly`) the resolved DM
 *  must be the operator by the live check at send time (`isOperator`, the
 *  shared `isOperatorId` with the live LID mapping). `dm` resolves to
 *  `operatorDm()` when that chat passes the live check, and to the
 *  operator's phone JID otherwise. Groups are decided by `resolveTarget`
 *  alone. */
export async function resolveQueuedTarget(input: {
  target: string;
  source: string;
  config: TargetConfig;
  groups: GroupAllowlist;
  operatorDm: () => string | null;
  operatorPhone: string | null;
  isOperator: (jid: string) => Promise<boolean>;
  /** Called when the live check rejects an `@lid` target of an
   *  operator-only or task-result row; `to` is the operator's phone JID the
   *  row is redirected to (the caller drops the LID's verification and
   *  moves the row). */
  onStaleLid?: (lid: string, to: string) => void;
}): Promise<string | null> {
  const { target, config, groups } = input;
  const phone = () => (input.operatorPhone ? resolveTarget(input.operatorPhone, config, groups) : null);
  const stale = (lid: string): string | null => {
    const to = phone();
    if (to) input.onStaleLid?.(lid, to);
    return to;
  };
  if (target === "dm") {
    const chat = input.operatorDm();
    const jid = chat ? resolveTarget(chat, config, groups) : null;
    if (jid && (await input.isOperator(jid))) return jid;
    if (jid && jid.endsWith("@lid")) return stale(jid);
    return phone();
  }
  const jid = resolveTarget(target, config, groups);
  const operatorOnly = isOperatorOnly(target, input.source);
  if (target.endsWith("@lid")) {
    const digits = normalizeSenderId(target);
    // Another allowed contact's LID keeps its own replies and task results.
    if (!operatorOnly && jid && config.allowedDmSenders.has(digits)) return jid;
    if (operatorOnly || isTaskResult(input.source) || !jid) {
      if (await input.isOperator(target)) return jid ?? target;
      // The LID is no longer the operator's by the live mapping: an
      // operator-only row or a task result goes to his phone instead of
      // being dropped; anything else is refused.
      return operatorOnly || isTaskResult(input.source) ? stale(target) : null;
    }
    return jid;
  }
  if (!jid || jid.endsWith("@g.us") || !operatorOnly) return jid;
  return (await input.isOperator(jid)) ? jid : null;
}

/** A task's result or delivery note (tasks.rs uses the sender `task:<id>`). */
export function isTaskResult(source: string): boolean {
  return source.startsWith("task:");
}

/** The JID to send `target` to, or null when it is not allowed. `dm` is
 *  resolved by `operatorDm` (the queue's shorthand for the operator's DM).
 *  An operator DM keeps an `@lid` form as is (WhatsApp delivers some DMs
 *  under it) and becomes `<digits>@s.whatsapp.net` otherwise. Pure. */
export function resolveTarget(
  target: string,
  config: TargetConfig,
  groups: GroupAllowlist,
  operatorDm: () => string | null = () => null,
): string | null {
  if (target === "dm") {
    const chat = operatorDm();
    return chat && chat !== "dm" ? resolveTarget(chat, config, groups) : null;
  }
  if (target.endsWith("@g.us")) return groups.roles.has(target) ? target : null;
  if (target.includes("@") || /^\d+$/.test(target)) {
    if (!isOperatorDm(target, config)) return null;
    return target.endsWith("@lid") ? target : `${normalizeSenderId(target)}@s.whatsapp.net`;
  }
  const jid = groups.byName.get(target.toLowerCase());
  return jid && groups.roles.has(jid) ? jid : null;
}

/** Enqueue-time check for a queue writer, which has no connection to look
 *  groups up: the target must be `dm`, the operator's DM, a configured
 *  group JID, or a configured group name. The drain resolves it again.
 *  Returns the refusal reason, or null. Pure. */
export function enqueueRefusal(target: string, config: TargetConfig): string | null {
  if (target === "dm" || isOperatorDm(target, config)) return null;
  if (target.endsWith("@g.us")) {
    return config.allowedChatIds.includes(target) || config.brainDumpChatIds.includes(target)
      ? null
      : "the group JID is not in WHATSAPP_ALLOWED_CHAT_IDS or WHATSAPP_BRAINDUMP_CHAT_IDS (a configured group may be named instead)";
  }
  if (target.includes("@") || /^\d+$/.test(target)) {
    return "the number is not the operator's DM (WHATSAPP_ALLOWED_DM_JIDS)";
  }
  const lower = target.toLowerCase();
  const named = [...config.allowedGroupNames, ...config.brainDumpGroupNames].some((n) => n.toLowerCase() === lower);
  return named ? null : "the group name is not in WHATSAPP_ALLOWED_GROUP_NAMES or WHATSAPP_BRAINDUMP_GROUP_NAMES";
}
