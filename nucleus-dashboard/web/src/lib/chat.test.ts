import { describe, expect, test } from "vitest";
import type { ChatDetail } from "@/lib/api";
import { chatBusy, PENDING_ID, shownMessages } from "./chat";

const base: ChatDetail = {
  chat: { id: "c1", title: null, claude_session_id: null, created_at: "t0", last_active: "t0" },
  messages: [
    { id: 1, chat_id: "c1", role: "user", content: "q1", ts: "t1" },
    { id: 2, chat_id: "c1", role: "assistant", content: "a1", ts: "t2" },
  ],
  turn: null,
  titling: false,
};

describe("shownMessages", () => {
  test("a running turn shows its message after the stored ones", () => {
    const shown = shownMessages({ ...base, turn: { status: "running", message: "q2", ts: "t3" } });
    expect(shown.map((m) => m.content)).toEqual(["q1", "a1", "q2"]);
    expect(shown[2]).toMatchObject({ id: PENDING_ID, role: "user", chat_id: "c1", ts: "t3" });
  });

  test("a running turn whose rows are already stored is shown once", () => {
    const shown = shownMessages({
      ...base,
      messages: [...base.messages, { id: 3, chat_id: "c1", role: "user", content: "q2", ts: "t3" }],
      turn: { status: "running", message: "q2", ts: "t3" },
    });
    expect(shown.map((m) => m.id)).toEqual([1, 2, 3]);
  });

  test("a failed turn is not shown as a message", () => {
    const shown = shownMessages({
      ...base,
      turn: { status: "failed", message: "q2", ts: "t3", error: "e" },
    });
    expect(shown.map((m) => m.content)).toEqual(["q1", "a1"]);
  });

  test("no chat, no messages", () => {
    expect(shownMessages(null)).toEqual([]);
  });
});

describe("chatBusy", () => {
  test("polls while a turn runs or a title is generated", () => {
    expect(chatBusy({ ...base, turn: { status: "running", message: "q", ts: "t" } })).toBe(true);
    expect(chatBusy({ ...base, titling: true })).toBe(true);
  });

  test("stops when the turn is stored or failed", () => {
    expect(chatBusy(base)).toBe(false);
    expect(chatBusy({ ...base, turn: { status: "failed", message: "q", ts: "t", error: "e" } })).toBe(false);
    expect(chatBusy(null)).toBe(false);
  });
});
