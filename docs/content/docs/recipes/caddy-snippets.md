---
title: Caddy snippets cookbook
weight: 15
---

`caddy_extra_json:` (and `caddy_extra_caddyfile:`) are yoink's escape hatch for Caddy features that aren't deploy primitives — auth, rate limiting, headers, redirects, IP allowlists, body limits, etc. Yoink models the routing graph; Caddy models the traffic handling. This page is the lookup table for the most common patterns.

Each recipe shows:

- **Caddyfile** — what you'd write in vanilla Caddy / `caddy_extra_caddyfile:`. Friendlier syntax; what Caddy's own docs use.
- **JSON** — the equivalent for `caddy_extra_json:`. Lower-level; what Caddy's admin API consumes.
- **Yoink config** — the smallest service block that uses the snippet.

Both forms work on vanilla `caddy:2` — no plugins required unless noted.

{{< callout type="info" >}}
**Caddyfile vs JSON**: pick whichever you prefer. `caddy_extra_caddyfile:` shells out to `caddy adapt` at render time (requires docker on the operator's machine; one-time pull). `caddy_extra_json:` is parsed inline (no docker dep at render time). They're mutually exclusive on a single service.
{{< /callout >}}

## Forward auth → Authelia / Authentik / oauth2-proxy

Gates every request to this service through an auth-decision endpoint on another service. Standard pattern for SSO over self-hosted apps.

**Caddyfile:**
```yaml
caddy_extra_caddyfile: |
  forward_auth authelia:9091 {
    uri /api/verify?rd=https://auth.example.com
    copy_headers Remote-User Remote-Groups Remote-Email Remote-Name
  }
```

**JSON:**
```yaml
caddy_extra_json: |
  [{
    "handler": "forward_auth",
    "upstreams": [{"dial": "authelia:9091"}],
    "uri": "/api/verify?rd=https://auth.example.com",
    "copy_headers": ["Remote-User", "Remote-Groups", "Remote-Email", "Remote-Name"]
  }]
```

**Service:**
```yaml
- name: internal-app
  image: ghcr.io/me/internal
  domain: app.example.com
  caddy_extra_caddyfile: |
    forward_auth authelia:9091 { ... }
  run: { port: 3000 }
```

## Basic auth (single user)

For a quick admin page or dashboard. Caddy hashes the password with bcrypt at config-load.

**Caddyfile:**
```yaml
caddy_extra_caddyfile: |
  basic_auth /* {
    admin $2a$14$ABCDEFG...   # bcrypt hash from `caddy hash-password`
  }
```

**JSON:**
```yaml
caddy_extra_json: |
  [{
    "handler": "authentication",
    "providers": {
      "http_basic": {
        "accounts": [
          {"username": "admin", "password": "$2a$14$ABCDEFG..."}
        ]
      }
    }
  }]
```

Generate the hash:
```sh
docker run --rm caddy:2 caddy hash-password --plaintext 'your-password'
```

## IP allowlist

Restrict access to a set of IPs (CIDR ranges OK). Common pattern for tailnet-only routes.

**Caddyfile:**
```yaml
caddy_extra_caddyfile: |
  @internal client_ip 100.64.0.0/10 192.168.1.0/24
  handle @internal {
    # request continues to reverse_proxy below
  }
  handle {
    respond "Forbidden" 403
  }
```

**JSON:**
```yaml
caddy_extra_json: |
  [{
    "match": [{"not": [{"client_ip": {"ranges": ["100.64.0.0/10", "192.168.1.0/24"]}}]}],
    "handle": [{"handler": "static_response", "status_code": 403, "body": "Forbidden"}],
    "terminal": true
  }]
```

## Custom request / response headers

Strip a sensitive header before it reaches the upstream, or add one to the response.

**Caddyfile:**
```yaml
caddy_extra_caddyfile: |
  request_header -X-Internal-Token
  header X-Frame-Options "DENY"
  header X-Content-Type-Options "nosniff"
  header Referrer-Policy "strict-origin-when-cross-origin"
```

**JSON:**
```yaml
caddy_extra_json: |
  [
    {
      "handler": "headers",
      "request": {"delete": ["X-Internal-Token"]}
    },
    {
      "handler": "headers",
      "response": {
        "set": {
          "X-Frame-Options": ["DENY"],
          "X-Content-Type-Options": ["nosniff"],
          "Referrer-Policy": ["strict-origin-when-cross-origin"]
        }
      }
    }
  ]
```

(Yoink already emits `Strict-Transport-Security` for TLS sites by default — see `hsts:` in the [proxy guide](/docs/guide/proxy).)

## Request body size limit

Block oversized request bodies before they reach the upstream.

**Caddyfile:**
```yaml
caddy_extra_caddyfile: |
  request_body {
    max_size 10MB
  }
```

**JSON:**
```yaml
caddy_extra_json: |
  [{
    "handler": "request_body",
    "max_size": 10485760
  }]
```

## Redirect a specific path

Permanent redirect of a specific URL to another. (For full canonical-domain redirects use the `canonical_domain:` field instead.)

**Caddyfile:**
```yaml
caddy_extra_caddyfile: |
  redir /old-path /new-path 308
```

**JSON:**
```yaml
caddy_extra_json: |
  [{
    "match": [{"path": ["/old-path"]}],
    "handle": [{
      "handler": "static_response",
      "status_code": 308,
      "headers": {"Location": ["/new-path"]}
    }],
    "terminal": true
  }]
```

## Maintenance page

Force every request to a holding page for the duration of a maintenance window.

**Caddyfile:**
```yaml
caddy_extra_caddyfile: |
  respond "This service is undergoing scheduled maintenance. Back at 14:00 UTC." 503
```

**JSON:**
```yaml
caddy_extra_json: |
  [{
    "handler": "static_response",
    "status_code": 503,
    "body": "This service is undergoing scheduled maintenance. Back at 14:00 UTC.",
    "headers": {"Content-Type": ["text/plain; charset=utf-8"]}
  }]
```

## Plugins via `proxy.xcaddy:`

For directives that aren't in stock caddy (rate-limit, l4, redis-storage, third-party DNS providers), list the plugins under `proxy.xcaddy:` and yoink builds caddy on each proxy host with those modules baked in. No registry, no operator-side Dockerfile. See the [proxy guide](../guide/proxy#proxyxcaddy-block--caddy-plugins-without-a-registry) for the full block.

> **Snippets vs plugin directives:** `caddy_extra_caddyfile:` adapts via the bundled `caddy:2` adapter on the operator's machine, which doesn't know plugin-provided directives. If your snippet uses one (e.g. `rate_limit { ... }`), write it as `caddy_extra_json:` instead — that path skips the adapter entirely.

### Rate limiting (`caddy-ratelimit`)

```yaml
proxy:
  email: ops@example.com
  xcaddy:
    plugins:
      - github.com/mholt/caddy-ratelimit

services:
  - name: public-api
    domain: api.example.com
    caddy_extra_json: |
      [{
        "handler": "rate_limit",
        "zones": {"api": {"key": "{remote_host}", "events": 100, "window": "1m"}}
      }]
    run: { port: 8080 }
```

### Multi-host LE certs (`caddy-storage-redis`)

See the [Multi-host Redis storage recipe](/docs/recipes/multi-host-redis-storage) — same `proxy.xcaddy:` story, different plugin.

## See also

- [Reverse proxy guide](/docs/guide/proxy) — full schema reference for the routing primitives yoink models.
- [Caddy JSON config docs](https://caddyserver.com/docs/json/apps/http/servers/routes/handle/) — exhaustive reference for handler shapes.
- [Caddyfile docs](https://caddyserver.com/docs/caddyfile) — for `caddy_extra_caddyfile:` syntax.
