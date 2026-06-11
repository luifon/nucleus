import { test } from "node:test";
import assert from "node:assert/strict";
import { formatReply } from "./format.js";

test("bolds each non-empty line individually and signs with the persona", () => {
  const out = formatReply("first line\nsecond line", "Alfred");
  assert.equal(out, "*first line*\n*second line*\n\n*— Alfred*");
});

test("blank lines pass through unbolded", () => {
  const out = formatReply("top\n\nbottom", "Alfred");
  assert.equal(out, "*top*\n\n*bottom*\n\n*— Alfred*");
});

test("whitespace-only lines are not bolded", () => {
  const out = formatReply("a\n   \nb", "Q");
  assert.equal(out, "*a*\n   \n*b*\n\n*— Q*");
});
