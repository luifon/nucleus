// Transcript turn tracker (ADR-033). Line-for-line mirror of
// core/src/turn_tracker.rs; both run core/testdata/turn_tracker_vectors.json.
//
// Reads the Claude Code transcript JSONL incrementally and reports: a prompt
// was accepted, input typed during a running turn was absorbed, the model
// wrote text before a tool call (progress), a background command started or
// finished, and a turn ended (`system` record with subtype `turn_duration`).
// The chat engine delivers each turn's final text exactly once from these
// events, instead of returning the first text followed by a quiet
// transcript.

export type TrackEvent =
  | { type: "prompt"; origin: string; text: string; starts_turn: boolean }
  | { type: "absorbed"; text: string }
  | { type: "enqueued"; text: string }
  | { type: "progress"; text: string }
  | { type: "bg_started"; id: string }
  | { type: "bg_finished"; id: string }
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
        if (v.subtype === "turn_duration" && this.open) this.endTurn(out);
        break;
    }
  }

  private user(v: any, out: TrackEvent[]): void {
    if (v.isMeta === true) return;
    const content = v.message?.content;
    let text: string;
    if (typeof content === "string") {
      text = content;
    } else if (Array.isArray(content)) {
      if (content.some((b: any) => b?.type === "tool_result")) return;
      text = content
        .filter((b: any) => b?.type === "text" && typeof b.text === "string")
        .map((b: any) => b.text)
        .join("\n");
    } else {
      return;
    }
    if (!text.trim() || text.trimStart().startsWith("[Request interrupted")) return;
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
          out.push({ type: "bg_started", id: b.id });
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

function between(s: string, open: string, close: string): string | null {
  const a = s.indexOf(open);
  if (a < 0) return null;
  const start = a + open.length;
  const b = s.indexOf(close, start);
  if (b < 0) return null;
  return s.slice(start, b).trim();
}
