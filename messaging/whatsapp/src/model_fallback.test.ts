import { test } from "node:test";
import assert from "node:assert/strict";
import { paneShowsModelError } from "./claude_session.js";

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
