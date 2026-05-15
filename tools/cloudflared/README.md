# Cloudflare tunnel configs

Three tunnel ingress routes — news API, dashboard, chat. Hostnames are
operator-specific and live in `.env`:

- `news`: routes `$NUCLEUS_NEWS_PUBLIC_URL` → `localhost:8080` (news API)
- `dashboard`: routes `$NUCLEUS_DASHBOARD_PUBLIC_URL` → `localhost:8090` (dashboard)
- `chat`: routes `$NUCLEUS_CHAT_PUBLIC_URL` → `localhost:8091` (chat)

## Setup

```bash
# 1. Create (or reuse) a cloudflared tunnel; note the UUID it prints.
cloudflared tunnel create my-tunnel

# 2. Point your DNS records at the tunnel for each hostname.
cloudflared tunnel route dns my-tunnel "$(echo $NUCLEUS_NEWS_PUBLIC_URL | sed -E 's#^[a-z]+://##; s#/.*$##')"
cloudflared tunnel route dns my-tunnel "$(echo $NUCLEUS_DASHBOARD_PUBLIC_URL | sed -E 's#^[a-z]+://##; s#/.*$##')"
cloudflared tunnel route dns my-tunnel "$(echo $NUCLEUS_CHAT_PUBLIC_URL | sed -E 's#^[a-z]+://##; s#/.*$##')"

# 3. Generate the real yaml configs from the templates. install.sh reads
#    .env and substitutes __USER_HOME__, __TUNNEL_UUID__, and the matching
#    hostname placeholder.
TUNNEL_UUID=<uuid-from-step-1> ./tools/cloudflared/install.sh

# 4. Run the tunnel — either ad-hoc:
cloudflared tunnel --config tools/cloudflared/news.yaml run
# or install as a long-lived service:
cloudflared service install --config "$PWD/tools/cloudflared/news.yaml"
```

If you're hosting multiple services on the same tunnel, you can either run
multiple `cloudflared` processes (one per yaml) or merge the ingress rules
into one config.

## Templates → real configs

| Template (committed) | Generated (gitignored) |
|----------------------|------------------------|
| `news.yaml.example` | `news.yaml` |
| `dashboard.yaml.example` | `dashboard.yaml` |
| `chat.yaml.example` | `chat.yaml` |

Placeholders substituted at install time:

- `__USER_HOME__` → `$HOME`
- `__TUNNEL_UUID__` → the `TUNNEL_UUID` env var you set
- `__NEWS_HOSTNAME__` → host portion of `NUCLEUS_NEWS_PUBLIC_URL`
- `__DASHBOARD_HOSTNAME__` → host portion of `NUCLEUS_DASHBOARD_PUBLIC_URL`
- `__CHAT_HOSTNAME__` → host portion of `NUCLEUS_CHAT_PUBLIC_URL`

## Gitignore

Real `*.yaml` files are gitignored — they encode your specific tunnel UUIDs,
credential paths, and hostnames. Only `*.yaml.example` is checked in.
