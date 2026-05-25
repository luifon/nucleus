# ADR-011 — Perimeter: Tailscale + Caddy, private dashboard at its real hostname

**Status:** Accepted — **Implemented (2026-05-24).**

> **History.** Drafted 2026-05-17 against the pre-ADR-015 topology (three
> operator crates at three public Cloudflare hostnames). [[ADR-015]]
> collapsed those into one binary (`nucleus-dashboard`, `localhost:8092`)
> at a single origin (`$NUCLEUS_PUBLIC_URL`). An interim rewrite proposed a
> *path-scoped* split (operator paths private, `/news/api/*` left public).
> At implementation the operator chose to **lock the whole origin down** —
> news included — **and keep the real hostname** rather than fall back to a
> `*.ts.net` URL. This document records the as-built result. See git
> history for the superseded path-scoped framing.

## Context

Every operator HTTP surface is one binary (`nucleus-dashboard`,
`localhost:8092`) at `$NUCLEUS_PUBLIC_URL`, fronted by a Cloudflare tunnel.
None of it had auth — the only thing keeping it private was hostname
obscurity. ADR-012 (canvas) made an interactive agent surface on a public
URL untenable, so a real perimeter became a prerequisite.

Two constraints shaped the final design:

1. **No login.** Network membership should be the access control, not an
   SSO/login gate (Cloudflare Access was considered and declined for the
   login friction).
2. **Keep `$NUCLEUS_PUBLIC_URL`'s real hostname.** A `*.ts.net` URL was
   rejected; the dashboard should stay reachable at its existing name so
   bookmarks and the news-fetcher's links keep working.

Those two together rule out both Cloudflare Access (login) and bare
Tailscale Serve (which can only issue certs for `*.ts.net`). The answer is
Tailscale for the network + a local TLS terminator for the real hostname.

## Decision

Lock the entire `nucleus-dashboard` origin behind the tailnet. **Nothing**
is publicly reachable anymore — including `/news/api/*` (this reverses
ADR-001's "news is public"; the operator confirmed no external consumer).
The dashboard stays at its real hostname, reachable only by tailnet
members:

- **Tailscale** provides the private network (the host + the operator's
  devices on one tailnet).
- **Caddy** (local, on the host) terminates TLS for the real hostname
  using a **Let's Encrypt cert obtained via the ACME DNS-01 challenge**
  (Cloudflare DNS API), and reverse-proxies to `localhost:8092`. DNS-01 is
  required because the host has no public inbound — it proves domain
  ownership by writing a TXT record, not by answering on :80/:443.
- **DNS:** the public A record for the hostname points at the host's
  **tailnet IP** (a `100.x` CGNAT address), **DNS-only** (not proxied).
  CGNAT is not internet-routable, so off-tailnet clients resolve the name
  but cannot reach it; tailnet members route to it over the mesh.
- **Cloudflared** no longer fronts the nucleus hostname at all (its
  ingress route was removed). The tunnel remains only for the unrelated
  `$NUCLEUS_CONTAINERS_PUBLIC_URL` project.

Net effect: `https://<nucleus-host>/` serves the full dashboard with a
valid public cert, no login, **only** from devices on the tailnet. From
anywhere else the name resolves to an unroutable `100.x` and times out.

## Access model

Every device reaches the dashboard the same way: install the **Tailscale
client**, sign into the tailnet, toggle it on. Then `$NUCLEUS_PUBLIC_URL`
works from that device anywhere (wifi or cellular) — transparently, no
login. Clients exist for macOS, iOS, **Windows** (`winget install
tailscale`), **Linux** (`curl -fsSL https://tailscale.com/install.sh |
sh`), Android, etc. The only devices that can't get in are ones you can't
install Tailscale on (a work-managed or borrowed machine) — which is the
point.

## Components (as built)

| Piece | Role |
|---|---|
| Tailscale (`tailscale up`) | The private mesh. Host renamed `nucleus`; HTTPS/MagicDNS enabled in the admin console. |
| Caddy (custom build w/ `caddy-dns/cloudflare`) | TLS terminator + reverse proxy. Binds the tailnet IP `:443`, cert via DNS-01, → `localhost:8092`. Runs as a **root LaunchDaemon** (`__LAUNCHD_PREFIX__.caddy`) since :443 is privileged. |
| Cloudflare DNS A record | `$NUCLEUS_PUBLIC_URL` host → tailnet `100.x`, DNS-only. Flipped from the old proxied CNAME→tunnel via the CF API. |
| `CF_API_TOKEN` (`.env`) | Cloudflare token scoped Zone:DNS:Edit on the zone. Used by Caddy for cert issuance/renewal **and** for the one-time DNS flip. |
| cloudflared | nucleus ingress **removed**; only the containers route remains. |
| `tailscale serve` | **Not used** (it can't do custom-domain certs); turned off so Caddy owns :443. |

Repo artifacts: `tools/caddy/` (`Caddyfile.example`, `caddy.plist.example`,
`install.sh`, `README.md`). Generated `Caddyfile` + real plist + cert store
(`tools/caddy/data/`) are gitignored; the cert store and the token-bearing
plist never touch git.

## Migration steps (as executed)

Ordering rule: **prove the new path before removing the old one.**

1. Tailscale bootstrap: `brew install tailscale`, `sudo tailscale up`,
   approve + rename host to `nucleus`, enable HTTPS in the admin console,
   install clients on operator devices.
2. Mint a Cloudflare API token (Edit zone DNS, scoped to the zone) → `.env`
   as `CF_API_TOKEN`.
3. `./tools/caddy/install.sh` — generate the real `Caddyfile` (binds the
   tailnet IP, DNS-01 via Cloudflare) + the LaunchDaemon plist.
4. `sudo tailscale serve --https=443 off` (free :443), then load Caddy:
   `sudo cp … /Library/LaunchDaemons/`, `sudo chown root:wheel …`,
   `sudo launchctl bootstrap system …`. Caddy obtains the cert via DNS-01.
5. Verify Caddy serves HTTPS on the tailnet IP with a valid cert (the
   tunnel still fronts the public name at this point — no outage).
6. **Flip DNS** via the CF API: proxied CNAME→tunnel becomes DNS-only
   `A → <tailnet-ip>`. Confirm propagation, then verify
   `$NUCLEUS_PUBLIC_URL` end-to-end (resolves to `100.x`, serves via Caddy,
   Let's Encrypt cert).
7. Remove the nucleus ingress from `~/.cloudflared/config.yml` (keep
   containers), `cloudflared … ingress validate`, restart the tunnel.

## Rollback

Fully reversible, no lockout risk — the host always has localhost +
direct SSH:

- **DNS:** restore the record to the proxied CNAME →
  `<tunnel-uuid>.cfargotunnel.com` via the CF API (old value was captured
  before the flip).
- **cloudflared:** restore `~/.cloudflared/config.yml.pre-adr011.bak` and
  restart — re-adds the nucleus ingress.
- **Caddy:** `sudo launchctl bootout system/__LAUNCHD_PREFIX__.caddy`.
- Reaching the dashboard while rolling back: `ssh -L 8092:localhost:8092
  <host>`, or just localhost on the box.

## What this does NOT do

- **No public surface for nucleus at all** — news included. (ADR-001's
  public-news premise is reversed here per operator decision; revisit if
  an external news consumer ever appears.)
- **No login / SSO.** Tailscale membership is the only gate.
- **Does not touch `$NUCLEUS_CONTAINERS_PUBLIC_URL`** — separate project,
  still public via the tunnel; its perimeter is that project's call.
- **Does not gate SSH / launchctl / system admin** differently. Unchanged.

## Operational notes

- **Cert renewal** is automatic — Caddy renews via DNS-01 on the same
  token. Keep `CF_API_TOKEN` valid; if it's rotated, regenerate the plist
  (`install.sh`) and reload Caddy. Certs persist in `tools/caddy/data/`.
- **Token rotation:** the token is the standing credential for renewals.
  Treat it like any secret in `.env`.
- **Boot ordering:** Caddy binds the tailnet IP, so it needs `tailscaled`
  up first. `KeepAlive` + `ThrottleInterval` make it retry until the
  interface is present; no hard dependency wiring needed.
- **`$NUCLEUS_PUBLIC_URL` stays the real hostname** in `.env` — every
  consumer (news-fetcher "full feed" links, the dashboard tunnel-probe
  tile) now resolves it to the tailnet path and keeps working for the
  operator.

## Future work

- **ACL formalization.** If the tailnet ever holds more than the operator's
  own devices, commit `tools/tailscale/acl.hujson` restricting the nucleus
  host to the operator's devices.
- **Tailscale SSH.** Evaluate SSH-over-tailnet now that the mesh exists.
- **Subnet/exit-node** considerations if the host's tailnet IP ever needs
  to be reached by name internally beyond `$NUCLEUS_PUBLIC_URL`.

## References

- [[ADR-001]] — surfaces; its "news is public" is reversed here
- [[ADR-015]] — single-origin consolidation this builds on
- [[ADR-012]] — canvas; declared this work a hard prerequisite
- CLAUDE.md Rule 1 — secrets in `.env` (`CF_API_TOKEN`, URLs via `NUCLEUS_*`)
- [[cloudflared_setup]] (T2 memory) — combined `~/.cloudflared/config.yml`
- `tools/caddy/README.md` — the Caddy terminator setup
- Tailscale DNS-01 / CGNAT-IP pattern; Caddy `caddy-dns/cloudflare`
