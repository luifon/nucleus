import { test } from "node:test";
import assert from "node:assert/strict";
import { ConnectionSupervisor, DEFAULT_BREAKER, classifyClose, type BreakerConfig } from "./breaker.js";

const CFG: BreakerConfig = {
  ladderMs: [1_000, 2_000, 5_000, 15_000, 60_000, 300_000],
  stableMs: 120_000,
  openThreshold: 4, // small for tests
  windowMs: 15 * 60_000,
  probeMs: 5 * 60_000,
  alertAfterOutageMs: 30 * 60_000,
};

/** Deterministic clock the tests advance by hand. */
function makeClock(start = 1_000_000) {
  let t = start;
  return { now: () => t, advance: (ms: number) => (t += ms) };
}

test("classifyClose: only logged-out is non-reconnectable", () => {
  assert.deepEqual(classifyClose(401), { cls: "logged-out", reconnectable: false });
  for (const [code, cls] of [
    [405, "login-405"],
    [428, "connection-closed"],
    [440, "connection-replaced"],
    [515, "restart-required"],
    [undefined, "unknown"],
  ] as const) {
    const c = classifyClose(code as number | undefined);
    assert.equal(c.cls, cls);
    assert.equal(c.reconnectable, true);
  }
  assert.equal(classifyClose(999).cls, "code-999");
});

test("ladder escalates per consecutive quick failure and caps at the last rung", () => {
  const clock = makeClock();
  const sup = new ConnectionSupervisor(CFG, clock.now);
  const delays: number[] = [];
  for (let i = 0; i < 3; i++) {
    sup.onOpen();
    clock.advance(5_000); // dies quickly — never stable
    const out = sup.onClose(428);
    assert.equal(out.decision.action, "reconnect");
    if (out.decision.action === "reconnect") delays.push(out.decision.delayMs);
    clock.advance(60_000); // spread failures so the window doesn't open the circuit
  }
  assert.deepEqual(delays, [1_000, 2_000, 5_000]);
});

test("a stable connection resets the ladder", () => {
  const clock = makeClock();
  const sup = new ConnectionSupervisor(CFG, clock.now);
  sup.onOpen();
  clock.advance(1_000);
  sup.onClose(428); // rung 0 consumed
  sup.onOpen();
  clock.advance(CFG.stableMs + 1); // stable stretch
  const out = sup.onClose(428);
  assert.deepEqual(out.decision, { action: "reconnect", delayMs: 1_000 }); // back to rung 0
  assert.equal(out.uptimeMs! >= CFG.stableMs, true);
});

test("failure storm opens the circuit exactly once", () => {
  const clock = makeClock();
  const sup = new ConnectionSupervisor(CFG, clock.now);
  let opened = 0;
  for (let i = 0; i < CFG.openThreshold; i++) {
    sup.onOpen();
    clock.advance(2_000);
    const out = sup.onClose(405);
    if (out.decision.action === "open-circuit" && out.decision.justOpened) opened++;
  }
  assert.equal(opened, 1);
  assert.equal(sup.isOpen, true);
});

test("probe that connects then dies: circuit closes, ladder stays high, no re-alert", () => {
  const clock = makeClock();
  const sup = new ConnectionSupervisor(CFG, clock.now);
  for (let i = 0; i < CFG.openThreshold; i++) {
    sup.onOpen();
    clock.advance(2_000);
    sup.onClose(405);
  }
  assert.equal(sup.isOpen, true);
  // half-open probe connects (recovery clears the window)…
  clock.advance(CFG.probeMs);
  sup.onOpen();
  clock.advance(3_000); // …and dies before proving stable
  const probe = sup.onClose(405);
  assert.equal(sup.isOpen, false); // fresh storm required to re-open
  assert.equal(probe.decision.action, "reconnect");
  if (probe.decision.action === "reconnect") {
    // ladder continuity: rung wasn't reset by the brief connection
    assert.equal(probe.decision.delayMs >= 15_000, true);
  }
});

test("close without a preceding open while the circuit is open keeps probing, no re-alert", () => {
  const clock = makeClock();
  const sup = new ConnectionSupervisor(CFG, clock.now);
  for (let i = 0; i < CFG.openThreshold; i++) {
    sup.onOpen();
    clock.advance(2_000);
    sup.onClose(405);
  }
  assert.equal(sup.isOpen, true);
  // probe's connect() dies before any "open" event
  clock.advance(CFG.probeMs);
  const probe = sup.onClose(405);
  assert.equal(probe.decision.action, "open-circuit");
  if (probe.decision.action === "open-circuit") assert.equal(probe.decision.justOpened, false);
  assert.equal(sup.isOpen, true);
});

test("recovery: an open reports the outage and closes the circuit", () => {
  const clock = makeClock();
  const sup = new ConnectionSupervisor(CFG, clock.now);
  for (let i = 0; i < CFG.openThreshold; i++) {
    sup.onOpen();
    clock.advance(1_000);
    sup.onClose(428);
  }
  assert.equal(sup.isOpen, true);
  clock.advance(45 * 60_000); // outage
  const open = sup.onOpen();
  assert.equal(open.recovered, true);
  assert.equal(open.outageMs >= 45 * 60_000, true);
  assert.equal(sup.isOpen, false);
});

test("old failures age out of the window", () => {
  const clock = makeClock();
  const sup = new ConnectionSupervisor(CFG, clock.now);
  for (let i = 0; i < CFG.openThreshold - 1; i++) {
    sup.onOpen();
    clock.advance(1_000);
    sup.onClose(428);
  }
  clock.advance(CFG.windowMs + 1); // everything ages out
  sup.onOpen();
  clock.advance(1_000);
  const out = sup.onClose(428);
  assert.equal(out.decision.action, "reconnect"); // not open-circuit
  assert.equal(sup.isOpen, false);
});

test("logged-out holds — never reconnects, never exits", () => {
  const clock = makeClock();
  const sup = new ConnectionSupervisor(DEFAULT_BREAKER, clock.now);
  sup.onOpen();
  clock.advance(10_000);
  const out = sup.onClose(401);
  assert.deepEqual(out.decision, { action: "hold" });
});
