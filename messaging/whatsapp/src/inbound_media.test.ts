import { test } from "node:test";
import assert from "node:assert/strict";
import { classifyCaption } from "./caption.js";

test("name:/nome: prefix forces archive-only with that name", () => {
  assert.deepEqual(classifyCaption("name: RG Jane Doe"), { mode: "archive", name: "RG Jane Doe" });
  assert.deepEqual(classifyCaption("nome: CNH 2026"), { mode: "archive", name: "CNH 2026" });
});

test("act:/faz:/! prefix forces the act path", () => {
  assert.deepEqual(classifyCaption("act: summarize this"), {
    mode: "act",
    instruction: "summarize this",
  });
  assert.deepEqual(classifyCaption("faz: resume isso aqui"), {
    mode: "act",
    instruction: "resume isso aqui",
  });
  assert.deepEqual(classifyCaption("! what's the total?"), {
    mode: "act",
    instruction: "what's the total?",
  });
});

test("empty caption archives with derived naming", () => {
  assert.deepEqual(classifyCaption(""), { mode: "archive" });
  assert.deepEqual(classifyCaption("   "), { mode: "archive" });
});

test("short label without sentence punctuation is a name", () => {
  assert.deepEqual(classifyCaption("passport"), { mode: "archive", name: "passport" });
  assert.deepEqual(classifyCaption("contrato aluguel 2026"), {
    mode: "archive",
    name: "contrato aluguel 2026",
  });
});

test("instruction-shaped captions act", () => {
  assert.equal(classifyCaption("what is the total on this receipt?").mode, "act");
  assert.equal(classifyCaption("summarize this contract for me please today").mode, "act"); // >5 words
  assert.equal(classifyCaption("read this.").mode, "act"); // punctuation
});

test("priv: prefix archives without enrichment", () => {
  assert.deepEqual(classifyCaption("priv: RG novo"), {
    mode: "archive",
    name: "RG novo",
    noEnrich: true,
  });
  assert.deepEqual(classifyCaption("priv:"), { mode: "archive", noEnrich: true });
});

test("vault:/import: prefix routes to vault-import with optional name", () => {
  assert.deepEqual(classifyCaption("vault: contrato aluguel"), {
    mode: "vault-import",
    name: "contrato aluguel",
  });
  assert.deepEqual(classifyCaption("import:"), { mode: "vault-import" });
  assert.deepEqual(classifyCaption("vault: please import this entire document now ok?"), {
    mode: "vault-import",
  });
});
