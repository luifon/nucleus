# ADR-007 — Gmail + Calendar via Claude.ai MCP (JARVIS persona)

**Status:** Accepted (2026-05-15)

## Context

Two unlocks the existing Nucleus surfaces can't deliver:

1. **Calendar invites from any bot.** "Alfred, dentist Monday 17h" should land on the user's personal calendar with native alerts — not just a once-per-minute Discord nudge from the local `reminders` table.
2. **Inbox metabolism** for the trash account (`$NUCLEUS_GMAIL_ACCOUNT`) — currently 5,500+ emails of mostly machine-sent junk that the user never sees.

Two implementation paths considered:

- **A.** Our own OAuth client in `messaging/gmail/`, raw Google API access via `reqwest`. Full control, but ~OAuth flow + refresh + token storage + revocation handling code we have to write and maintain, plus Anthropic-Max doesn't subsidize the API calls.
- **B.** The Claude.ai-hosted Gmail / Calendar / Drive MCP servers. No auth code; we just call MCP tools. Token lives in Anthropic's stack.

Chose **B**. The trash account is low-sensitivity — losing it would lose nothing irreplaceable. The cost (auth runs through Anthropic instead of our `.env`) is far below the benefit (no auth code, refresh is Anthropic's problem). If a sensitive Gmail account ever needs the same treatment, revisit with path A.

## Decision

### Identity

- The crate is `messaging/gmail/` (venue-named per Rule 7).
- The persona is **JARVIS** — distinct from Alfred (WhatsApp) and Jerry Lewis (Discord). Lives at `messaging/gmail/persona.md`, substituted into the spawned `claude` session via `--append-system-prompt`.
- JARVIS operates on `$NUCLEUS_GMAIL_ACCOUNT` directly — no separate bot account, no auto-forwarding setup. JARVIS *is* the persona on the trash account.
- User's actual personal email lives in `.env` as `NUCLEUS_PERSONAL_EMAIL` — that's the address JARVIS adds as attendee on calendar events so invites land on the personal calendar.

### Access mechanism

- All Gmail / Calendar / Drive operations go through Claude.ai's MCP servers (`mcp__claude_ai_Gmail__*`, `mcp__claude_ai_Google_Calendar__*`, `mcp__claude_ai_Google_Drive__*`).
- The launchd-spawned `claude` subprocess inherits the user's Claude Max MCP integrations — same auth surface as interactive use. **Verify on first prod run** that MCPs are reachable in the headless-tmux context; if not, fall back to interactive-only and reconsider path A.
- No `gmail/calendar/drive` SDK pulled into Cargo.toml. The crate is mostly: spawn JARVIS session, post results.

### What MCP gives us and what it doesn't

| Capability | MCP status | Nucleus workaround |
|---|---|---|
| Search threads | ✅ `search_threads` | — |
| Read thread | ✅ `get_thread` | — |
| Apply / remove labels | ✅ `label_thread` / `unlabel_thread` | — |
| Create / delete labels | ✅ `create_label` / `delete_label` | — |
| **Delete email** | ❌ no direct tool | Add system label `TRASH` — equivalent (Gmail auto-purges after 30d) |
| **Send mail** | ❌ only `create_draft` | Outbound = drafts; user one-clicks send. Acceptable: Rule 6 says don't send to anyone but the user anyway. |
| **Create Gmail filter** | ❌ not exposed | Bot-driven daily metabolism handles future mail instead |
| Calendar event CRUD | ✅ full | — |
| Drive read | ✅ | — |
| Drive write/upload | ❌ | Vault backup deferred (see "Out of scope") |

### Calendar — the primary unlock

`reminders --channel calendar` extends the existing `reminders` binary. Any bot (Alfred, Jerry, JARVIS itself, or you on the command line) can schedule:

```
You → Alfred:    "dentist Monday 17h"
Alfred (claude)  → reminders add --at "2026-05-18T17:00:00<offset>" \
                                 --body "dentist" --channel calendar
reminders bin    → mcp__claude_ai_Google_Calendar__create_event:
                     summary: "dentist"
                     start:   2026-05-18T17:00:00<offset>
                     end:     start + DEFAULT_DURATION (config)
                     attendees: [$NUCLEUS_PERSONAL_EMAIL]
                     send_updates: "all"
Google           → invite email to $NUCLEUS_PERSONAL_EMAIL
You              → see event on personal calendar, native phone/watch alerts
```

This is shared infrastructure — calendar isn't JARVIS-exclusive. The MCP is authenticated against the-trash-account, so the calendar is on the-trash-account, but invites land on the personal email through standard Google flow.

**Gated on the reminders v2 refactor currently in progress.** Don't start building the `--channel calendar` branch until the reminders binary's TZ-correct, pause/resume, `--cron` work lands. The new branch needs to ride on whatever model v2 settles on.

### Inbox metabolism — the daily job

Daily 5am launchd cron (`tools/launchd/gmail-metabolism.plist.example`). No Pub/Sub.

Flow per run:

1. Open `memory/gmail.db`. Read last-run watermark.
2. Search `is:unread newer_than:<watermark>` via MCP.
3. For each thread: spawn a one-shot SessionPool turn against JARVIS — prompt asks for `{label, confidence, reason}` JSON.
4. Apply label via `label_thread`.
5. If sender is on the SQLite kill-list OR classifier returned `nucleus/junk` with confidence > threshold → also apply `TRASH`.
6. Save watermark.
7. Count by label, post one-line digest to `DISCORD_HOME_CHANNEL_ID` via `discord_sdk::send_announcement`.

**Locked label taxonomy for v1:**

| Label | Meaning | Auto-trash? |
|---|---|---|
| `nucleus/transactional` | Receipts, 2FA, order confirmations | No — kept silently, indexed for "what did I pay for last month" |
| `nucleus/newsletter/keep` | High-signal newsletters worth your time | No |
| `nucleus/newsletter/skim` | Low-priority newsletters | No |
| `nucleus/human` | From an actual person, not a domain | No — labeled only; v1 doesn't escalate. v2 refines. |
| `nucleus/junk` | Mass-sent marketing / spam shape | Yes |
| `nucleus/review` | Classifier uncertain | No — user decides |
| `nucleus/unsubscribed` | Receipt of List-Unsubscribe action | No |

If new categories surface during cleanup, add them then. Nested labels (`/`) are Gmail-supported; if `create_label` doesn't accept them, fall back to flat names (`nucleus-newsletter-keep`).

### Kill-list

SQLite table at `memory/gmail.db`:

```sql
CREATE TABLE killlist_senders (
  email      TEXT PRIMARY KEY,
  added_at   TEXT NOT NULL,
  reason     TEXT,
  added_by   TEXT  -- 'manual' | 'classifier' | 'user_discord_command'
);
```

Why SQLite over T2 markdown: high-churn, structured, machine-read-only. T2 is for facts the brain spawns *read* every session; the kill-list is read only by the metabolism job.

Adding to the list: bot can promote `nucleus/junk` senders automatically after N occurrences. Manual seeding via a one-shot CLI (`gmail-metabolism killlist add <sender>`).

### Auth-expiry handling

Catch-on-use:

1. Any MCP call returns an auth_required-shaped error.
2. JARVIS's session catches it, calls `discord_sdk::send_announcement` to the home channel: "Gmail MCP auth expired — run `/mcp` to re-auth."
3. The current run aborts; watermark stays put.
4. Next 5am cron tries again. If still failing, posts again.

No flag file, no manual pause/resume. The job is idempotent — re-running after re-auth picks up where it stopped.

Same pattern for Calendar / Drive when they're in play. Hourly health-check is not built v1; catch-on-use is enough since the daily job exercises auth daily.

### Daily digest

End of 5am run posts to `DISCORD_HOME_CHANNEL_ID`:

```
▸ overnight email: 47 transactional, 12 newsletters, 1 human, 8 junk → trashed
```

One line. Mentions you (`<@$DISCORD_ALLOWED_USER_IDS[0]>`) only if `human` count > 0.

No separate channel — uses the existing home channel like reminders and news already do.

### Configuration shape

`nucleus.toml`:
```toml
[gmail]
metabolism_cron = "0 5 * * *"         # daily 5am
classifier_model = "claude-haiku-4-5" # cheap; classification doesn't need sonnet
killlist_auto_promote_threshold = 3   # N occurrences before auto-add to killlist
calendar_default_duration_min = 30    # default event length
```

`.env`:
```
NUCLEUS_PERSONAL_EMAIL=you@your-personal-domain
```

### Persona — JARVIS voice

Brief, dry, precise. Anticipates two steps ahead. Addresses by `${USER_NAME}`. Reports outcomes, not process. Light wit acceptable; preamble is not. Will not draft mail to anyone but the user without explicit per-message authorization (Rule 6).

Distinct from Alfred (quiet competence, butler) and Jerry (veteran field handler dry). Each persona occupies a different IP, deliberately.

## Out of scope (v1)

- **Pub/Sub real-time** — daily polling is enough for a trash account. Revisit if cadence proves too slow.
- **JARVIS interactivity** ("hey JARVIS, did Anthropic bill me?") — v1 is autonomous-only (daily job + calendar invite path). Future: maybe a webhook-triggered surface, or routing through Alfred.
- **Vault writes from JARVIS** — Rule 9 doesn't bind JARVIS v1. Brain-dump-worthy emails get labeled; user manually forwards to Alfred for capture.
- **Per-human escalation** — `nucleus/human` is label-only in v1. Discord ping / personal-email forward refined in v2.
- **Drive vault backup** — deferred. Drive MCP doesn't expose upload; we'd need raw API. Worth coming back to.
- **Auto-send mail** — MCP doesn't support; drafts only. Rule 6 says we shouldn't auto-send to non-user destinations anyway.

## Slice number

This is **S10** in the slice roadmap (add row to ADR-001). It's two slices in one shipment — calendar wiring (gated on reminders v2) + inbox metabolism (independent) — but they share the same MCP plumbing and persona, so treating as one slice.

## Risks

- **MCP availability in launchd-spawned `claude` subprocess** — not yet verified end-to-end. The other bots already run `claude` in launchd tmux and presumably have MCP access; this assumes that pattern holds. Test on first prod run.
- **Anthropic token storage** — fine for trash account, would not be fine for a sensitive account.
- **Rate limits** — unknown. Likely fine for daily 5am bursts. Catch-on-use handles 429s by retrying next run.
- **Nested labels via MCP** — uncertain whether `create_label` accepts `parent/child` syntax. Fallback is flat names.
- **The reminders v2 refactor** — calendar wiring is fully blocked on it. Until v2 lands, only the inbox-metabolism half ships.

## Cross-references

- [[ADR-001]] — add S10 row; update architecture diagram with JARVIS / Gmail surface
- [[ADR-002]] — memory tiers; SQLite kill-list is T1, no T2 promotions in v1
- CLAUDE.md Rule 4 (tmux Session/SessionPool), Rule 6 (outbound auth), Rule 7 (venue=code, persona=character)
- `messaging/whatsapp/` — reference for the brain-dump-style classifier pattern
- `messaging/discord/` — reference for the Rust SessionPool pattern
- `chores/reminders/` — extends with `--channel calendar` once v2 lands
