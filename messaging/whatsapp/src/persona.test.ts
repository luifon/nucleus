import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { resolvePersona } from "./persona.js";

function tempWorkspace(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-persona-"));
  fs.mkdirSync(path.join(dir, "personas"));
  return dir;
}

function withClean<T>(envKeys: string[], fn: () => T): T {
  const saved: Record<string, string | undefined> = {};
  for (const k of envKeys) {
    saved[k] = process.env[k];
    delete process.env[k];
  }
  try {
    return fn();
  } finally {
    for (const k of envKeys) {
      if (saved[k] === undefined) delete process.env[k];
      else process.env[k] = saved[k];
    }
  }
}

test("resolves frontmatter display_name and substitutes ${USER_NAME}", () => {
  const dir = tempWorkspace();
  fs.writeFileSync(
    path.join(dir, "personas/robot.md"),
    "---\ndisplay_name: ROBOT\n---\n\nHello ${USER_NAME}.\n",
  );
  withClean(["NUCLEUS_PERSONA_DISCORD"], () => {
    process.env.NUCLEUS_PERSONA_DISCORD = "robot";
    const p = resolvePersona(dir, "Alice", "discord");
    assert.equal(p.displayName, "ROBOT");
    assert.equal(p.body.trim(), "Hello Alice.");
  });
});

test("falls back to slug when no frontmatter", () => {
  const dir = tempWorkspace();
  fs.writeFileSync(
    path.join(dir, "personas/assistant.md"),
    "Just a body, no frontmatter.\n",
  );
  withClean(["NUCLEUS_PERSONA_WHATSAPP"], () => {
    process.env.NUCLEUS_PERSONA_WHATSAPP = "assistant";
    const p = resolvePersona(dir, "Alice", "whatsapp");
    assert.equal(p.displayName, "assistant");
    assert.equal(p.body.trim(), "Just a body, no frontmatter.");
  });
});

test("errors when env var missing", () => {
  const dir = tempWorkspace();
  withClean(["NUCLEUS_PERSONA_GMAIL"], () => {
    assert.throws(
      () => resolvePersona(dir, "Alice", "gmail"),
      /NUCLEUS_PERSONA_GMAIL/,
    );
  });
});

test("errors when persona file missing", () => {
  const dir = tempWorkspace();
  withClean(["NUCLEUS_PERSONA_DISCORD"], () => {
    process.env.NUCLEUS_PERSONA_DISCORD = "ghost";
    assert.throws(
      () => resolvePersona(dir, "Alice", "discord"),
      /ghost/,
    );
  });
});

test("context override wins over venue default", () => {
  const dir = tempWorkspace();
  fs.writeFileSync(path.join(dir, "personas/base.md"), "base body");
  fs.writeFileSync(path.join(dir, "personas/dm.md"), "dm body");
  withClean(["NUCLEUS_PERSONA_WHATSAPP", "NUCLEUS_PERSONA_WHATSAPP_DM"], () => {
    process.env.NUCLEUS_PERSONA_WHATSAPP = "base";
    process.env.NUCLEUS_PERSONA_WHATSAPP_DM = "dm";
    const p = resolvePersona(dir, "Alice", "whatsapp", "dm");
    assert.equal(p.body.trim(), "dm body");
    assert.equal(p.displayName, "dm");
  });
});

test("context falls back to venue default when override unset", () => {
  const dir = tempWorkspace();
  fs.writeFileSync(path.join(dir, "personas/base.md"), "base body");
  withClean(["NUCLEUS_PERSONA_WHATSAPP", "NUCLEUS_PERSONA_WHATSAPP_DM"], () => {
    process.env.NUCLEUS_PERSONA_WHATSAPP = "base";
    const p = resolvePersona(dir, "Alice", "whatsapp", "dm");
    assert.equal(p.body.trim(), "base body");
  });
});
