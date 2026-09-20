import { test } from "node:test";
import assert from "node:assert/strict";
import {
  paneShowsModelError,
  classifyInfraReply,
  INFRA_ERROR_REPLY_MAX_LEN,
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

test("classifyInfraReply catches the banners that got posted as deliverables", () => {
  // The exact text the 2026-08-24 13:01 fire sent to the operator.
  assert.equal(
    classifyInfraReply(
      "There's an issue with the selected model (claude-fable-5). It may not exist or you may not have access to it. Run /model to pick a different model.",
    ),
    "model-unavailable",
  );
  // Transcript whitespace must not hide it.
  assert.equal(
    classifyInfraReply(
      "\n  Claude Fable 5 is currently unavailable. Please use Opus 4.8 or another available model.  \n",
    ),
    "model-unavailable",
  );
  // The 09:03 fire, which then died in the outbound queue.
  assert.equal(
    classifyInfraReply(
      "API Error: Can't reach the API server — check your internet or DNS (ENOTFOUND)",
    ),
    "api",
  );
  assert.equal(classifyInfraReply("API Error: 529 Overloaded"), "api");
  // Expired credentials are fatal, never a retryable API error.
  assert.equal(classifyInfraReply("Not logged in \u00b7 Please run /login"), "not-logged-in");
  // The Max session-limit banner the heartbeat flooded WhatsApp with, 2026-09-19.
  assert.equal(
    classifyInfraReply("You've hit your session limit \u00b7 resets 8:50pm"),
    "usage-limit",
  );
  assert.equal(
    classifyInfraReply("Claude usage limit reached. Resets at 8:50pm."),
    "usage-limit",
  );
  // The phantom turn a still-limited respawned session returned.
  assert.equal(classifyInfraReply("No response requested."), "no-turn");
  assert.equal(classifyInfraReply("  No response requested  "), "no-turn");
});

test("classifyInfraReply leaves multi-line limit reports alone", () => {
  // A multi-line report ABOUT a limit keeps its newlines and must deliver.
  const report =
    "Heartbeat: the WhatsApp session hit its usage limit at 23:00.\n" +
    "It resets at 8:50pm; reminders will queue until then.";
  assert.equal(classifyInfraReply(report), null);
  // A single-line report quoting the phantom is content, not the phantom.
  assert.equal(
    classifyInfraReply(
      'The 23:30 fire came back with "No response requested." \u2014 still usage-limited.',
    ),
    null,
  );
});

test("classifyInfraReply leaves real answers alone", () => {
  assert.equal(
    classifyInfraReply("Switched the default to Opus. Nothing else pins a model."),
    null,
  );
  // A long answer quoting a banner is content, not a failure.
  const quoting =
    'The fire failed because the pane said "There\'s an issue with the selected model ' +
    '(claude-fable-5). It may not exist or you may not have access to it." and the ' +
    "spawn-time check never saw it. " +
    "Detail follows. ".repeat(30);
  assert.ok(quoting.length > INFRA_ERROR_REPLY_MAX_LEN);
  assert.equal(classifyInfraReply(quoting), null);
  const recovered =
    'The 09:03 fire hit "API Error: Can\'t reach the API server" and retried clean. ' +
    "Detail follows. ".repeat(30);
  assert.ok(recovered.length > INFRA_ERROR_REPLY_MAX_LEN);
  assert.equal(classifyInfraReply(recovered), null);
});
