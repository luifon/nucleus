import { test } from "node:test";
import assert from "node:assert/strict";
import { redact } from "./diary.js";

test("redact replaces JIDs, emails, phones and home dirs", () => {
  assert.equal(redact("Connected as 5511999999999:2@s.whatsapp.net"), "Connected as <jid>");
  assert.equal(redact("group 5511999999999-1234567890@g.us and 5511999999999@lid"), "group <jid> and <jid>");
  assert.equal(redact("mail someone@example.com now"), "mail <email> now");
  assert.equal(redact("call +5511999999999 or 5511999999999"), "call <phone> or <phone>");
  assert.equal(redact("/Users/someone/path/to/x"), "~/path/to/x");
  assert.equal(redact("Call +5511999999999."), "Call <phone>.");
  assert.equal(redact("Home (/Users/someone)."), "Home (~).");
  assert.equal(redact("v1.2.3456789012 stays"), "v1.2.3456789012 stays");
});

test("redact leaves dates, reminder ids, snowflakes and counts alone", () => {
  const line = "2026-09-07 12:00 reminder #50, msg 1546638944463097886, queue#513 sent in 1.2s (937c)";
  assert.equal(redact(line), line);
});
