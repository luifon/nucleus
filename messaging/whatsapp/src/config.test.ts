import { test } from "node:test";
import assert from "node:assert/strict";
import { parseToml, turnsConfig } from "./config.js";
import { DEFAULT_TEXTS } from "./texts.js";

test("parseToml nests dotted table names", () => {
  const t = parseToml(`
[whatsapp.breaker]
probe_ms = 1000

[whatsapp.turns]
ack_after_secs = 45

[whatsapp.texts]
ack = "⏳ on it…"

[whatsapp.texts.infra_reasons]
usage_limit = "quota used up"

[claude]
binary = "claude"
`);
  assert.equal(t.whatsapp.breaker.probe_ms, 1000);
  assert.equal(t.whatsapp.turns.ack_after_secs, 45);
  assert.equal(t.whatsapp.texts.ack, "⏳ on it…");
  assert.equal(t.whatsapp.texts.infra_reasons.usage_limit, "quota used up");
  assert.equal(t.claude.binary, "claude");
});

test("turnsConfig applies overrides and keeps defaults for the rest", () => {
  const c = turnsConfig(
    { ack_after_secs: 45, progress_interval_secs: -1 },
    { ack: "⏳ on it…", progress_prefix: "» ", unknown_key: "x", no_final: 3, infra_reasons: { usage_limit: "quota used up" } },
  );
  assert.equal(c.ackAfterMs, 45_000);
  assert.equal(c.texts.ack, "⏳ on it…");
  assert.equal(c.texts.progressPrefix, "» ");
  assert.equal(c.texts.noFinal, DEFAULT_TEXTS.noFinal, "a non-string value keeps the default");
  assert.equal(c.texts.infraReasons["usage-limit"], "quota used up");
  assert.equal(c.texts.infraReasons.api, DEFAULT_TEXTS.infraReasons.api);
  assert.equal(c.progressIntervalMs, 180_000, "invalid values fall back to the default");
  assert.equal(c.ceilingMs, 6 * 3_600_000);
  const d = turnsConfig({});
  assert.equal(d.texts.ack, "⏳ Working on it…");
  assert.equal(d.ackAfterMs, 30_000);
  assert.equal(d.progressMaxChars, 160);
});

test("every default text is English (no Portuguese left in code-owned texts)", () => {
  const all = JSON.stringify(DEFAULT_TEXTS);
  assert.doesNotMatch(all, /[ãõçáéíóúâêô]/i);
});
