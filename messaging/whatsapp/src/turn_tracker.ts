// Transcript turn tracker (ADR-033). Line-for-line mirror of
// core/src/turn_tracker.rs; both run core/testdata/turn_tracker_vectors.json.
//
// Reads the Claude Code transcript JSONL incrementally and reports: a prompt
// was accepted, input typed during a running turn was absorbed, the model
// wrote text before a tool call (progress), a background command started,
// finished or never started, the session was compacted, and a turn ended
// (`system` record with subtype `turn_duration`). The chat engine delivers
// each turn's final text exactly once from these events, instead of
// returning the first text followed by a quiet transcript.

export type TrackEvent =
  | { type: "prompt"; origin: string; text: string; starts_turn: boolean }
  | { type: "absorbed"; text: string }
  | { type: "enqueued"; text: string }
  | { type: "progress"; text: string }
  | { type: "bg_started"; id: string; description: string | null }
  | { type: "bg_finished"; id: string }
  | { type: "bg_never_started"; id: string }
  | { type: "compacted"; trigger: string }
  | { type: "turn_end"; final_text: string | null; pending_bg: number };

export class TurnTracker {
  private partial = "";
  private open = false;
  private lastMsgId: string | null = null;
  private lastMsgText = "";
  private readonly pendingBg = new Set<string>();

  /** True while a turn has started and not yet ended. */
  turnOpen(): boolean {
    return this.open;
  }

  /** Background commands started and not yet reported finished. */
  pendingBackground(): number {
    return this.pendingBg.size;
  }

  /** Feed newly appended transcript text; returns the events it produced. */
  feed(chunk: string): TrackEvent[] {
    this.partial += chunk;
    const out: TrackEvent[] = [];
    let nl: number;
    while ((nl = this.partial.indexOf("\n")) >= 0) {
      const line = this.partial.slice(0, nl).trim();
      this.partial = this.partial.slice(nl + 1);
      if (!line) continue;
      let v: any;
      try {
        v = JSON.parse(line);
      } catch {
        continue;
      }
      this.record(v, out);
    }
    return out;
  }

  private record(v: any, out: TrackEvent[]): void {
    switch (v?.type) {
      case "user":
        this.user(v, out);
        break;
      case "assistant":
        this.assistant(v, out);
        break;
      case "attachment":
        if (v.attachment?.type === "queued_command" && typeof v.attachment?.prompt === "string") {
          // A completion notice can also arrive mid-turn as absorbed input.
          this.bgNotice(v.attachment.prompt, out);
          out.push({ type: "absorbed", text: v.attachment.prompt });
        }
        break;
      case "queue-operation":
        if (v.operation === "enqueue" && typeof v.content === "string") {
          out.push({ type: "enqueued", text: v.content });
        }
        break;
      case "system":
        if (v.subtype === "turn_duration" && this.open) {
          this.endTurn(out);
        } else if (v.subtype === "compact_boundary") {
          // The session's context was replaced by a summary. An open turn
          // stays open and ends on its own turn_duration.
          const trigger = v.compactMetadata?.trigger;
          out.push({ type: "compacted", trigger: typeof trigger === "string" ? trigger : "unknown" });
        }
        break;
    }
  }

  private user(v: any, out: TrackEvent[]): void {
    // The compaction summary is a user record shown only in the transcript.
    if (v.isMeta === true || v.isCompactSummary === true || v.isVisibleInTranscriptOnly === true) return;
    const content = v.message?.content;
    let text: string;
    if (typeof content === "string") {
      text = content;
    } else if (Array.isArray(content)) {
      if (content.some((b: any) => b?.type === "tool_result")) {
        // Tool results are part of the running turn, not prompts. A failed
        // result of a background call means the command never started (the
        // permission classifier or the operator refused it).
        for (const b of content) {
          if (b?.type === "tool_result" && b.is_error === true && typeof b.tool_use_id === "string") {
            if (this.pendingBg.delete(b.tool_use_id)) out.push({ type: "bg_never_started", id: b.tool_use_id });
          }
        }
        return;
      }
      text = content
        .filter((b: any) => b?.type === "text" && typeof b.text === "string")
        .map((b: any) => b.text)
        .join("\n");
    } else {
      return;
    }
    if (!text.trim() || text.trimStart().startsWith("[Request interrupted")) return;
    if (localCommand(v, text)) return;
    const origin: string = typeof v.origin?.kind === "string" ? v.origin.kind : "unknown";
    if (origin === "task-notification") this.bgNotice(text, out);
    const startsTurn = !this.open;
    if (startsTurn) {
      this.open = true;
      this.lastMsgId = null;
      this.lastMsgText = "";
    }
    out.push({ type: "prompt", origin, text, starts_turn: startsTurn });
  }

  /** A `<task-notification>` naming a pending background tool call. */
  private bgNotice(text: string, out: TrackEvent[]): void {
    if (!text.includes("<task-notification>")) return;
    const id = between(text, "<tool-use-id>", "</tool-use-id>");
    if (id && this.pendingBg.delete(id)) out.push({ type: "bg_finished", id });
  }

  private assistant(v: any, out: TrackEvent[]): void {
    const msg = v.message;
    if (!msg) return;
    const id: string = typeof msg.id === "string" ? msg.id : "";
    const stop: string = typeof msg.stop_reason === "string" ? msg.stop_reason : "";
    if (this.lastMsgId !== id) {
      this.lastMsgId = id;
      this.lastMsgText = "";
    }
    if (!Array.isArray(msg.content)) return;
    let text = "";
    for (const b of msg.content) {
      if (b?.type === "text" && typeof b.text === "string") {
        text += b.text;
      } else if (b?.type === "tool_use" && b.input?.run_in_background === true) {
        if (typeof b.id === "string" && !this.pendingBg.has(b.id)) {
          this.pendingBg.add(b.id);
          out.push({ type: "bg_started", id: b.id, description: bgDescription(b.input) });
        }
      }
    }
    if (!text.trim()) return;
    if (this.lastMsgText) this.lastMsgText += "\n";
    this.lastMsgText += text.trim();
    if (stop === "tool_use" && this.open) out.push({ type: "progress", text: text.trim() });
  }

  private endTurn(out: TrackEvent[]): void {
    const finalText = this.lastMsgText.trim() ? this.lastMsgText.trim() : null;
    this.open = false;
    this.lastMsgId = null;
    this.lastMsgText = "";
    out.push({ type: "turn_end", final_text: finalText, pending_bg: this.pendingBg.size });
  }
}

/** Text of the records a slash command typed into the session leaves
 *  (`/compact`, `/model`): the command's name and arguments and a local
 *  command's output. A local command writes no turn_duration, so a turn
 *  these records opened would never end. */
const LOCAL_COMMAND_PREFIXES = ["<command-name>", "<command-message>", "<command-args>", "<local-command-"];

/** A slash command line as the harness records it: one line, no `origin`. */
const SLASH_COMMAND_LINE = /^\/[A-Za-z][\w:-]*( [^\n]*)?$/;

function localCommand(v: any, text: string): boolean {
  const t = text.trim();
  if (LOCAL_COMMAND_PREFIXES.some((p) => t.startsWith(p))) return true;
  return v.origin === undefined && SLASH_COMMAND_LINE.test(t);
}

/** Longest description kept for a background command, in characters. */
const BG_DESCRIPTION_CHARS = 120;

/** What a background tool call says it runs: the first line of its
 *  `description`, else the first line of its `command`, cut to
 *  BG_DESCRIPTION_CHARS characters; null when it has neither. */
function bgDescription(input: any): string | null {
  const firstLine = (s: unknown) => (typeof s === "string" ? s.trim().split("\n")[0].trim() : "");
  const d = firstLine(input?.description) || firstLine(input?.command);
  if (!d) return null;
  const chars = Array.from(d);
  return chars.length > BG_DESCRIPTION_CHARS ? `${chars.slice(0, BG_DESCRIPTION_CHARS).join("")}…` : d;
}

/** Turns that ended after the last compaction of a whole transcript (after
 *  its start when it was never compacted). The daily compaction skips a
 *  session with fewer than its threshold. Pure. */
export function turnsSinceCompaction(transcript: string): number {
  const t = new TurnTracker();
  let n = 0;
  for (const ev of t.feed(transcript.endsWith("\n") ? transcript : `${transcript}\n`)) {
    if (ev.type === "compacted") n = 0;
    else if (ev.type === "turn_end") n++;
  }
  return n;
}

function between(s: string, open: string, close: string): string | null {
  const a = s.indexOf(open);
  if (a < 0) return null;
  const start = a + open.length;
  const b = s.indexOf(close, start);
  if (b < 0) return null;
  return s.slice(start, b).trim();
}
