import { test } from "node:test";
import assert from "node:assert/strict";
import {
  paneShowsModelError,
  replyIsModelError,
  MODEL_ERROR_REPLY_MAX_LEN,
} from "./claude_session.js";

test("paneShowsModelError catches the fable-5 boot banner + post-send error", () => {
  assert.ok(
    paneShowsModelError(
      "Claude Fable 5 is currently unavailable. Please use Opus 4.8 or another available model.",
    ),
  );
  assert.ok(
    paneShowsModelError(
      "There's an issue with the selected model (claude-fable-5). It may not exist or you may not have access to it.",
    ),
  );
});

test("paneShowsModelError ignores a normal ready pane", () => {
  assert.ok(
    !paneShowsModelError("❯ \n~/Development/nucleus | main | Opus 4.8\n⏵⏵ auto mode on"),
  );
});

test("replyIsModelError catches the banner that got posted as a deliverable", () => {
  // The exact text the 2026-08-24 heartbeat fire sent to the operator.
  assert.ok(
    replyIsModelError(
      "There's an issue with the selected model (claude-fable-5). It may not exist or you may not have access to it. Run /model to pick a different model.",
    ),
  );
  // Transcript whitespace must not hide it.
  assert.ok(
    replyIsModelError(
      "\n  Claude Fable 5 is currently unavailable. Please use Opus 4.8 or another available model.  \n",
    ),
  );
});

test("replyIsModelError leaves real answers alone", () => {
  assert.ok(!replyIsModelError("Switched the default to Opus. Nothing else pins a model."));
  // A long answer quoting the banner is content, not a failure.
  const quoting =
    'The fire failed because the pane said "There\'s an issue with the selected model ' +
    '(claude-fable-5). It may not exist or you may not have access to it." and the ' +
    "spawn-time check never saw it. " +
    "Detail follows. ".repeat(20);
  assert.ok(quoting.length > MODEL_ERROR_REPLY_MAX_LEN);
  assert.ok(!replyIsModelError(quoting));
});
