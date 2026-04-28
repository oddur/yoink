---
title: Caddy plugins (xcaddy, no registry)
weight: 6
---

The bundled `caddy:2` image is small and well-curated, but it doesn't carry plugins like rate-limiting, the L4 module, third-party DNS providers, or shared ACME storage. The standard fix is [`xcaddy`](https://github.com/caddyserver/xcaddy) — caddy's own build tool that fetches modules from Go and statically links them into a custom binary.

Yoink wraps that flow into one config block. List the plugins you want under `proxy.xcaddy:`, run `yoink up`, and each proxy host runs a one-shot xcaddy build into its local image cache. **No registry needed** — important if you're running fully behind a tailnet or just don't want to maintain a registry for one image.

```yaml
proxy:
  email: ops@example.com
  xcaddy:
    plugins:
      - github.com/caddyserver/caddy-l4
      - github.com/mholt/caddy-ratelimit@v0.1.0      # `module@version` to pin
      - github.com/caddy-dns/cloudflare
```

That's the whole feature. The rest of this page covers what's happening, when it rebuilds, how to debug, and the rough edges.

## What yoink does

On the next `yoink up`, before the regular service pull loop runs, yoink fans out across every host the proxy will run on and:

1. Computes a content hash over `(caddy_version, base_image, builder_image, sorted plugin list)` and resolves the local tag `yoink-caddy:<hash>`.
2. Asks the host's docker daemon if that tag is already cached. If yes, skip — the entire xcaddy step is a no-op.
3. If no, synthesizes a two-stage Dockerfile (`caddy:2-builder` → `caddy:2`), packs it into an in-memory tar context, and streams it to the host's docker daemon as a `POST /build` (over the same SSH transport yoink already uses for everything else).
4. Tags the resulting image as `yoink-caddy:<hash>` on that host. Adds `yoink.caddy.xcaddy_hash=<hash>` and `yoink.caddy.plugins=<csv>` labels so you can identify it from `docker images` later.

The proxy service then deploys from the local tag. Subsequent `yoink up` invocations short-circuit at step 2 and run in seconds.

The Dockerfile yoink renders is small enough to read end-to-end. Run `yoink proxy-dockerfile` to print it without touching docker:

```dockerfile
# syntax=docker/dockerfile:1
FROM caddy:2-builder AS builder
RUN xcaddy build \
    --with github.com/caddy-dns/cloudflare \
    --with github.com/caddyserver/caddy-l4 \
    --with github.com/mholt/caddy-ratelimit@v0.1.0

FROM caddy:2
COPY --from=builder /usr/bin/caddy /usr/bin/caddy
LABEL yoink.caddy.xcaddy_hash=763762748729e677
LABEL yoink.caddy.plugins="github.com/caddy-dns/cloudflare,github.com/caddyserver/caddy-l4,github.com/mholt/caddy-ratelimit@v0.1.0"
```

## When yoink rebuilds (and when it doesn't)

The hash is the contract. Two `yoink up` runs produce the same `yoink-caddy:<hash>` tag iff:

- The plugin list is the same (order doesn't matter — yoink sorts before hashing).
- `caddy_version` matches.
- `base_image` and `builder_image` match.

So:

| Change | Hash flips? | Effect |
|---|---|---|
| Reorder `plugins:` entries in the YAML | No | No-op |
| Add or remove a plugin | Yes | Fresh build on next `up` |
| Change `module@version` pin (e.g. `@v0.1.0` → `@v0.2.0`) | Yes | Fresh build |
| Drop the `@version` from a pinned plugin | Yes | Fresh build (different string ≠ same module) |
| Pin `caddy_version: v2.8.4` | Yes | Fresh build |
| New caddy release upstream (no config change) | No | Cached build still used — bump `caddy_version` or set `xcaddy:` from `unset` to a pinned version to force a rebuild |

The "no automatic refresh on upstream caddy release" trade-off is intentional: deterministic builds beat surprise rebuilds. If you want to track caddy's latest, run `docker rmi yoink-caddy:<hash>` on each host (or use the TUI's image-prune gesture) and `yoink up` will rebuild against whatever xcaddy picks up at that moment.

## Common plugin recipes

### Rate limiting

Pair with `caddy_extra_json:` per service (the `rate_limit` directive isn't in core caddy, so the `caddy_extra_caddyfile:` adapter can't parse it — see the [snippets vs plugin directives](#snippets-vs-plugin-directives) note below):

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
        "zones": {
          "per_ip": {"key": "{remote_host}", "events": 100, "window": "1m"}
        }
      }]
    run: { port: 8080 }
```

### HTTP caching (`caddyserver/cache-handler`)

Cache responses at the proxy layer using [caddyserver/cache-handler](https://github.com/caddyserver/cache-handler) — RFC-compliant HTTP caching keyed by URL + `Vary` headers, honoring `Cache-Control` from the origin. The default in-memory backend ships sane defaults so a single-line plugin add and a per-route handler is enough to start caching.

```yaml
proxy:
  email: ops@example.com
  xcaddy:
    plugins:
      - github.com/caddyserver/cache-handler

services:
  - name: marketing
    domain: www.example.com
    caddy_extra_json: |
      [{"handler": "cache"}]
    run: { port: 8080 }
```

That gets you in-process caching with the upstream's `Cache-Control` headers as the source of truth. Marketing/blog HTML, static asset proxies, and APIs that emit `Cache-Control: public, max-age=…` all benefit immediately.

**When you want caching:**

- High-fanout responses (homepage, marketing pages, public API endpoints) where origin recompute is expensive.
- Slow origins (Rails/Django/PHP rendering) sitting behind Caddy on the same host or a tailnet hop away.
- Burst protection in front of a database-bound endpoint — even short TTLs (5-30s) collapse traffic spikes into one origin hit.
- Asset proxies where the upstream is a slow blob store.

**When you don't:**

- **You're already on Cloudflare / Fastly / Bunny / a CDN edge** — they cache ahead of your origin proxy. Adding cache-handler is duplicated work for the same response, with worse hit rates because each yoink host has its own cold cache. Skip it. The exception is cache-warmup for purge events (your proxy keeps the response when the CDN re-fetches), but that's niche enough to defer.
- **Responses are highly personalized** (per-user dashboards, signed-in feeds). Either the origin sets `Cache-Control: private` (and the cache handler skips them) or you carefully configure cache keys with `Vary: Authorization` — gets fragile fast.
- **Strong consistency** is a hard requirement (e.g., financial state). Caching at the proxy adds a delay between writes and reads.

**Advanced backends (Redis, Badger, etcd):**

The default in-memory backend is per-proxy-instance, no shared state. For multi-host shared cache or persistent cache across proxy restarts, the plugin supports Redis/Badger/etcd backends via a top-level `cache` app block. That goes through [`proxy.config_extra:`](/docs/guide/proxy#proxyconfig_extra-block--global-caddy-config-escape-hatch):

```yaml
proxy:
  email: ops@example.com
  xcaddy:
    plugins:
      - github.com/caddyserver/cache-handler
  config_extra: |
    {
      "apps": {
        "cache": {
          "redis": {"url": "redis://redis:6379"}
        }
      }
    }
```

Yoink deep-merges this into the rendered Caddy config; the `apps.cache` block lands alongside `apps.http` without clobbering yoink's routes.

### Multi-host LE certs (`caddy-storage-redis`)

Share ACME state via Redis so multiple proxies don't each hit Let's Encrypt's rate limits. See [Multi-host Let's Encrypt with Redis storage](/docs/recipes/multi-host-redis-storage) — same `proxy.xcaddy:` mechanism, plus a Redis service definition.

### Layer-4 (TCP/UDP) routing

Caddy's L4 module unlocks raw TCP/UDP forwarding (think Postgres, SSH, custom protocols). Yoink doesn't yet model L4 routes natively — bake the plugin in via `proxy.xcaddy:` and configure routes through `proxy.tls:` + custom Caddy admin pushes for now.

```yaml
proxy:
  xcaddy:
    plugins:
      - github.com/caddyserver/caddy-l4
```

### Third-party DNS providers (DNS-01 challenges, wildcards)

Caddy's auto-HTTPS supports the DNS-01 challenge (required for wildcard certs) only via per-provider plugins. Pick one:

```yaml
proxy:
  email: ops@example.com
  xcaddy:
    plugins:
      - github.com/caddy-dns/cloudflare
      # or:
      # - github.com/caddy-dns/route53
      # - github.com/caddy-dns/digitalocean
      # ... see https://github.com/caddy-dns
```

Wire the provider's API key as a sealed secret and reference it from `caddy_extra_json:` per service.

## Plugin landscape

A non-exhaustive map of caddy plugins worth knowing about — yoink doesn't model these natively, so the plugin route is the answer when you need them. Each is a one-line addition to `proxy.xcaddy.plugins:`; the upstream README documents the directive shape to drop into `caddy_extra_json:` per service. For plugins that should run on **every** request (CrowdSec, Coraza, fleet-wide rate limiting), use [`proxy.global_handlers:`](/docs/guide/proxy#proxyglobal_handlers-block--proxy-wide-middleware-chain) instead — yoink wires them once and they apply automatically as new services land. For plugins that need top-level Caddy app config (storage, server-level `trusted_proxies`, etc.), use [`proxy.config_extra:`](/docs/guide/proxy#proxyconfig_extra-block--global-caddy-config-escape-hatch).

- **[`caddyserver/cache-handler`](https://github.com/caddyserver/cache-handler)** — RFC-compliant HTTP caching at the proxy. See the worked recipe above. Skip if you're behind a CDN edge.
- **[`mholt/caddy-ratelimit`](https://github.com/mholt/caddy-ratelimit)** — In-process rate limiting per-IP, per-header, or per-zone. See the worked recipe above. Pair with a CDN's edge limiter for two-layer defense; use solo when the proxy is your only public surface.
- **[`mholt/caddy-l4`](https://github.com/mholt/caddy-l4)** — Layer-4 (TCP/UDP) routing. Forward Postgres, SSH, custom protocols, gRPC-with-mTLS-passthrough through caddy. Yoink doesn't model L4 routes natively, so wiring requires custom Caddy admin pushes — the plugin compiles in cleanly via xcaddy, but configuration is on you.
- **[`greenpau/caddy-security`](https://github.com/greenpau/caddy-security)** — SSO/AAA at the proxy: JWT validation, OAuth2 / OIDC, SAML, LDAP, per-route authorization policies. The "Authelia-as-a-plugin" option — useful when you want auth at the proxy without running an extra service. Pair with `forward_auth` (already supported in core caddy) or use the plugin's native `authenticate`/`authorize` directives for finer-grained policies.
- **[`corazawaf/coraza-caddy`](https://github.com/corazawaf/coraza-caddy)** — ModSecurity-compatible Web Application Firewall. OWASP Core Rule Set out of the box, request inspection, virtual-patching for known CVEs. Most useful behind a non-WAF CDN, or as defence-in-depth even if you have one. Heavy compared to the rest of this list; benchmark before turning it on for hot endpoints.
- **[`hslatman/caddy-crowdsec-bouncer`](https://github.com/hslatman/caddy-crowdsec-bouncer)** — Enforces [CrowdSec](https://www.crowdsec.net/) community-IP blocklists at the proxy. Cheap/free DDoS-bot and credential-stuffing mitigation when you're not behind a managed edge. Needs a CrowdSec local API instance reachable from the proxy (run it as another yoink service).
- **[`caddy-dns/*`](https://github.com/caddy-dns)** — DNS-01 ACME challenge providers (cloudflare, route53, digitalocean, hetzner, dozens more). Required for wildcard certs. See the worked recipe above.
- **[`pberkel/caddy-storage-redis`](https://github.com/pberkel/caddy-storage-redis)** — Shared ACME storage backend for multi-host fleets. See the [redis-storage recipe](/docs/recipes/multi-host-redis-storage).
- **[`WeidiDeng/caddy-cloudflare-ip`](https://github.com/WeidiDeng/caddy-cloudflare-ip)** — Auto-refreshing Cloudflare IP-range trust source. Drop in when running behind Cloudflare so the real client IP propagates correctly to logs, geo-IP, and IP-aware rate limits — pairs with `trusted_proxies` in `proxy.config_extra:`. See the [Origin Certs recipe](/docs/recipes/cloudflare-origin-certs#real-client-ip-from-cf-connecting-ip).

For the broader ecosystem, [caddy's own module index](https://caddyserver.com/download) lets you browse every published module.

## `proxy.xcaddy:` field reference

| Field | Type | Default | Notes |
|---|---|---|---|
| `plugins` | list of strings | required (non-empty) | One entry per caddy module. Bare path or `module@version` — same syntax as `xcaddy build --with`. Sorted alphabetically before hashing/rendering. |
| `caddy_version` | string | unset → xcaddy's latest tagged release | Caddy git tag (e.g. `v2.8.4`). Use a real tag from [caddyserver/caddy](https://github.com/caddyserver/caddy/tags). |
| `base_image` | string | `caddy:2` | Runtime stage of the Dockerfile (final `FROM`). |
| `builder_image` | string | `caddy:2-builder` | Builder stage — carries xcaddy + Go toolchain. Pin to `caddy:<v>-builder` to also pin the xcaddy CLI version. |

Mutually exclusive with `proxy.image:` — `image:` is the bring-your-own-image path; `xcaddy:` is the managed-build path. Pick one.

## Operational notes

### First-build cost

Each fresh host pays the full xcaddy compile (~2-5 minutes for typical plugin sets — Go toolchain download, module fetches, link). Subsequent `up`s on that host are no-ops. Three implications:

- **Adding a host to a fleet means that host pays the build cost once.** Fan-out is parallel across hosts so it's `max` not `sum`, but a fresh host's first deploy is slower than a cache-hit deploy.
- **Set `RUST_LOG=debug`** to see live build output. Yoink forwards bollard's build stream to `tracing::debug!`. Default INFO logging stays focused on deploy milestones; without DEBUG, the operator stares at a blank terminal during the compile.
- If you want **build-once, ship-to-many** instead of build-on-each-host, that's not what this feature does — it's the per-host model precisely because we don't assume a registry. If you do have a registry, hand-build with xcaddy and use `proxy.image:` directly.

### Each host needs egress to Go's module proxy

xcaddy uses `go get` under the hood, which hits `proxy.golang.org` and `sum.golang.org` by default. Air-gapped hosts will fail at compile time with an opaque Go error. Either:

- Open egress to those hosts (the simplest fix).
- Set `GOPROXY` / `GOSUMDB` via the environment in a custom `builder_image:` (advanced; out of scope here).

### Stale images linger

When the plugin set changes the new build is tagged `yoink-caddy:<new-hash>` and the old `yoink-caddy:<old-hash>` stays in the host's image cache. Yoink doesn't auto-prune them — disk reclamation is opt-in.

To clean up:

```sh
# On each host:
docker images yoink-caddy        # list all xcaddy builds
docker image prune -a            # remove anything unreferenced (broad)
docker rmi yoink-caddy:<old-hash>  # surgical
```

Or use the TUI's image-prune gesture from the Resources pane. The current image is the one referenced by the running `yoink-proxy-*` container; everything else is stale.

### Snippets vs plugin directives

`caddy_extra_caddyfile:` snippets get adapted to JSON on the operator's machine using the bundled `caddy:2` adapter — which doesn't know about plugin-provided directives. So this **doesn't work**:

```yaml
caddy_extra_caddyfile: |
  rate_limit {  # ← rate_limit is a plugin directive
    zone per_ip { key {remote_host}; events 100; window 1m }
  }
```

You'll get a `caddy adapt` error like `unrecognized directive: rate_limit`. The fix: write it as `caddy_extra_json:` instead — that path skips the adapter entirely, and Caddy itself validates the JSON at `/load` time on the host (which has the plugin baked in):

```yaml
caddy_extra_json: |
  [{
    "handler": "rate_limit",
    "zones": {"per_ip": {"key": "{remote_host}", "events": 100, "window": "1m"}}
  }]
```

A future yoink could adapt on a host instead of the operator to fix this; for now, `caddy_extra_json:` is the supported path for plugin-aware snippets. See [Caddy snippets cookbook](/docs/recipes/caddy-snippets) for the JSON-shape conventions.

### `yoink validate` skips the docker check

`yoink validate` normally spawns `docker run <proxy-image> caddy validate` on the operator's machine to catch schema errors before deploy. With `proxy.xcaddy:` set, the proxy image only exists on hosts — so the docker step is skipped with a clear message. The pure-render step still runs and surfaces schema errors; Caddy refuses bad configs at `/load` time on the host as a backstop.

## Debugging

### Print the synthesized Dockerfile

```sh
yoink proxy-dockerfile
```

No docker calls. Useful for code review, pinning a Dockerfile in CI, or hand-running `docker build -` against it.

### Verify the plugin made it into the binary

After `yoink up`, on the proxy host:

```sh
docker run --rm --entrypoint /usr/bin/caddy yoink-caddy:<hash> list-modules
```

Look for the module path printed by your plugin (e.g. `http.handlers.rate_limit`).

### Watch a live build

```sh
RUST_LOG=debug yoink up
```

The build stream is forwarded line-by-line through `tracing::debug!`. You'll see Go module fetches, compile progress, and the final tag step.

### Build failed

xcaddy errors are surfaced verbatim through the deploy error chain. Common shapes:

- **`The command '/bin/sh -c xcaddy build ...' returned a non-zero code: 1`** — xcaddy compile failure. Re-run with `RUST_LOG=debug` to see the actual Go error. Most often an incompatible plugin version or a transitive module pin clash.
- **DNS / network errors during `go get`** — host can't reach `proxy.golang.org`. Fix host egress or override `GOPROXY` in a custom `builder_image:`.
- **`module not found`** — typo in the module path, or you're using a private module without `GOPRIVATE` set on the builder.

## See also

- [Reverse proxy guide](/docs/guide/proxy) — full schema reference for the routing primitives yoink models, including the `proxy.xcaddy:` field table.
- [Caddy snippets cookbook](/docs/recipes/caddy-snippets) — JSON shapes for `caddy_extra_json:` (the path you need when your snippet uses plugin directives).
- [Defense-in-depth web serving](/docs/recipes/defense-in-depth) — composing the security plugins above into a Cloudflare + CrowdSec + Coraza stack.
- [Multi-host Let's Encrypt with Redis](/docs/recipes/multi-host-redis-storage) — the canonical "I need a plugin" worked example.
- [xcaddy on GitHub](https://github.com/caddyserver/xcaddy) — upstream tool docs.
- [caddy-dns providers](https://github.com/caddy-dns) — directory of DNS-provider plugins for DNS-01 / wildcard certs.
