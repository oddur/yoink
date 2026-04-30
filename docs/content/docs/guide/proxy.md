---
title: Reverse proxy
weight: 4
---

Putting an app behind HTTPS used to be a 30-line nginx or Caddy config plus a Let's Encrypt CLI dance plus a renewal cron. Yoink replaces that with **one field**.

Set `domain: api.example.com` on a service and yoink stands up a managed Caddy in front of it: routes the hostname, issues a Let's Encrypt cert, sets up auto-renewal, and rolls cleanly with the service it's fronting. The Caddy is a normal yoink-managed service — same drift detection, same logs, same TUI — you just don't write its config yourself.

## TL;DR

```yaml
# yoink.yaml
hosts:
  - { address: 1.2.3.4, user: root }

proxy:
  email: ops@example.com           # for Let's Encrypt registration

services:
  - name: api
    image: ghcr.io/me/api
    tag: v1
    domain: api.example.com        # ← the whole proxy opt-in
    run:
      port: 8080
      healthcheck_path: /health
```

Point `api.example.com` DNS at `1.2.3.4`. Run `yoink up`. Done — `https://api.example.com` serves with a valid cert.

That's the floor. The rest of this page is reference + advanced patterns.

{{< callout type="info" >}}
**Walkthroughs for advanced setups** are split into focused how-to pages:

- [Cloudflare Origin Certificates](/docs/how-to/cloudflare-origin-certs) — skip Let's Encrypt with a 15-year cert + origin-pull mTLS that locks your origin to Cloudflare's edge.
- [gRPC hosting](/docs/how-to/grpc-hosting) — `upstream_h2c: true` for native gRPC backends (Tonic, grpc-go, grpc-java).
- [Multi-host Let's Encrypt with Redis storage](/docs/how-to/multi-host-redis-storage) — share ACME state across hosts to avoid rate limits.
{{< /callout >}}

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
| `path_prefix` | string | — | Match this path glob in addition to `domain:` so multiple services can share a hostname. Yoink orders routes so path-constrained ones come before catch-alls. |
| `tls` | `auto` \| `off` \| `cert` | `auto` | `auto` = ACME via Let's Encrypt. `off` = HTTP only. `cert` = inline cert from sealed secrets. |
| `tls_cert_secret` | string | — | Sealed-secret name holding the PEM cert (for `tls: cert`). |
| `tls_key_secret` | string | — | Sealed-secret name holding the PEM private key (for `tls: cert`). |
| `upstream_h2c` | bool | `false` | Talk to backend over HTTP/2 cleartext. Required for native gRPC backends (Tonic, grpc-go, grpc-java). |
| `compression` | bool | `false` | Emit `encode gzip zstd`. No-op behind a CDN. |
| `canonical_domain` | string | — | One of the `domain:` entries. Yoink 308-redirects every other entry to it (apex/www patterns). |
| `hsts` | bool | `true` for TLS sites | Emit `Strict-Transport-Security: max-age=31536000; includeSubDomains`. Default-on best practice. |
| `caddy_extra_json` | string (JSON) | — | Raw Caddy handler JSON merged into the route. Routes auto-wrapped in `subroute`. See the [snippets cookbook](#snippets-cookbook) below. |
| `caddy_extra_caddyfile` | string (Caddyfile) | — | Same as above but in Caddyfile syntax. Yoink shells out to `caddy adapt` at render time (needs docker on operator). Mutually exclusive with `caddy_extra_json:`. |

`run.port:` is required when `domain:` is set — the proxy needs to know which container port to forward to. `run.healthcheck_path:` (if set) is reused as Caddy's active health check URI.

#### Per-route handler order

The handler chain on a routed service runs in a fixed order:

1. Your `caddy_extra_json:` snippet's handlers (in declaration order — hand-written `forward_auth` runs before hand-written `headers`, etc.).
2. `compression` handler (`encode gzip zstd`), if `compression: true`.
3. HSTS header handler, if `hsts: true` *and* the service is serving TLS (`tls: auto` or `tls: cert`, or proxy-level TLS).
4. `reverse_proxy` to the upstream.

This means your snippet sees the request *first*, and your handlers (auth gates, rate-limits, redirects) run before yoink's compression/HSTS/forwarding. If you need behavior between the yoink-managed handlers — say, "compress everything, then auth-gate, then forward" — write the full chain yourself in `caddy_extra_json:` (including the reverse_proxy at the end) and turn `compression: false` so yoink doesn't double-add it.

For chains that should run *globally* on every request (CrowdSec, Coraza, fleet-wide rate-limit), use [`proxy.global_handlers:`](#proxyglobal_handlers-block--proxy-wide-middleware-chain) instead — they run before any per-service route matches.

### Top-level `proxy:` block

| Field | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | implicit when any service has `domain:` | |
| `email` | string | — | Let's Encrypt registration email. **Required when any service uses `tls: auto`.** Unused when `proxy.tls:` is set. |
| `image` | string | `caddy:2` | Bring-your-own custom caddy image (registry-pulled). Mutually exclusive with `xcaddy:` — pick one. |
| `xcaddy` | block (see below) | — | Build a custom caddy on each host using [`xcaddy`](https://github.com/caddyserver/xcaddy). Mutually exclusive with `image:`. |
| `cert_volume` | string | `yoink_caddy_data` | Named volume for ACME state and certs. Persisted across proxy restarts. |
| `bind` | string | — (all interfaces) | Host IP to bind `:80` and `:443` to. Common use: bind to a Tailscale IP so the proxy is reachable only over the tailnet. Admin port stays on `127.0.0.1` regardless. |
| `tls` | block (see below) | — | Proxy-level TLS — every routed service inherits this cert (and optional mTLS) by default. |
| `config_extra` | string (JSON) | — | Top-level Caddy JSON snippet, deep-merged into the rendered config before `/load`. Escape hatch for global settings yoink doesn't model as typed fields — `trusted_proxies`, `storage`, plugin app blocks. See the [`config_extra:` block](#proxyconfig_extra-block--global-caddy-config-escape-hatch). |
| `global_handlers` | list of strings (JSON) | `[]` | Caddy handlers (and/or routes) that run for every request before any service-specific route matches. The natural place for proxy-wide concerns: CrowdSec bouncer, Coraza WAF / OWASP CRS, fleet-wide rate limiting. See the [`global_handlers:` block](#proxyglobal_handlers-block--proxy-wide-middleware-chain). |

### `proxy.config_extra:` block — global Caddy config escape hatch

Caddy has many global config knobs yoink doesn't model as typed fields: server-level `trusted_proxies` and `client_ip_headers`, top-level `storage` (for shared ACME state across hosts), top-level app blocks for plugins like `cache-handler` and `coraza`. `config_extra:` takes a raw JSON snippet and **deep-merges** it into the rendered Caddy config before yoink pushes to `/load`.

```yaml
proxy:
  email: ops@example.com
  xcaddy:
    plugins:
      - github.com/WeidiDeng/caddy-cloudflare-ip
  config_extra: |
    {
      "apps": {
        "http": {
          "servers": {
            "main": {
              "trusted_proxies": {"source": "cloudflare"},
              "client_ip_headers": ["CF-Connecting-IP"]
            }
          }
        }
      }
    }
```

> ⚠ **The yoink-rendered server is named `main`, not `srv0`.** Caddy's docs and Caddyfile-adapted output overwhelmingly use `srv0` as the default server name; yoink uses `main`. If you write `apps.http.servers.srv0.trusted_proxies` here, the deep-merge silently creates a *second* server config block named `srv0` that listens on nothing — your `trusted_proxies` is dead config. Always use `apps.http.servers.main.<…>` for server-scoped settings.

Merge semantics:

- The string is parsed as JSON and validated as an object at config-load time (non-object JSON, invalid JSON, or arrays at the top level are rejected).
- Merge is recursive on objects: if both sides have an object at the same key, yoink merges their children; otherwise the user-supplied value wins.
- Yoink's own keys at non-overlapping paths are preserved — e.g. setting `apps.http.servers.main.trusted_proxies` doesn't clobber the `routes` array yoink generates from your services.
- User wins on every leaf conflict: setting `admin.listen` to your own value overrides yoink's default `0.0.0.0:2019`. Yoink trusts you.
- **Two paths are reserved for yoink:** `apps.http.servers.main.routes` (writing here would wipe every per-service route yoink rendered) and `apps.http.servers.main.tls_connection_policies` (writing here would silently disable mTLS configured via `proxy.tls.client_auth:` — a security regression). Both fail at config-load with a denylist error pointing at the typed field that owns each path.

Use it for plugin-specific top-level config too — for example, `caddy-storage-redis` for shared ACME state across a fleet ([how-to](../how-to/multi-host-redis-storage)), `caddyserver/cache-handler` advanced backends, `coraza` global directives, `crowdsec` agent connection settings.

### `proxy.global_handlers:` block — proxy-wide middleware chain

`config_extra:` injects raw config; `global_handlers:` is the typed slot for Caddy *handlers* you want running on every request before any service route matches. Yoink wraps the per-service routes in a `subroute` handler and prepends your handlers in front of it — the whole thing becomes one wildcard-match route, so the chain executes unconditionally.

```yaml
proxy:
  email: ops@example.com
  xcaddy:
    plugins:
      - github.com/hslatman/caddy-crowdsec-bouncer
      - github.com/corazawaf/coraza-caddy/v3
  global_handlers:
    - |
      {"handler": "crowdsec", "appsec_url": "http://crowdsec:8080"}
    - |
      {
        "handler": "waf",
        "directives": [
          "Include @coraza.conf-recommended",
          "Include @crs-setup.conf.example",
          "Include @owasp_crs/*.conf",
          "SecRuleEngine On"
        ]
      }
```

When to reach for it:

- **CrowdSec bouncer** — IP-based denial that should apply everywhere, not just to the services that opt in. Add a service tomorrow and it's protected without touching its config.
- **Coraza / OWASP CRS** — signature-based payload inspection on every request. Same "fleet-wide by default" framing.
- **Global rate limiting** — `caddy-ratelimit` zones that apply across hostnames (e.g. a single `per_ip` budget for the whole proxy).

Each entry is a JSON snippet: a single handler object (`{"handler": "x", ...}`), a route object (`{"match": ..., "handle": ...}`), or an array mixing the two. Same shape as `caddy_extra_json:` per-service. Order matters — entries run as a pipeline in the order written. Put cheap filters (CrowdSec hashmap lookup) before expensive ones (Coraza regex evaluation) so you don't burn CPU on traffic you were going to drop anyway.

`global_handlers:` and per-service `caddy_extra_json:` compose: the global chain runs first, then per-service handlers run on the matched route. Put fleet-wide concerns at the proxy level and per-app concerns on the service.

### `proxy.xcaddy:` block — caddy plugins without a registry

> Task-oriented walkthrough with debugging tips, common-plugin recipes, and operational notes lives at [Caddy plugins (xcaddy, no registry)](/docs/how-to/caddy-plugins). What follows is the schema reference.

Want rate-limit, redis-storage, the L4 module, or a non-bundled DNS provider? Just list them and yoink builds caddy on each host the proxy runs on:

```yaml
proxy:
  email: ops@example.com
  xcaddy:
    plugins:
      - github.com/caddyserver/caddy-l4
      - github.com/mholt/caddy-ratelimit@v0.1.0      # `module@version` to pin
      - github.com/caddy-dns/cloudflare
```

On the next `yoink up`, each proxy host runs a one-shot xcaddy build (multi-stage `caddy:2-builder` → `caddy:2`) and tags the result locally as `yoink-caddy:<hash>`. The proxy service runs from that tag — no registry needed. The hash is content-addressed over the build inputs, so subsequent `yoink up` runs short-circuit (`image_present` skip) until plugins or version change.

| Field | Type | Default | Notes |
|---|---|---|---|
| `plugins` | list of strings | required (non-empty) | One entry per caddy module. Bare module path or pinned (`module@version`) — same syntax as `xcaddy build --with`. Sorted alphabetically before hashing/rendering so order in the config file doesn't change the tag. |
| `caddy_version` | string | unset → xcaddy's latest tagged release | Caddy git tag to compile (e.g. `v2.8.4`). Pinned values are passed verbatim to `xcaddy build`, so use the form that's a real git tag in [caddyserver/caddy](https://github.com/caddyserver/caddy/tags). |
| `base_image` | string | `caddy:2` | Runtime stage of the Dockerfile (final `FROM`). |
| `builder_image` | string | `caddy:2-builder` | Builder stage (carries xcaddy + Go toolchain). Pin to `caddy:<v>-builder` to also pin the xcaddy CLI version. |

Operational notes:

- **Each host needs egress to `proxy.golang.org`** (xcaddy fetches Go modules during the build). Air-gapped hosts will fail at build time.
- **First `up` is slow** on each fresh host (~2-5 min for the compile). Subsequent ones are no-ops until plugin set changes.
- **Plugin rotation leaves stale images.** When you change plugins the new build is tagged `yoink-caddy:<new-hash>` and the old `yoink-caddy:<old-hash>` lingers. Run `docker image prune -a` on the host (or use the TUI's image-prune gesture) to reclaim. Stale builds are labelled `yoink.caddy.xcaddy_hash=...` for human inspection.
- **Caddyfile snippets and plugin directives don't mix.** `caddy_extra_caddyfile:` adapts via the bundled `caddy:2` adapter on the operator's machine, which doesn't know plugin-provided directives like `rate_limit { ... }`. If your snippet uses one, write it as `caddy_extra_json:` instead. See the [snippets cookbook](#snippets-cookbook) below for the JSON shapes.
- **`yoink validate` skips the docker-spawn check** when `xcaddy:` is set (the image only exists on hosts). The pure rendering path still runs and surfaces schema errors; Caddy refuses bad configs at `/load` time on the host.
- **Debug:** `yoink proxy-dockerfile` prints the synthesized two-stage Dockerfile without running docker. Useful for code review or pinning a Dockerfile in CI.

### `proxy.tls:` block

When set, all routed services use the same TLS config by default. Per-service `tls_cert_secret:` / `tls_key_secret:` overrides for the rare different-cert-per-service case. ACME is implicitly off and a `:80 → :443` redirect is auto-emitted.

| Field | Type | Default | Notes |
|---|---|---|---|
| `cert_secret` | string | required | Sealed-secret name holding the PEM cert (full chain). |
| `key_secret` | string | required | Sealed-secret name holding the PEM private key. |
| `client_auth` | block | — | Optional mTLS (e.g. Cloudflare origin-pull). |
| `client_auth.mode` | enum | `require_and_verify` | `request` / `require` / `verify_if_given` / `require_and_verify`. |
| `client_auth.trust_pool_secret` | string | required (when `client_auth` set) | Sealed-secret name holding the trust-pool CA bundle (PEM). |

Common shape:

```yaml
proxy:
  tls:
    cert_secret: CF_ORIGIN_CERT
    key_secret:  CF_ORIGIN_KEY
    client_auth:
      mode: require_and_verify
      trust_pool_secret: CF_ORIGIN_PULL_CA
```

This is the Cloudflare-style "every route is an origin behind Cloudflare's edge with mTLS proving the request came from Cloudflare" pattern. No host-filesystem cert state — everything lives in `secrets.age` or via `provider: command`, encrypted at rest.

That's the full surface. Five service-level fields, four proxy-level fields. Anything beyond that is `caddy_extra_json:` / `caddy_extra_caddyfile:` (next section) or a custom proxy image.

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

## Snippets cookbook

`caddy_extra_json:` (and `caddy_extra_caddyfile:`) are the escape hatch for Caddy features that aren't deploy primitives — auth, rate limiting, headers, redirects, IP allowlists, body limits. Yoink models the routing graph; Caddy models the traffic handling.

The cookbook of ready-to-paste snippets lives in its own page: [Caddy snippets cookbook](/docs/how-to/caddy-snippets) — forward auth, basic auth, IP allowlist, custom headers, body-size limit, redirects, maintenance page, plugins. Each snippet shows both the Caddyfile and JSON forms.


## What yoink doesn't model

Auth, rate limiting, headers, CORS, redirects, mTLS, geo-blocking, WAF, L4/TCP routing — none of these are in yoink's schema. They live in `caddy_extra_json:` using Caddy's [JSON config syntax](https://caddyserver.com/docs/json/apps/http/servers/routes/handle/), which is what Caddy's docs use directly. The escape hatch is the *advertised* path for these — yoink models deploy primitives, Caddy models traffic handling.

## See also

- [Cloudflare Origin Certificates](/docs/how-to/cloudflare-origin-certs) — skip Let's Encrypt entirely with a 15-year cert from Cloudflare, sealed in the repo.
- [Multi-host LE with Redis storage](/docs/how-to/multi-host-redis-storage) — share ACME state across hosts so they don't all hit Let's Encrypt rate limits.
- [Caddy JSON config reference](https://caddyserver.com/docs/json/) — for `caddy_extra_json:` snippets.
- [Caddyfile docs](https://caddyserver.com/docs/caddyfile) — for `caddy_extra_caddyfile:` syntax.
