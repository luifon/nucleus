# Caddy — private TLS for the dashboard at its real hostname (ADR-011)

Tailscale Serve can only issue certs for the ugly `*.ts.net` name. To reach
the dashboard at `$NUCLEUS_PUBLIC_URL`'s hostname **privately** (tailnet-only,
no login), Caddy terminates TLS for that hostname using a Let's Encrypt cert
obtained via the ACME **DNS-01** challenge, and reverse-proxies to
`localhost:8092`.

Why this shape:

- **DNS-01** means the host never needs public `:80`/`:443` inbound to prove
  domain ownership — it proves it by writing a TXT record via the Cloudflare
  API. Required here because the host lives on a CGNAT tailnet IP.
- The public DNS A record for the hostname points at the host's **tailnet IP**
  (`100.x`), which is unroutable off-tailnet. Caddy `bind`s only that IP. So
  on-tailnet devices reach it; everything else times out.
- No cloudflared in the path for this hostname (its route is removed); no
  login gate. Tailnet membership is the perimeter.

## Files

| Committed template            | Generated (gitignored)            |
|-------------------------------|-----------------------------------|
| `Caddyfile.example`           | `Caddyfile`                       |
| `caddy.plist.example`         | `<prefix>.caddy.plist`            |

The Caddy binary is a custom build that includes the `caddy-dns/cloudflare`
module (the stock/brew build does not). Fetch it from
`https://caddyserver.com/download` with that plugin selected, or
`xcaddy build --with github.com/caddy-dns/cloudflare`, and place it at
`~/.local/bin/caddy`.

## Setup

```bash
# 1. Mint a Cloudflare API token: dashboard → My Profile → API Tokens →
#    "Edit zone DNS" template, scoped to your zone. Put it in .env:
#      CF_API_TOKEN=...
#
# 2. Generate the real Caddyfile + plist from the templates.
./tools/caddy/install.sh

# 3. Load the LaunchDaemon (needs sudo — Caddy binds :443; runs as root).
#    install.sh prints the exact sudo lines.

# 4. Point the hostname's public DNS at the tailnet IP, DNS-only (grey
#    cloud). Done via the Cloudflare API with the same token, or by hand.
```

Certs live in `tools/caddy/data/` (gitignored); Caddy auto-renews them.
Logs: `memory/caddy.log`.
