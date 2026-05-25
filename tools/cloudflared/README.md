# Cloudflare tunnel config

One combined ingress for the Nucleus host. Since ADR-015 every operator
surface is a single binary (`nucleus-dashboard` on `localhost:8092`), so
there is one Nucleus hostname, not three. A second hostname (the
containers project) shares the same tunnel. Hostnames are
operator-specific and live in `.env`:

- `$NUCLEUS_PUBLIC_URL` → `localhost:8092` (nucleus-dashboard)
- `$NUCLEUS_CONTAINERS_PUBLIC_URL` → `localhost:4000` (separate project)

## Perimeter (ADR-011)

This template is the **pre-lockdown** config: it routes the Nucleus
hostname through the public tunnel for initial bring-up. ADR-011 then makes
the dashboard **tailnet-private at its real hostname** (Tailscale + a local
Caddy TLS terminator — see `tools/caddy/`) and **removes the nucleus route
from this tunnel entirely**. After lockdown the only thing this config
serves is the containers hostname (a separate project, untouched). So on a
locked-down host, `~/.cloudflared/config.yml` has just the containers
ingress + the catch-all 404.

## Setup

```bash
# 1. Create (or reuse) a cloudflared tunnel; note the UUID it prints.
cloudflared tunnel create my-tunnel

# 2. Point DNS at the tunnel for each hostname.
cloudflared tunnel route dns my-tunnel "$(echo $NUCLEUS_PUBLIC_URL | sed -E 's#^[a-z]+://##; s#/.*$##')"
cloudflared tunnel route dns my-tunnel "$(echo $NUCLEUS_CONTAINERS_PUBLIC_URL | sed -E 's#^[a-z]+://##; s#/.*$##')"

# 3. Generate the real config from the template. install.sh reads .env and
#    substitutes __USER_HOME__, __TUNNEL_UUID__, and both hostnames.
TUNNEL_UUID=<uuid-from-step-1> ./tools/cloudflared/install.sh

# 4. Install it as the combined config and (re)start the tunnel.
cp tools/cloudflared/nucleus.yaml ~/.cloudflared/config.yml
cloudflared service restart   # or: cloudflared tunnel --config ~/.cloudflared/config.yml run
```

## Templates → real config

| Template (committed)  | Generated (gitignored) | Installed to             |
|-----------------------|------------------------|--------------------------|
| `nucleus.yaml.example`| `nucleus.yaml`         | `~/.cloudflared/config.yml` |

Placeholders substituted at install time:

- `__USER_HOME__` → `$HOME`
- `__TUNNEL_UUID__` → the `TUNNEL_UUID` env var you set
- `__NUCLEUS_HOSTNAME__` → host portion of `NUCLEUS_PUBLIC_URL`
- `__CONTAINERS_HOSTNAME__` → host portion of `NUCLEUS_CONTAINERS_PUBLIC_URL`

## Gitignore

The generated `nucleus.yaml` (and `~/.cloudflared/config.yml`) encode your
specific tunnel UUID, credential path, and hostnames — gitignored. Only
`nucleus.yaml.example` is checked in.
