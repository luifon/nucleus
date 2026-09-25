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
  return digits.length > 0 && config.allowedDmSenders.has(digits);
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
