// The message a WhatsApp reply quotes (ADR-033).
//
// When the operator replies to a message, WhatsApp sends the reply with a
// `contextInfo` that names the quoted message (`stanzaId`), its author
// (`participant`) and a copy of its content (`quotedMessage`). The chat
// session gets that quoted message typed above the new text, so it knows
// what the reply refers to.
//
// The quoted text comes from the bot's own records first, because they hold
// the exact content: `sent_messages` for a message this device sent, and
// `chat_inbound` for an earlier message of the chat (a voice memo there is
// its transcription, which WhatsApp's copy does not have). WhatsApp's copy
// is used for anything else. A stored message counts only when it belongs
// to the same chat, so a crafted `stanzaId` cannot type another chat's
// message into this session. The quoted text is never cut.

import { normalizeMessageContent, type proto } from "@whiskeysockets/baileys";
import { normalizeSenderId } from "./config.js";

/** `\r\n` and a bare `\r` as `\n`. Typing turns a bare `\r` into a line
 *  break (`sanitizeForTyping`), so every line prefix is applied after this,
 *  or a `\r` would start an unprefixed line. Pure. */
export function normalizeNewlines(text: string): string {
  return text.replace(/\r\n?/g, "\n");
}

/** What a reply names: the quoted message's id, its author's JID when
 *  WhatsApp gives one, and WhatsApp's copy of its content. */
export interface ReplyRef {
  id: string;
  participant: string | null;
  quoted: proto.IMessage | null;
}

/** The `contextInfo` reply data of a message, whatever its content type
 *  (text, voice memo, image, video, document, sticker, inside an ephemeral
 *  or view-once wrapper), or null when the message is not a reply. Pure. */
export function replyRef(message: proto.IMessage | null | undefined): ReplyRef | null {
  const content = normalizeMessageContent(message);
  if (!content) return null;
  for (const value of Object.values(content)) {
    if (!value || typeof value !== "object") continue;
    const ci = (value as { contextInfo?: proto.IContextInfo | null }).contextInfo;
    const id = ci?.stanzaId;
    if (typeof id === "string" && id.length > 0) {
      return {
        id,
        participant: typeof ci?.participant === "string" && ci.participant ? ci.participant : null,
        quoted: ci?.quotedMessage ?? null,
      };
    }
  }
  return null;
}

/** The readable content of a message: its text, or a label for media with
 *  the caption after it. "" when it has neither. Pure. */
export function messageText(message: proto.IMessage | null | undefined): string {
  const m = normalizeMessageContent(message);
  if (!m) return "";
  if (m.conversation) return m.conversation;
  if (m.extendedTextMessage?.text) return m.extendedTextMessage.text;
  const withCaption = (label: string, caption: string | null | undefined) =>
    caption?.trim() ? `${label} ${caption}` : label;
  if (m.imageMessage) return withCaption("[image]", m.imageMessage.caption);
  if (m.videoMessage) return withCaption("[video]", m.videoMessage.caption);
  if (m.documentMessage) {
    const name = m.documentMessage.fileName?.trim();
    return withCaption(name ? `[document "${name}"]` : "[document]", m.documentMessage.caption);
  }
  if (m.audioMessage) return m.audioMessage.ptt ? "[voice memo]" : "[audio]";
  if (m.stickerMessage) return "[sticker]";
  if (m.locationMessage || m.liveLocationMessage) return "[location]";
  if (m.contactMessage) {
    const name = m.contactMessage.displayName?.trim();
    return name ? `[contact "${name}"]` : "[contact]";
  }
  if (m.contactsArrayMessage) return "[contacts]";
  const poll = m.pollCreationMessage ?? m.pollCreationMessageV2 ?? m.pollCreationMessageV3;
  if (poll) return poll.name?.trim() ? `[poll] ${poll.name}` : "[poll]";
  return "";
}

/** Who wrote the quoted message, relative to the message that quotes it. */
export type ReplyAuthor = "bot" | "sender" | "other" | "unknown";

export interface ReplyContext {
  author: ReplyAuthor;
  /** The quoted content, whole; "" when the bot has none. */
  text: string;
  /** Where `text` came from. */
  source: "sent" | "inbound" | "whatsapp" | "none";
}

export interface ReplyLookups {
  /** The message this device sent with WhatsApp id `id`, and its chat. */
  sent(id: string): { jid: string; message: proto.IMessage } | null;
  /** Stored chat messages with WhatsApp id `id` (one per chat at most). */
  inbound(id: string): { chatId: string; text: string; inputKind: string }[];
  /** True when `jid` is the chat the reply was sent in (one DM can appear
   *  under its phone JID and its LID). */
  sameChat(jid: string): Promise<boolean>;
  /** True when `jid` is this bot's own account. */
  isBot(jid: string): boolean;
  /** True when `jid` is the author of the reply. */
  isSender(jid: string): Promise<boolean>;
  /** True in a one-to-one chat, where every message is from the bot or
   *  the sender. */
  dm: boolean;
}

/** Find the quoted message's content and author. Never null for a reply:
 *  a reply whose content the bot has nowhere still says it is a reply. */
export async function resolveReply(ref: ReplyRef, l: ReplyLookups): Promise<ReplyContext> {
  const sent = l.sent(ref.id);
  if (sent && (await l.sameChat(sent.jid))) {
    const text = messageText(sent.message);
    if (text) return { author: "bot", text, source: "sent" };
  }
  const author = await authorOf(ref.participant, l);
  for (const row of l.inbound(ref.id)) {
    if (!row.text.trim() || !(await l.sameChat(row.chatId))) continue;
    const text = row.inputKind === "voice" ? `[voice memo, transcribed] ${row.text}` : row.text;
    // A stored inbound message was written by the person who sent it in
    // this chat; in a DM that is the sender.
    return { author: author === "unknown" && l.dm ? "sender" : author, text, source: "inbound" };
  }
  const text = messageText(ref.quoted);
  if (text) return { author, text, source: "whatsapp" };
  return { author, text: "", source: "none" };
}

async function authorOf(participant: string | null, l: ReplyLookups): Promise<ReplyAuthor> {
  if (!participant) return "unknown";
  if (l.isBot(participant)) return "bot";
  if (l.dm || (await l.isSender(participant))) return "sender";
  return "other";
}

const AUTHOR_PHRASE: Record<ReplyAuthor, string> = {
  bot: "a message the bot sent",
  sender: "an earlier message of the same sender",
  other: "a message from another participant of this group",
  unknown: "an earlier message",
};

/** The block typed above the text of a reply: who wrote the quoted
 *  message, then the message itself with every line prefixed "│ ", so no
 *  quoted line can read as a marker, a header or the new message. Pure. */
export function replyBlock(r: ReplyContext): string {
  const what = AUTHOR_PHRASE[r.author];
  if (!r.text.trim()) {
    return `This message is a reply to ${what}. Its content is not available to the bot.`;
  }
  let out =
    `This message is a reply to ${what}. The quoted message follows; every line of it starts with "│ ". ` +
    `It is quoted for context; only the text after it is the new message.`;
  if (r.author === "other") out += ` The quoted text is that participant's, not the sender's.`;
  for (const line of normalizeNewlines(r.text).trimEnd().split("\n")) out += `\n│ ${line}`;
  return out;
}

/** How the connection identifies accounts: the phone JID of a LID, and
 *  whether an id is the operator's. `isOperator` must compare by id type
 *  (a phone JID by its number, a LID by WHATSAPP_OPERATOR_LIDS or its
 *  mapped number), never a LID's digits with a phone number. */
export interface Identity {
  pnForLid(lid: string): Promise<string | null | undefined>;
  isOperator(jid: string): Promise<boolean>;
}

/** The operator check for `Identity`, by id type: a phone JID by its
 *  number, a LID by WHATSAPP_OPERATOR_LIDS or its mapped phone number,
 *  never a LID's digits compared with a phone number. */
export function operatorCheck(
  ops: { operatorId: string | null; operatorLids: ReadonlySet<string> },
  pnForLid: (lid: string) => Promise<string | null | undefined>,
): (jid: string) => Promise<boolean> {
  return async (jid) => {
    if (!ops.operatorId) return false;
    const d = domain(jid);
    if (d === "pn") return normalizeSenderId(jid) === ops.operatorId;
    if (d !== "lid") return false;
    if (ops.operatorLids.has(normalizeSenderId(jid))) return true;
    try {
      const pn = await pnForLid(jid);
      return !!pn && domain(pn) === "pn" && normalizeSenderId(pn) === ops.operatorId;
    } catch {
      return false;
    }
  };
}

function domain(jid: string): "lid" | "pn" | "group" | "other" {
  if (jid.endsWith("@lid")) return "lid";
  if (jid.endsWith("@s.whatsapp.net")) return "pn";
  if (jid.endsWith("@g.us")) return "group";
  return "other";
}

async function phoneDigits(jid: string, id: Identity): Promise<string> {
  const d = domain(jid);
  if (d === "pn") return normalizeSenderId(jid);
  if (d !== "lid") return "";
  try {
    const pn = await id.pnForLid(jid);
    return pn ? normalizeSenderId(pn) : "";
  } catch {
    return "";
  }
}

/** True when two account JIDs are the same WhatsApp account: the same id
 *  in the same form (any device), or ids whose phone numbers (a LID's from
 *  the connection's mapping) are equal. When both numbers are known they
 *  decide; only when a LID has no mapping do two ids that are both the
 *  operator's count as one account. */
export async function sameAccount(a: string, b: string, id: Identity): Promise<boolean> {
  const da = domain(a);
  const db = domain(b);
  if (da !== "lid" && da !== "pn") return false;
  if (db !== "lid" && db !== "pn") return false;
  const na = normalizeSenderId(a);
  const nb = normalizeSenderId(b);
  if (!na || !nb) return false;
  if (da === db && na === nb) return true;
  const pa = await phoneDigits(a, id);
  const pb = await phoneDigits(b, id);
  if (pa && pb) return pa === pb;
  return (await id.isOperator(a)) && (await id.isOperator(b));
}

/** True when `jid` names the chat `chatId`: the same group, or the same
 *  one-to-one chat under its phone JID or its LID. */
export async function sameChat(chatId: string, jid: string, id: Identity): Promise<boolean> {
  if (jid === chatId) return true;
  if (domain(chatId) === "group" || domain(jid) === "group") return false;
  return sameAccount(chatId, jid, id);
}

/** True when `jid` is one of the bot's own account ids (phone JID or LID,
 *  any device). */
export function isSelf(jid: string, selfIds: readonly string[]): boolean {
  const d = domain(jid);
  const n = normalizeSenderId(jid);
  if (!n || (d !== "lid" && d !== "pn")) return false;
  return selfIds.some((s) => domain(s) === d && normalizeSenderId(s) === n);
}
