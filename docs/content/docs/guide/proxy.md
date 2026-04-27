---
title: Reverse proxy
weight: 4
---

Yoink bundles **Caddy** as a managed proxy service. One field on a service exposes it on a domain with TLS:

```yaml
services:
  - name: api
    image: ghcr.io/me/api
    tag: v1
    domain: api.example.com         # ← that's the whole opt-in
    run:
      port: 8080
proxy:
  email: ops@example.com            # required for Let's Encrypt
```

`yoink up`. Caddy is reconciled. App is reconciled. TLS is issued. Routing works.

## How it works

When any service has `domain:` set, yoink synthesizes a `yoink-proxy` service running `caddy:2` and renders its config from your `yoink.yaml`. The config is pushed via Caddy's admin API at deterministic points in the rolling deploy — yoink owns the routing flip, no Docker-label fights with a separate reconciler.

Two Docker networks are managed for you:

- **`yoink-ingress`** — proxy joins this; every service with `domain:` joins this. How the proxy reaches your containers by name.
- **`yoink-proxy-admin`** — the proxy's admin API listens here. Never published to the host or the public internet; yoink reaches it via SSH-tunneled access for the duration of one `/load` call.

Containers are named in upstream entries (not IPs), so a container restart with a new IP doesn't break routing — Docker DNS handles it. Caddy active health checks remove dead backends from rotation.

## Schema

### Per service

| Field | Type | Default | Notes |
|---|---|---|---|
| `domain` | string \| list | — | Hostname(s); presence enables proxying. |
| `tls` | `auto` \| `off` \| `cert` | `auto` | `auto` = ACME via Let's Encrypt. `off` = HTTP only. `cert` = inline cert from sealed secrets. |
| `tls_cert_secret` | string | — | Sealed-secret name holding the PEM cert (for `tls: cert`). |
| `tls_key_secret` | string | — | Sealed-secret name holding the PEM private key (for `tls: cert`). |
| `caddy_extra_json` | string (JSON) | — | Raw Caddy handler JSON merged into this site's route, before the auto-generated `reverse_proxy`. Escape hatch for advanced features (auth, rate limit, headers). |

`run.port:` is required when `domain:` is set — the proxy needs to know which container port to forward to. `run.healthcheck_path:` (if set) is reused as Caddy's active health check URI.

### Top-level `proxy:` block

| Field | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | implicit when any service has `domain:` | |
| `email` | string | — | Let's Encrypt registration email. **Required when any service uses `tls: auto`.** |
| `image` | string | `caddy:2` | Override to use an [`xcaddy`](https://github.com/caddyserver/xcaddy)-built image with plugins (rate-limit, l4, redis-storage, …). |
| `cert_volume` | string | `yoink_caddy_data` | Named volume for ACME state and certs. Persisted across proxy restarts. |

That's the full surface. Five service-level fields, four proxy-level fields. Anything beyond that is `caddy_extra_json:` or a custom proxy image.

## Cert volume — keep it

The `yoink_caddy_data` named volume holds your ACME state, issued certs, and OCSP staples. Don't delete it casually — Let's Encrypt rate-limits aggressively (5 duplicate-cert issuances per week) and a fresh volume = re-issuance.

`yoink prune` won't touch the volume. Manually purging it is on you.

## Rolling deploys with zero traffic loss

Per service deploy:

1. New replica starts.
2. Yoink waits for its healthcheck.
3. Yoink pushes updated Caddy config (`/load`) — new replica added to upstream pool.
4. Caddy gracefully reloads in-process; no in-flight requests dropped.
5. Old replica stops.
6. Caddy active health check notices the old upstream is gone and removes it.

Every step is yoink-driven; no second reconciler. The config push happens only when the *set* of containers changes — exactly when yoink is in the loop.

## What yoink doesn't model

Auth, rate limiting, headers, CORS, redirects, mTLS, geo-blocking, WAF, L4/TCP routing — none of these are in yoink's schema. They live in `caddy_extra_json:` using Caddy's [JSON config syntax](https://caddyserver.com/docs/json/apps/http/servers/routes/handle/), which is what Caddy's docs use directly. The escape hatch is the *advertised* path for these — yoink models deploy primitives, Caddy models traffic handling.

### Example: `forward_auth` to Authelia

```yaml
services:
  - name: authelia
    image: authelia/authelia
    domain: auth.example.com
    run: { port: 9091 }

  - name: internal-app
    image: ghcr.io/me/internal
    domain: app.example.com
    caddy_extra_json: |
      [
        {
          "handler": "forward_auth",
          "upstreams": [{"dial": "authelia:9091"}],
          "uri": "/api/verify?rd=https://auth.example.com",
          "copy_headers": ["Remote-User", "Remote-Groups", "Remote-Email"]
        }
      ]
    run: { port: 3000 }
```

### Example: rate limit (requires `caddy-ratelimit` plugin)

```yaml
proxy:
  image: ghcr.io/me/caddy-with-ratelimit:2.7   # xcaddy build
  email: ops@example.com

services:
  - name: public-api
    domain: api.example.com
    caddy_extra_json: |
      [
        {
          "handler": "rate_limit",
          "zones": {"api": {"key": "{remote_host}", "events": 100, "window": "1m"}}
        }
      ]
    run: { port: 8080 }
```

## What's next

- [Cloudflare Origin Certificates](/docs/recipes/cloudflare-origin-certs) — skip Let's Encrypt entirely with a 15-year cert from Cloudflare, sealed in the repo.
- [Multi-host LE with Redis storage](/docs/recipes/multi-host-redis-storage) — share ACME state across hosts so they don't all hit Let's Encrypt rate limits.
- [Caddy JSON config reference](https://caddyserver.com/docs/json/) — for `caddy_extra_json:` snippets.
