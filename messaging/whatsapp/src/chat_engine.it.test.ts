// Chat engine integration tests against a REAL tmux + claude session
// (ADR-033). Opt-in: they spend model turns and take minutes.
//
//   NUCLEUS_IT=1 npx tsx --test src/chat_engine.it.test.ts
//
// Everything runs in a temporary workspace and the tmux session
// `nucleus-test-engine`, which is killed at the end. Nothing touches the
// operator's sessions or memory/.

import { test, after } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { ChatEngine, DEFAULT_TEXTS, type TurnsConfig } from "./chat_engine.js";
import { ChatSessionStore, OutboundQueueStore } from "./db.js";
import { TurnStore } from "./turn_store.js";

const ENABLED = process.env.NUCLEUS_IT === "1";
const TMUX = "nucleus-test-engine";
const CHAT = "5511999999999@s.whatsapp.net";

const CFG: TurnsConfig = {
  ackAfterMs: 10_000,
  progressIntervalMs: 8_000,
  progressMaxChars: 200,
  ceilingMs: 30 * 60_000,
  permissionStallMs: 120_000,
  texts: { ...DEFAULT_TEXTS, ack: "ACK" },
};

function setup() {
  const ws = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-it-")));
  fs.mkdirSync(path.join(ws, "memory"));
  const dbPath = path.join(ws, "memory/whatsapp.db");
  const store = new ChatSessionStore(dbPath);
  const outbound = new OutboundQueueStore(dbPath);
  const turns = new TurnStore(dbPath);
  const engine = new ChatEngine(
    {
      turns,
      outbox: outbound,
      sessions: store,
      cfg: CFG,
      format: (b) => b,
      outboundTarget: (c) => c,
      log: {
        info: (o, m) => console.log("[it]", m, JSON.stringify(o)),
        warn: (o, m) => console.log("[it] WARN", m, JSON.stringify(o)),
        error: (o, m) => console.log("[it] ERROR", m, JSON.stringify(o)),
      },
    },
    {
      dm: {
        name: "dm",
        workspaceRoot: ws,
        tmuxSession: TMUX,
        permissionMode: "auto",
        appendSystemPrompt:
          "You are a test assistant. Follow the instructions in each message exactly and keep replies short.",
        idleTimeoutMs: 60 * 60_000,
        taskScope: true,
        agentLabel: "whatsapp",
      },
    },
  );
  const timer = setInterval(() => void engine.tick(), 500);
  const sent = () => outbound.pending(200).map((r) => ({ body: r.body, source: r.source, quoted: r.quotedJson }));
  const until = async (what: string, cond: () => boolean, ms: number) => {
    const start = Date.now();
    while (!cond()) {
      if (Date.now() - start > ms) throw new Error(`timed out waiting for: ${what}\nsent: ${JSON.stringify(sent(), null, 1)}`);
      await new Promise((r) => setTimeout(r, 500));
    }
  };
  const transcript = () => {
    const sid = store.lookup(CHAT);
    if (!sid) return [];
    const p = path.join(os.homedir(), ".claude", "projects", ws.replace(/\//g, "-"), `${sid}.jsonl`);
    if (!fs.existsSync(p)) return [];
    return fs
      .readFileSync(p, "utf8")
      .split("\n")
      .filter(Boolean)
      .map((l) => JSON.parse(l));
  };
  const stop = async () => {
    clearInterval(timer);
    await engine.shutdown();
  };
  return { engine, turns, sent, until, transcript, stop, ws };
}

after(() => {
  if (ENABLED) spawnSync("tmux", ["kill-session", "-t", TMUX]);
});

const msg = (text: string, id: string) => ({
  chatId: CHAT,
  pool: "dm",
  text,
  inputKind: "text" as const,
  waMsgId: id,
  quotedJson: JSON.stringify({ key: { id, remoteJid: CHAT } }),
});

test(
  "real session: slow tool, mid-turn messages, one final reply, typed input",
  { skip: !ENABLED, timeout: 10 * 60_000 },
  async () => {
    const t = setup();
    try {
      t.engine.receive(
        msg(
          "Integration test. Step 1: write exactly one short sentence saying you are starting. " +
            'Step 2: run this Bash command exactly, in the foreground: python3 -c "import time; time.sleep(30)" ' +
            "Step 3: after it finishes, reply with the word DONE-A followed by every extra word I send you while the command runs.",
          "M1",
        ),
      );
      // Wait until the command is running (a turn is open with the message).
      await t.until(
        "first message consumed",
        () => t.transcript().some((r) => r.type === "assistant" && JSON.stringify(r).includes("time.sleep")),
        120_000,
      );
      t.engine.receive(msg("Extra word: BANANA", "M2"));
      await new Promise((r) => setTimeout(r, 1_500));
      t.engine.receive(msg("Extra word: CHERRY", "M3"));

      await t.until("final reply", () => t.sent().some((m) => m.source === "chat-reply"), 300_000);
      await new Promise((r) => setTimeout(r, 5_000));
      const sent = t.sent();
      console.log("[it] outbound:", JSON.stringify(sent, null, 1));
      const replies = sent.filter((m) => m.source === "chat-reply");
      assert.equal(replies.length, 1, "exactly one final reply");
      assert.match(replies[0].body, /DONE-A/);
      assert.match(replies[0].body, /BANANA/);
      assert.match(replies[0].body, /CHERRY/);
      assert.match(replies[0].quoted ?? "", /"M1"/, "the reply quotes the first message");
      assert.ok(sent.filter((m) => m.source === "chat-ack").length <= 1, "at most one ack");
      assert.equal(t.turns.unanswered(CHAT).length, 0);

      // The operator's words reached the model as typed input, not as a paste.
      const tr = t.transcript();
      const first = tr.find((r) => r.type === "user" && typeof r.message?.content === "string" && r.message.content.includes("Integration test."));
      assert.ok(first, "the first message is a plain user prompt");
      assert.equal(first.promptSource, "typed");
      assert.ok(!first.message.content.includes("<pasted_content"), "no pasted_content wrapper");
      const absorbed = tr.filter((r) => r.type === "attachment" && r.attachment?.type === "queued_command");
      console.log("[it] absorbed:", absorbed.map((r) => r.attachment.prompt.slice(-40)));
      assert.ok(
        absorbed.some((r) => r.attachment.prompt.includes("BANANA")) ||
          tr.some((r) => r.type === "user" && JSON.stringify(r.message).includes("BANANA")),
        "the mid-turn message reached the session",
      );
    } finally {
      await t.stop();
    }
  },
);

test(
  "real session: an injected context message is typed in its envelope and its reply is not sent",
  { skip: !ENABLED, timeout: 5 * 60_000 },
  async () => {
    const t = setup();
    try {
      let done: boolean | null = null;
      t.engine.injectContext(
        CHAT,
        "dm",
        {
          sender: "task:ab12cd34",
          enqueuedAt: "2026-09-24T12:00:00Z",
          body: "Background task ab12cd34 ended with status done.\nResult:\n42 rows.",
        },
        1,
        (ok) => (done = ok),
      );
      await t.until("context typed", () => done !== null, 180_000);
      assert.equal(done, true);
      await t.until(
        "context turn ended",
        () => t.transcript().some((r) => r.type === "system" && r.subtype === "turn_duration"),
        180_000,
      );
      await new Promise((r) => setTimeout(r, 3_000));
      assert.equal(t.sent().length, 0, "nothing sent to WhatsApp for a context turn");
      const ctx = t.transcript().find((r) => r.type === "user" && JSON.stringify(r.message).includes("agent-msg"));
      assert.ok(ctx);
      console.log("[it] context prompt shape:", JSON.stringify(ctx.message.content).slice(0, 160));
      // Typed like every Nucleus input, attributed by the envelope.
      assert.equal(ctx.promptSource, "typed");
      const content = typeof ctx.message.content === "string" ? ctx.message.content : JSON.stringify(ctx.message.content);
      assert.ok(!content.includes("<pasted_content"), "no pasted_content wrapper");
      assert.match(content, /\[agent-msg from:task:ab12cd34 at:2026-09-24T12:00:00Z hop:1\]/);
      assert.match(content, /not from the operator/);
      assert.match(content, /│ 42 rows\./);
    } finally {
      await t.stop();
    }
  },
);
