# ADR-018 — WhatsApp media + personal document library (encrypted Drive)

**Status:** Proposed (revised 2026-06-01 — supersedes the vault-binding draft)
**Date:** 2026-06-01
**Related:** ADR-005b (WhatsApp DM mode), ADR-006 (reminders / `outbound_queue`),
ADR-007 (Gmail/Calendar via claude.ai MCP — trash account), ADR-008 (skills),
ADR-019 (image-generation gallery — a producer for the outbound media path)

## Context

WhatsApp is **text-only in both directions** today:

- **Inbound** (`messaging/whatsapp/src/index.ts`): `extractText()` reads only
  `conversation` / `extendedTextMessage` / image|video `caption`; everything else
  drops at `if (!text.trim()) return;`. Audio is the lone media type fetched
  (`downloadMediaMessage` → Whisper) because it collapses to text.
- **Outbound** (`outbound_queue` in `memory/whatsapp.db`, drained 1s): text-only
  schema; the drain calls `sock.sendMessage(jid, { text })`. Baileys *does*
  support `{ image }` / `{ document, mimetype, fileName }` — never called.

The original trigger was delivering generated images. That's now **subsumed and
sharpened**: the real goal is **WhatsApp as a working surface for documents**,
and the headline is **on-demand retrieval of personal documents** — "send me my
RG / CPF / passport" from the DM, and I deliver the file. Generated images
(ADR-019's gallery) become just *one producer* feeding the same outbound path.

**This revision changes the storage model.** The prior draft bound media into the
Obsidian vault via a new brain-dump `attach` op. That's dropped. Binary
docs/media now live in **encrypted Google Drive**; the vault keeps only a
**text manifest** (index + audit). Rationale below.

## Decision — three planes + two cross-cutting rules

- **Storage = encrypted Google Drive** (the trash account, `$NUCLEUS_GMAIL_ACCOUNT`),
  client-side encrypted. Not the vault.
- **Index/audit = Obsidian manifest** — text only: logical name, tags, Drive file
  id, mimetype, direction, timestamp. Never the bytes, never decrypted content.
- **Transport = WhatsApp** — inbound via `downloadMediaMessage`; outbound via the
  `outbound_queue` extended for media.

Cross-cutting:
- **By-reference orchestration** (privacy-critical).
- **Security model** (encryption + DM-lock + audit).

### Storage: encrypted Drive, not the vault

- Binary docs/media live in the trash account's Drive, **client-side encrypted**
  before upload. Operator's choice: it's the account Nucleus already has wired
  (ADR-007), and encryption means a **trash-account compromise yields only
  ciphertext**.
- **Encryption:** symmetric, client-side (tool TBD — `age` preferred). The key
  lives in the **macOS Keychain** (read on demand by the plumbing), with a backup
  copy in the operator's password manager. *Lost key = unrecoverable docs* — the
  backup is mandatory, not optional.
- **Threat model:** protects against Drive / Google-account compromise. Does NOT
  protect against a fully-compromised Mac — but the Mac already holds `.env`,
  every bot token, and the WhatsApp session, so that's already game-over;
  Keychain doesn't make it worse. (This is why we didn't pick passphrase-per-
  retrieval: it would either ride through WhatsApp as plaintext or kill the
  from-anywhere use case, for marginal gain against an already-lost Mac.)

### Index & audit: the Obsidian manifest

- A text-only manifest in the vault records every stored item: logical name
  (`passport`, `RG`, `CPF`, …) + tags, Drive file id, mimetype, direction
  (in/out), source, timestamp. **No payload, no decrypted text.**
- It serves double duty: the **lookup index** that resolves "send me my passport"
  → a Drive file id, and the **audit log** of every store + retrieval.
- Lives in its own area (e.g. `4-Areas/Documents/`), distinct from knowledge
  brain-dumps. Text knowledge still flows to the vault as before; this is just a
  registry of where the encrypted binaries are.

### By-reference orchestration (privacy-critical)

The model resolves *which* file (by name/tags from the manifest) — it never holds
the document bytes. The plumbing does: download the ciphertext from Drive →
decrypt locally to a temp path → hand that path to the outbound queue → Baileys
sends → wipe the temp. **The bytes flow Drive → disk → WhatsApp, never into the
model context or session transcripts.** Your passport never lands in an LLM log.
(Inbound is symmetric: the model only *sees* an inbound doc when you explicitly
ask it to act on one — see below; pure archival is by-reference too.)

### Inbound (operator → Nucleus)

`handleMessage` gains an `imageMessage`/`documentMessage` branch
(`downloadMediaMessage`, mirroring the audio path). Two outcomes by intent:

- **"act on this"** ("summarize this contract", "what's the total on this
  receipt") → stage to a transient path and **instruct the session to `Read`
  it** (Read renders images + PDF pages natively; robust through the tmux paste
  path, unlike `@`-mentions). Respond, then sweep the temp.
- **archive** → encrypt → upload to Drive → write a manifest entry. Per the scope
  decision, **all inbound media is archived** to encrypted Drive; the model only
  reads it when you ask it to act.

### Outbound (Nucleus → operator) — the headline

"send me my passport" → resolve via the manifest → fetch + decrypt → enqueue a
media row → the drain delivers to the DM. Mechanism = the extended
`outbound_queue` (below). **DM-locked:** only the operator's JID
(`resolveOutboundTarget` already gates the allowlist). Identity docs go to the
operator's own DM and **never** a group/other contact — the catastrophic failure
mode. Generated images (ADR-019) enqueue the same way: one delivery path, many
producers.

### Outbound queue — media extension (carried from the prior draft, still valid)

Extend `outbound_queue` (created identically in `messaging/whatsapp/src/db.ts`
and `chores/reminders/src/store.rs`) with idempotent `ALTER TABLE … ADD COLUMN`
(guard the duplicate-column error):

```sql
ALTER TABLE outbound_queue ADD COLUMN kind       TEXT NOT NULL DEFAULT 'text';
ALTER TABLE outbound_queue ADD COLUMN media_path TEXT;   -- abs path to the decrypted temp
ALTER TABLE outbound_queue ADD COLUMN mimetype   TEXT;
ALTER TABLE outbound_queue ADD COLUMN filename   TEXT;   -- display name
```

`kind ∈ {text, image, document}`; `body` doubles as the caption. The drain
branches: `{ image, caption }` / `{ document, mimetype, fileName, caption }` /
`{ text }`. A venue-correct enqueue CLI (`messaging/whatsapp/src/enqueue-media.ts`)
+ a thin skill let a session queue a file; **authorization is enforced at the
drain** (allowlisted JID), not the enqueue.

### Headless Drive access — the real implementation wrinkle

The Google Drive access Nucleus has today is a **claude.ai connector
(interactive auth)**. The WhatsApp bot runs as a **headless** tmux session and
likely won't have it (cf. the memory: interactively-authed MCP servers are absent
in headless runs). So store/retrieve needs a **non-interactive Drive path** — a
Google **service account** (or an OAuth refresh token / `rclone`) with creds on
the Mac (`.env` / Keychain). This is where most of the build effort concentrates,
and it's independent of the WhatsApp + encryption work.

## Rollout phases

1. **Outbound media queue** — schema migration (both writers) + drain branch +
   enqueue CLI/skill. The delivery substrate; also unblocks ADR-019 image
   delivery. Validate with a static PNG to the DM; verify legacy `text` rows
   unaffected.
2. **Drive plumbing** — non-interactive Drive access (service account) + the
   encrypt/decrypt helpers (Keychain key + backup) + temp-file hygiene.
3. **Manifest** — the Obsidian text index/audit + resolve-by-name lookup.
4. **Outbound retrieval** ("send me my X") — the headline skill, DM-locked,
   by-reference, audited. This is the payoff.
5. **Inbound capture** — the image/document branch → archive (encrypt→Drive→
   manifest) + the act-on-this `Read` path.

Phases 1–3 are independent; 4 needs 1+2+3; 5 needs 2+3.

## Consequences

- `outbound_queue` becomes the single media-delivery primitive (reminders,
  gallery, doc retrieval) with one auth gate.
- Storage is fully decoupled from the vault — the vault holds only the manifest.
  No binary churn, no graph clutter; the prior draft's `_attachments`/`attach`-op
  complexity disappears.
- Encryption adds a key-management surface (Keychain + a mandatory backup) and a
  lost-key footgun.
- Headless Drive access (service account) is real, separable work.
- Sensitive-document delivery is a high-consequence path: the DM JID-lock,
  by-reference handling, and audit manifest are all load-bearing, not nice-to-have.

## Open questions

- Encryption tool + container format (`age` vs `gpg` vs `openssl enc`).
- Manifest shape: one note + per-entry rows/tags, vs a small DB; where under
  `4-Areas/Documents/`.
- Name resolution: a tag/alias convention so "passport" / "RG" / "CPF" map
  unambiguously to one file (and disambiguation when several match).
- Office formats inbound (`.docx`/`.xlsx`): convert to render, or store-only.
- Confirm-before-send for identity docs? (DM-to-self is pre-authorized; mis-
  delivery is the real risk and the JID-lock already covers it — likely no.)
