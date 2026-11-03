---
name: browser-auth-recovery
description: "Recover Playwright browser auth for Nucleus sessions (ADR-022): a browser that fails at launch with `Error reading storage state … ENOENT`, a page that should be logged in but renders logged out or redirects to a login page, or the legacy `Browser is already in use` error. Covers the operator-driven re-auth with `tools/playwright-auth` for any login site and the verification in a fresh session. Use whenever a browser-driven task or reminder fire is blocked by login state, even if auth is not mentioned."
flavor: recipe
trigger: model
mcp_needed: [playwright]
last_used: null
last_failure: null
failure_count_30d: 0
notify_on_failure: []
---

# browser-auth-recovery — restoring browser login state

Since ADR-022, every session runs `@playwright/mcp` with `--isolated
--storage-state ~/.nucleus/playwright-storage.json` (see `.mcp.json`). Each
session gets its own browser, seeded from that shared snapshot; there is no
shared browser and no profile lock in normal operation. Real logins live
only in the persistent auth profile `~/.nucleus/playwright-profile`, and only
`tools/playwright-auth/playwright-auth.mjs` opens it and writes the snapshot.

# When to invoke

- A browser launch fails with `Error reading storage state … ENOENT`.
- A page that should be logged in renders logged out, or redirects to a
  login path (`/login`, `/signin`, `?redirectTo=…`) after it loads. This
  applies to a reminder fire and to a manual task alike.
- A session fails with `Browser is already in use` (legacy, pre-ADR-022).
- The operator asks to log in to a site for automation, or to refresh a
  login that expired.

# Steps

1. **Classify the failure** from the error text or the final page URL:

   - **Missing snapshot** — `Error reading storage state … ENOENT` at
     browser launch (fresh machine, or the file was deleted). Run
     `node tools/playwright-auth/playwright-auth.mjs init` (creates an empty
     state; leaves an existing file alone). If logins should exist, run
     `… capture --origins <site-origin>[,<site-origin>…]`. Retry the task.
   - **Logged out** — the page renders without the login, or ends on a login
     path. Confirm it cheaply first: navigate to the site URL once and wait
     about 10 s, because some sites render the page and redirect only after
     hydration. At most 2 navigations; reloads never restore an expired
     login. Then go to step 2.
   - **`Browser is already in use` (legacy)** — only two sources remain: a
     still-running session whose Playwright MCP server predates the
     `.mcp.json` change (the session restart fixes it; the resident bots are
     restarted daily at 04:00 by the `bot-restart` launchd job), or two
     concurrent `playwright-auth` runs on the auth profile (wait and retry).
     If a freshly spawned session shows it, check `ps` for old MCP servers
     started with `--user-data-dir` before anything else. Do NOT delete
     `Singleton*` files or `pkill` browsers.

2. **Decide whether the auth profile still has a valid login.** If the
   operator logged in on the auth profile recently and only the snapshot is
   old, `node tools/playwright-auth/playwright-auth.mjs capture --origins
   <site-origin>` refreshes the snapshot without a login. Otherwise go to
   step 3.

3. **Ask the operator to log in.** Report the failure as an auth failure
   that blocks the task, not as a generic error, and give him the command:

   ```bash
   node tools/playwright-auth/playwright-auth.mjs login --url <site-url>
   ```

   It opens a headed browser on the auth profile at `<site-url>`. He logs
   in; the tool detects the login (a page on the site's origin, off any
   login path, for two checks in a row), waits a few seconds for tokens to
   be written, closes the window and captures the snapshot. Closing the
   window manually also works. Sites that keep tokens in localStorage or
   IndexedDB on another origin need that origin in `--origins`.

   - Never type credentials. Agents do not have them and must not ask for
     them.
   - A login inside a normal session's browser is not kept: the context is
     isolated and its state is discarded when it closes. Only
     `playwright-auth` (`login` or `capture`) writes the shared snapshot.

4. **Verify in a fresh session.** After the operator confirms the login,
   open a new browser (a fresh session, or close and reopen the browser in
   the session that failed), navigate to the site, and confirm that the
   logged-in page renders with no redirect to a login path. A browser opened
   before the capture still holds the old state.

5. **Resume the blocked work.** Produce the result again from live state
   instead of re-sending the failure text or old content. For a failed
   reminder fire, add a one-shot clone of the reminder (same prompt, same
   channels, `--at` a few minutes out); do not edit or cancel the recurring
   original.

6. **Close the browser** when done. This frees memory; since ADR-022 it is
   no longer needed to release a lock.

# Failure modes

- **Calling it an auth failure too early** — some sites render the page and
  redirect to login only after hydration. Wait about 10 s before reading
  the final URL. More than 2 navigations wastes time and fire attempts; an
  expired login only comes back through the operator.
- **Retrying an auth failure without restoring the login** — every retry
  fails the same way and sends another failure alert. The snapshot changes
  only when `playwright-auth` rewrites it.
- **Logging in inside a normal session's browser and calling it fixed** —
  the isolated context discards the login on close; the next session is
  logged out again.
- **Typing or requesting credentials** — never. The operator logs in
  himself in the headed `playwright-auth login` window.
- **Running the old lock-recovery ladder** (`pkill -f mcp-chrome-`, deleting
  `Singleton*` files) — obsolete since ADR-022, and the matching processes
  may belong to another project's browser or to the operator's open
  `playwright-auth login` window.
- **Deleting or hand-editing `~/.nucleus/playwright-storage.json`** — the
  tool writes it (mode 0600, live session cookies). Regenerate it with
  `playwright-auth`; never edit it, and never commit, paste or print its
  contents.
- **Hardcoding a literal home path** — use `$HOME` or `~`; the auth profile
  and the snapshot live under `~/.nucleus/`.
- **Verifying in a browser opened before the capture** — it keeps the old
  state; only a newly opened browser shows whether the snapshot works.
- **Declaring success without re-running the task** — the blocked output
  must be produced again after the login, not replayed from before it.
