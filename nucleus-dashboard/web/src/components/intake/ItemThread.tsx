import { useLayoutEffect, useMemo, useRef, useState } from "react";
import { Send } from "lucide-react";
import Markdown from "@/components/Markdown";
import CanvasBlock, { CanvasFallback } from "@/components/chat/CanvasBlock";
import {
  replyToItem,
  type IntakeHiddenFinding,
  type IntakeItem,
  type IntakeMessage,
  type IntakeQuestion,
  type IntakeReplyResult,
} from "@/lib/api";
import { parseMessage, parseResponses, type CanvasBlockData } from "@/lib/canvas";
import {
  boardFor,
  boardKey,
  canvasAnswerText,
  composerHint,
  composerPlaceholder,
  enterSends,
  isTerminal,
  sendReply,
  threadAnsweredIds,
  threadBlocks,
  threadOrder,
} from "@/lib/intake";
import { shortTime } from "@/lib/tasks";
import DecisionBoard, { type BoardActions } from "./DecisionBoard";

// The item's conversation, in the ChatPage pattern: the agent's replies
// are content (Markdown, full width, with its canvas questions as
// widgets), the operator's messages are stamps (right, amber border) that
// name where each came from, Nucleus notes are small and muted. Newest at
// the bottom; the list scrolls to the end when it opens and when a message
// arrives while the operator is at the end.
//
// Below the messages: the decision board while the item waits for the
// operator (or while an agent works: its status and "Write a message"),
// the composer otherwise or after "Continue discussing", nothing once the
// item is finished.

/** Distance from the bottom (px) within which new messages keep the view
 *  pinned to the end. */
const PIN_SLACK = 80;

export default function ItemThread({
  item,
  messages,
  visible,
  onSent,
  question = null,
  findings = [],
  post = replyToItem,
  boardActions,
  initialMode = "board",
}: {
  item: IntakeItem;
  messages: readonly IntakeMessage[];
  /** Whether the phone layout shows this pane; a change re-checks the
   *  scroll position (a hidden pane cannot be scrolled). */
  visible: boolean;
  onSent: (item: IntakeItem) => void;
  /** The page's open confirmation question. */
  question?: IntakeQuestion | null;
  /** A held item's hidden-content findings, shown on the board. */
  findings?: readonly IntakeHiddenFinding[];
  /** The reply call (the composer and canvas answers); tests pass a fake. */
  post?: (id: number, text: string) => Promise<IntakeReplyResult>;
  boardActions?: BoardActions;
  /** Whether the board or the composer shows first (tests). */
  initialMode?: "board" | "composer";
}) {
  const ordered = threadOrder(messages);
  const scrollRef = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);
  const lastId = ordered.length > 0 ? ordered[ordered.length - 1].id : 0;
  const answered = useMemo(() => threadAnsweredIds(ordered), [ordered]);
  const blocks = useMemo(() => threadBlocks(ordered), [ordered]);
  const [canvasBusy, setCanvasBusy] = useState(false);
  const [canvasError, setCanvasError] = useState<string | null>(null);

  // "board" shows the decision board, "composer" the text box. The board
  // comes back whenever what it offers changes (a new stage, plan or hold,
  // a new question).
  const [mode, setMode] = useState<"board" | "composer">(initialMode);
  const resetKey = `${boardKey(item)}|${question?.id ?? ""}`;
  const [seenKey, setSeenKey] = useState(resetKey);
  if (seenKey !== resetKey) {
    setSeenKey(resetKey);
    setMode("board");
  }

  const answerCanvas = async (text: string) => {
    setCanvasBusy(true);
    setCanvasError(null);
    const r = await sendReply(item.id, text, post);
    setCanvasBusy(false);
    if (r.kind === "sent") onSent(r.item);
    else setCanvasError(r.message);
  };

  const board = boardFor(item);
  let bottom: React.ReactNode;
  if (board.kind === "closed") {
    bottom = (
      <div className="shrink-0 border-t border-[var(--color-nucleus-border)] px-4 py-2.5 text-xs text-[var(--color-nucleus-faint)] md:px-5">
        item {item.stage}: the conversation is closed
      </div>
    );
  } else if (question || (board.kind === "board" && mode === "board")) {
    bottom = (
      <DecisionBoard
        item={item}
        question={question}
        findings={findings}
        onWrite={() => setMode("composer")}
        onChange={onSent}
        actions={boardActions}
      />
    );
  } else {
    bottom = (
      <Composer
        item={item}
        onSent={onSent}
        post={post}
        onBack={board.kind === "board" ? () => setMode("board") : undefined}
        autoFocus={board.kind === "board"}
      />
    );
  }

  // A hidden pane has no height; the effect runs again when it is shown.
  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (el && el.clientHeight > 0 && pinned.current) el.scrollTop = el.scrollHeight;
  }, [lastId, visible]);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div
        ref={scrollRef}
        onScroll={(e) => {
          const el = e.currentTarget;
          pinned.current = el.scrollHeight - el.scrollTop - el.clientHeight < PIN_SLACK;
        }}
        className="min-h-0 flex-1 overflow-y-auto px-4 py-4 md:px-5"
      >
        {ordered.length === 0 ? (
          <div className="text-xs text-[var(--color-nucleus-faint)]">no messages yet</div>
        ) : (
          <ul className="space-y-4">
            {ordered.map((m) => (
              <li key={m.id}>
                <ThreadMessage
                  message={m}
                  answered={answered}
                  blocks={blocks}
                  canvasDisabled={canvasBusy || isTerminal(item.stage)}
                  onCanvasSubmit={(text) => void answerCanvas(text)}
                />
              </li>
            ))}
          </ul>
        )}
        {canvasError && (
          <div role="alert" className="mt-3 text-xs text-[var(--color-status-down)]">
            {canvasError}
          </div>
        )}
      </div>
      {bottom}
    </div>
  );
}

/** `whatsapp-session` is the WhatsApp DM session relaying a message. */
function viaLabel(via: IntakeMessage["via"]): string {
  return via === "whatsapp-session" ? "whatsapp" : via;
}

const NO_IDS: ReadonlySet<string> = new Set();
const NO_BLOCKS: ReadonlyMap<string, CanvasBlockData> = new Map();

export function ThreadMessage({
  message: m,
  answered = NO_IDS,
  blocks = NO_BLOCKS,
  canvasDisabled = true,
  onCanvasSubmit = () => {},
}: {
  message: IntakeMessage;
  /** Ids of the canvas blocks a later operator message answered. */
  answered?: ReadonlySet<string>;
  /** The agent's canvas blocks by id, to name the options an answer chose. */
  blocks?: ReadonlyMap<string, CanvasBlockData>;
  /** A canvas answer is being sent, or the item is finished. */
  canvasDisabled?: boolean;
  /** Posts a canvas block's response as the operator's reply. */
  onCanvasSubmit?: (text: string) => void;
}) {
  if (m.author === "operator") {
    const responses = m.body.includes("<canvas-response") ? parseResponses(m.body) : [];
    return (
      <div className="flex justify-end" data-author="operator">
        <div className="max-w-[88%] rounded border border-[var(--color-nucleus-accent)] bg-[color-mix(in_srgb,var(--color-nucleus-accent)_8%,var(--color-nucleus-surface))] px-3 py-2 md:max-w-[80%]">
          <div className="mb-1 flex flex-wrap items-center gap-x-2 text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)]">
            <span>you</span>
            <span>·</span>
            <span className="text-[var(--color-nucleus-accent)]">{viaLabel(m.via)}</span>
            <span>·</span>
            <span title={m.at}>{shortTime(m.at)}</span>
          </div>
          {responses.length > 0 ? (
            // A click on an agent's canvas question: the choice, not the markup.
            <div className="flex flex-col gap-1 text-sm text-[var(--color-nucleus-text)]" data-canvas-response>
              {responses.map((r, i) => (
                <span key={i} className="break-words">
                  ✔ {canvasAnswerText(r, blocks.get(r.id))}
                </span>
              ))}
            </div>
          ) : (
            <div className="whitespace-pre-wrap break-words text-sm text-[var(--color-nucleus-text)]">{m.body}</div>
          )}
          {m.pending_agent === 1 && <div className="mt-1 text-[10px] text-[var(--color-nucleus-faint)]">not read by the agent yet</div>}
        </div>
      </div>
    );
  }
  if (m.author === "agent") {
    return (
      <div data-author="agent">
        <div className="mb-1 flex items-center gap-2 text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)] opacity-70">
          <span className="text-[var(--color-nucleus-accent)]">▸ agent</span>
          <span>·</span>
          <span title={m.at}>{shortTime(m.at)}</span>
        </div>
        {m.body.includes("<canvas") ? (
          // The agent's questions as canvas blocks (ADR-012). Its labels
          // are model text and render as plain text; a choice posts back
          // as a discussion reply, never as a decision.
          parseMessage(m.body).map((seg, i) =>
            seg.kind === "text" ? (
              <Markdown key={i} source={seg.text} />
            ) : seg.kind === "canvas-fallback" ? (
              <CanvasFallback key={i} reason={seg.reason} raw={seg.raw} />
            ) : (
              <CanvasBlock
                key={seg.block.id}
                block={seg.block}
                answered={answered.has(seg.block.id)}
                disabled={canvasDisabled}
                onSubmit={onCanvasSubmit}
              />
            ),
          )
        ) : (
          <Markdown source={m.body} />
        )}
      </div>
    );
  }
  return (
    <div data-author="nucleus" className="border-l border-[var(--color-nucleus-border)] pl-2.5 text-[11px] leading-relaxed text-[var(--color-nucleus-faint)]">
      <span className="uppercase tracking-widest opacity-70">nucleus · </span>
      <span title={m.at}>{shortTime(m.at)}</span>
      <div className="whitespace-pre-wrap break-words">{m.body}</div>
    </div>
  );
}

/** A touch keyboard has no Shift+Enter; there Enter adds a line and the
 *  send button sends. */
function coarsePointer(): boolean {
  return typeof window !== "undefined" && typeof window.matchMedia === "function" && window.matchMedia("(pointer: coarse)").matches;
}

export function Composer({
  item,
  onSent,
  post = replyToItem,
  initialNotice = null,
  onBack,
  autoFocus = false,
}: {
  item: IntakeItem;
  onSent: (item: IntakeItem) => void;
  /** The reply call; tests pass a fake. */
  post?: (id: number, text: string) => Promise<IntakeReplyResult>;
  /** A notice to show from the start (tests). */
  initialNotice?: string | null;
  /** Shows "Back to options", which returns to the decision board. */
  onBack?: () => void;
  /** Focus the text box when it appears (opened from the board). */
  autoFocus?: boolean;
}) {
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const [notice, setNotice] = useState<string | null>(initialNotice);
  const [error, setError] = useState<string | null>(null);
  const [touch] = useState(coarsePointer);
  const hint = composerHint(item.stage);

  const send = async () => {
    const text = draft.trim();
    if (!text || sending) return;
    setSending(true);
    setNotice(null);
    setError(null);
    const r = await sendReply(item.id, text, post);
    setSending(false);
    if (r.kind === "sent") {
      // `note` says what Nucleus did with it, or null when the refinement
      // agent reads it.
      setDraft("");
      setNotice(r.note);
      onSent(r.item);
    } else {
      // The draft stays so the operator can send it again.
      setError(r.message);
    }
  };

  return (
    <div className="shrink-0 border-t border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] p-3">
      {onBack && (
        <div className="mb-2 flex justify-end">
          <button type="button" onClick={onBack} className="text-xs text-[var(--color-nucleus-accent)] hover:underline">
            Back to options
          </button>
        </div>
      )}
      {sending && (
        <div role="status" className="mb-2 text-xs text-[var(--color-nucleus-faint)]">
          Nucleus is reading your message…
        </div>
      )}
      {notice && !sending && (
        <div role="status" className="mb-2 max-h-40 overflow-y-auto whitespace-pre-wrap text-xs text-[var(--color-status-warn)]">
          {notice}
        </div>
      )}
      {error && (
        <div role="alert" className="mb-2 text-xs text-[var(--color-status-down)]">
          {error}
        </div>
      )}
      {hint && !notice && !sending && <div className="mb-2 text-[11px] text-[var(--color-nucleus-faint)]">{hint}</div>}
      <form
        className="flex items-end gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          void send();
        }}
      >
        <textarea
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (enterSends({ key: e.key, shiftKey: e.shiftKey, isComposing: e.nativeEvent.isComposing }, touch)) {
              e.preventDefault();
              void send();
            }
          }}
          rows={2}
          autoFocus={autoFocus}
          aria-label="reply to the refinement agent"
          placeholder={composerPlaceholder(touch)}
          disabled={sending}
          // 16px below md: iOS zooms the page into smaller text fields.
          className="min-h-[2.75rem] flex-1 resize-y rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-3 py-2 text-base md:text-sm text-[var(--color-nucleus-text)] focus:border-[var(--color-nucleus-accent)] focus:outline-none disabled:opacity-50"
        />
        <button
          type="submit"
          disabled={sending || !draft.trim()}
          className="flex items-center gap-1.5 rounded border border-[var(--color-nucleus-accent)] bg-[color-mix(in_srgb,var(--color-nucleus-accent)_12%,transparent)] px-3 py-2 text-sm text-[var(--color-nucleus-accent)] hover:bg-[color-mix(in_srgb,var(--color-nucleus-accent)_22%,transparent)] disabled:cursor-not-allowed disabled:opacity-40"
        >
          <Send size={12} strokeWidth={1.75} />
          {sending ? "sending…" : "send"}
        </button>
      </form>
    </div>
  );
}
