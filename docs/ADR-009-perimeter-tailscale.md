# ADR-009 — Perimeter: Tailscale-gating the operator surfaces

**Status:** Proposed (2026-05-17)

## Context

Three Nucleus surfaces are exposed via Cloudflare tunnel today: dashboard, chat, and news. All three are reachable at public URLs; none of the operator-only surfaces (dashboard, chat) have auth in front of them. The only thing keeping the dashboard private is the obscurity of its hostname — discoverable by anyone who runs a subdomain scan against the operator's registered domain or stumbles onto it via a referrer leak.

ADR-010 (canvas) raised the stakes: canvas adds an interactive, agent-rendered surface to chat. Even with the destructive-action policy (canvas presents choices, never executes destructive ops without a typed confirmation), running that surface on a publicly addressable URL is the wrong default. ADR-009 declared this perimeter work a hard prerequisite for shipping canvas.

The work is also useful independent of canvas. Dashboard and chat are operator-only by nature; only the operator's devices ever need to reach them. They should not be on the public internet at all. News is intentionally public (read-only RSS-shape content; ADR-001).

## Decision

Move dashboard and chat behind **Tailscale Serve**. News stays on Cloudflare tunnel (unchanged). The Cloudflare routes for dashboard and chat are removed in the same migration step — no parallel period, no preserved fallback. Per the cleanup-over-parallel principle, the old routes go away when the new ones come up.

URL shape uses Tailscale's default `<machine>.<tailnet>.ts.net` form for v1 — no custom domain, no split-DNS gymnastics. A future iteration may preserve `dashboard.<domain>` via split-DNS + a locally-terminated ACME cert; that is explicitly deferred.

## What moves and what stays

| Surface | Before | After |
|---|---|---|
| `dashboard` | Cloudflare tunnel at `$NUCLEUS_DASHBOARD_PUBLIC_URL` | Tailscale Serve at `https://<machine>.<tailnet>.ts.net/` (or a dedicated port path). CF route removed. |
| `chat` | Cloudflare tunnel at `$NUCLEUS_CHAT_PUBLIC_URL` | Tailscale Serve at `https://<machine>.<tailnet>.ts.net/chat/` (or similar). CF route removed. |
| `chat-v2` (per ADR-010) | not yet deployed | Tailscale Serve from day one; never gets a public CF route. |
| `news-api` | Cloudflare tunnel at `$NUCLEUS_NEWS_PUBLIC_URL` | Unchanged. Stays public. |
| Other cloudflared routes | Whatever's in `~/.cloudflared/config.yml` | Audit during rollout; remove anything that's operator-only and now redundant. |

## URL shape (v1)

Default Tailscale Serve hostnames: `https://<machine>.<tailnet>.ts.net`.

- Pick a short machine name on the Nucleus host (e.g., rename the lab box to `nucleus`)
- Pick a short tailnet name during Tailscale account setup
- Result: URLs like `https://nucleus.<tailnet>.ts.net/dashboard` and `https://nucleus.<tailnet>.ts.net/chat/`

Path-based routing on the same host vs. per-service hostnames is an install-time choice. Path-based is simpler (one Tailscale Serve config, multiple proxies); per-service hostnames need separate Serve configs but produce cleaner URLs. Default: path-based. If it gets confusing in practice, revisit.

The pretty-URL upgrade path (split-DNS + ACME for `dashboard.<domain>`) is documented in [Future work](#future-work). Not v1.

## Tailscale bootstrap

Fresh install. Steps the operator runs once:

1. **Account.** Create a Tailscale account (free tier covers solo-operator scale).
2. **Pick a tailnet name** at account creation. Short, memorable, not personal-identifying.
3. **Install client on the Nucleus host:**
   ```
   brew install tailscale         # or the official installer
   sudo tailscale up
   ```
   Authenticate via the URL it prints. Approve the device in the admin console.
4. **Name the host:** in the Tailscale admin console, rename the machine to something like `nucleus`.
5. **Install client on operator devices** that need to reach the dashboard: Mac (`brew install tailscale`), iPhone (App Store), any others. Authenticate each.

The bootstrap is one-time, manual, and does not need to land in the repo. The operator-facing instructions live in `README.md` (a short "Setup → Perimeter" section).

## Tailscale Serve config

On the Nucleus host, after `tailscale up`:

```
sudo tailscale serve --bg --https=443 --set-path /dashboard http://localhost:8090
sudo tailscale serve --bg --https=443 --set-path /chat      http://localhost:<chat-port>
sudo tailscale serve --bg --https=443 --set-path /chat-v2   http://localhost:<chat-v2-port>
```

(Exact ports come from `nucleus.toml`; the example values are placeholders.)

This terminates TLS on the Tailscale node using a Tailscale-provisioned cert valid for `<machine>.<tailnet>.ts.net`. Backend services keep listening on `localhost:<port>` — no changes to the dashboard or chat binaries.

To persist the Serve config across reboots, the operator runs the same commands once (they're sticky), or commits a `tailscale serve --set-path` invocation to a launchd plist. Plist template: `tools/launchd/tailscale-serve.plist.example`. (Optional — Tailscale's `--bg` mode persists by default; the plist is belt-and-suspenders.)

## ACLs

Default Tailscale ACL is "tailnet members can reach each other" — fine for solo operator.

If the operator ever shares the tailnet (family member, contractor), tighten ACLs to restrict the Nucleus host to the operator's own devices. ACL config lives in the Tailscale admin console; can be committed as a JSON file at `tools/tailscale/acl.hujson` if desired (not v1).

## Migration steps (one sitting)

1. Stand up Tailscale on the Nucleus host (bootstrap above). Verify `https://<machine>.<tailnet>.ts.net/` reaches `nginx` or a placeholder page.
2. Configure `tailscale serve` to proxy `/dashboard`, `/chat`, `/chat-v2` to the corresponding localhost ports.
3. Update `.env` on the Nucleus host:
   - `NUCLEUS_DASHBOARD_PUBLIC_URL=https://<machine>.<tailnet>.ts.net/dashboard`
   - `NUCLEUS_CHAT_PUBLIC_URL=https://<machine>.<tailnet>.ts.net/chat`
   - `NUCLEUS_CHAT_V2_PUBLIC_URL=https://<machine>.<tailnet>.ts.net/chat-v2`
4. Restart any binaries that read these env vars at startup (dashboard, news-fetcher if it links to the dashboard, etc.).
5. Install Tailscale on the operator's devices that need access (Mac, phone).
6. Verify reach from Mac and phone — both tailnet-connected and not.
7. **Same change**, remove `dashboard` and `chat` routes from `~/.cloudflared/config.yml`. Restart cloudflared.
8. Verify the old public URLs no longer resolve to anything (`dig` returns NXDOMAIN or the apex page).
9. Delete any DNS records (CF dashboard) that pointed at the old tunnel routes for dashboard / chat.
10. Audit `~/.cloudflared/config.yml` for any other operator-only routes that snuck in over time. If found and confirmed operator-only, remove them.

## Rollback

If Tailscale setup fails or the operator is locked out mid-migration:

- The Nucleus host is still reachable via direct SSH (Tailscale doesn't change that).
- The dashboard / chat services are still listening on `localhost:<port>` — `ssh -L 8090:localhost:8090 <host>` gets you there from any machine with SSH access.
- The cloudflared routes for dashboard / chat were removed in step 7; restoring them is reverting that one change.

No "preserve the CF route as fallback" path. SSH tunneling covers the rollback case; reinstating the public CF route is a deliberate decision, not a default.

## Operator-facing changes

- **Bookmarks.** Old `dashboard.<domain>` bookmarks die. Replace with `https://<machine>.<tailnet>.ts.net/dashboard`. Two devices (Mac + phone), should take five minutes.
- **Phone access.** Phone needs the Tailscale app installed + logged in. After that, it's transparent — opening the bookmarked URL works whether you're on Wi-Fi or cellular.
- **Café / hotel access.** Works as long as Tailscale is on (default behavior when the app is running).
- **A device that's not on the tailnet** (e.g., borrowed laptop, work-managed device) cannot reach the dashboard. That's the point.

## What this does NOT do

- **Does not gate news.** News stays public via the existing Cloudflare tunnel.
- **Does not add SSO / login.** Tailscale itself is the access control. No Cloudflare Access, no Google login, no Authelia.
- **Does not implement the pretty-URL preservation.** `dashboard.<domain>` does not survive v1. The cosmetic loss is accepted.
- **Does not move SSH / launchctl / system admin behind a different gate.** Existing SSH access patterns unchanged.

## Future work

- **Pretty-URL preservation.** Split-DNS within the tailnet maps `dashboard.<domain>` to the tailnet IP of the Nucleus host; a locally-terminated ACME cert (DNS-01 challenge against Cloudflare DNS) provides valid HTTPS for that hostname. Result: same URL as before, only reachable from the tailnet. Estimated effort: half a day. Tracked as a follow-up once v1 is stable and the operator has formed an opinion about whether the URL matters enough.
- **Tailscale SSH.** Tailscale offers SSH-over-tailnet with key management in the admin console. Worth evaluating once the basic Tailscale plumbing is in place.
- **ACL formalization.** If the tailnet ever has more than the operator's devices, commit an ACL file (`tools/tailscale/acl.hujson`) rather than relying on console state.
- **Other tunnels audit.** During migration, log any other cloudflared routes that turn out to be operator-only. Either bring them behind Tailscale or document why they need to stay public.

## References

- ADR-001 — architecture / which surfaces exist and what they're for
- ADR-010 — canvas; declared this work as a hard prerequisite
- CLAUDE.md Rule 1 — secrets stay in `.env`; URLs are referenced via `NUCLEUS_*_PUBLIC_URL`
- `cloudflared_setup` (T2 memory) — current tunnel config conventions
- Tailscale Serve docs — https://tailscale.com/kb/1242/tailscale-serve
