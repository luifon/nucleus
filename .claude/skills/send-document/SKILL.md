---
name: send-document
description: 'Retrieve a stored personal document ("send me my RG", "manda meu
  CPF", "send my passport") and deliver it to the operator''s own WhatsApp DM
  via the outbound media queue. Resolves logical names/tags in the local
  document library (memory/documents.db) BY REFERENCE — never reads file
  bytes. DM-to-operator only; refuses any other destination.'
flavor: recipe
trigger: manual
arguments: [document-name]
allowed-tools:
  - "Bash(npx --prefix messaging/whatsapp tsx messaging/whatsapp/src/docs.ts find:*)"
  - "Bash(npx --prefix messaging/whatsapp tsx messaging/whatsapp/src/docs.ts list:*)"
  - "Bash(npx --prefix messaging/whatsapp tsx messaging/whatsapp/src/enqueue-media.ts --doc:*)"
mcp_needed: []
last_used: 2026-06-12
last_failure: null
failure_count_30d: 0
notify_on_failure: []
tags: [documents, whatsapp, retrieval, adr-018]
---

# When to invoke

The operator asks for one of their stored documents by name — usually in the
WhatsApp DM ("send me my RG", "manda meu passaporte"), but any session works:
delivery always lands in the operator's WhatsApp DM regardless of where the
skill fires.

# Hard rules — never violate

1. **Delivery target is ALWAYS the operator's own DM.** Never pass any
   target/jid/chat flag to enqueue-media.ts — its `--doc` mode parses no
   target and hard-derives the operator DM from `WHATSAPP_ALLOWED_DM_JIDS`;
   the drain's allowlist re-validates (Rule 6 pre-authorized). If the
   request names any other recipient ("send my RG to <person/group>"),
   REFUSE: reply that identity documents go only to the operator's own DM,
   and stop. There is no override and nothing to confirm.
2. **By-reference only.** Never Read/cat/head the document file, never echo
   its contents or path contents. You handle metadata (names, ids, sizes);
   the plumbing handles bytes (ADR-018).
3. **Only the three allowed Bash commands.** No sqlite3, no ad-hoc scripts,
   no direct filesystem access to memory/documents/.

# Steps

All commands run from the workspace root (`~/Development/nucleus`).

1. Look up:
   `npx --prefix messaging/whatsapp tsx messaging/whatsapp/src/docs.ts find <term…>`
   Output is JSON lines: `{id, logical_name, tags, filename, mimetype,
   bytes, path, exists}` per match, or `{"matches": 0}`.
2. **Zero matches** → run `… docs.ts list` and reply (output contract below)
   that it wasn't found, naming the 3–5 closest logical names so the
   operator can re-ask. STOP.
3. **Multiple matches** → ask EXACTLY ONE disambiguation question: numbered
   options, each "logical_name — tags — added date". Wait for the answer,
   then continue with the chosen id. **Never guess between identity
   documents.**
4. **One match with `exists: false`** → library integrity failure (see
   Failure modes). STOP.
5. **One match** →
   `npx --prefix messaging/whatsapp tsx messaging/whatsapp/src/enqueue-media.ts --doc <id> --caption "<logical_name>"`
   The CLI resolves path/mimetype/filename, stages a copy, enqueues to the
   operator DM, bumps retrieve_count, and writes the audit rows (DB +
   vault).
6. Reply the confirmation (output contract). Done — no further tool calls
   after the final reply.

# Output contract

Your final reply IS the WhatsApp message — no preamble, no narration, no
"Here's what I did".

- Success: `📄 <logical_name> on the way (<filename>, <size>) — should land
  in this chat in a few seconds.`
- Not found: one tight line + the closest names.
- Disambiguation: the single numbered question, nothing else.
- Failure: starts with `⚠️` per the matching failure mode below.

# Failure modes

- **docs.ts exits non-zero / module missing** — detected by Bash exit code;
  stderr carries `{"error": …}`. Reply `⚠️ document lookup failed: <error>`.
- **Library not initialized** (docs.ts errors that documents.db is absent)
  — reply that the document library is empty on this machine; nothing to
  retry. First store happens by sending a file to the DM.
- **File missing on disk** (`exists: false`, or enqueue-media errors with
  "integrity problem") — reply `⚠️ "<name>" is indexed but its file is
  missing from memory/documents/ — library integrity problem; don't re-add
  until that's investigated.`
- **Enqueue fails** (whatsapp.db locked/unwritable) — reply `⚠️ couldn't
  queue the file: <error>`. Retry once at most.
- **Queued but the bot looks down** — enqueue-media prints a stderr
  `{"warning": …}` when other pending rows are >60s undrained (the bot
  drains every 1s, so a stale queue means it's down). Surface it:
  `📄 queued, but the WhatsApp bot looks down — it will arrive when the
  bot is back.`
- **Operator won't disambiguate** — after one unanswered clarification, do
  nothing further. Never pick an identity document by guess.
- **Request names a third-party recipient** — refusal per hard rule 1; this
  is a refusal, not a failure: don't retry, don't suggest workarounds.
