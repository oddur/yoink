---
title: Multi-host Let's Encrypt with Redis storage
weight: 16
---

When you run yoink across multiple hosts that serve the same domain, **each host's Caddy independently asks Let's Encrypt for a cert and you hit rate limits within a week**. The fix: share ACME state across all proxies via a small Redis instance reachable on a private network.

If you're using Cloudflare's edge, the simpler answer is [Origin Certificates](/docs/recipes/cloudflare-origin-certs) — no ACME at all. This recipe is for the LE-direct case.

## Architecture

```
host-1 ──┐
host-2 ──┤── all reach ──→ redis:6379 (on tailnet) ──→ shared ACME state
host-3 ──┘
```

- A **single Redis** instance, deployed by yoink onto one canonical host.
- All proxies use the [`caddy-storage-redis`](https://github.com/pberkel/caddy-storage-redis) plugin to point Caddy's storage at Redis instead of the local `/data` volume.
- **Tailscale** (or any private network) keeps Redis off the public internet — the redis port is published only on the tailnet IP.

The single Redis is a small SPOF, but a tractable one (it's only used for cert issuance/renewal, not request-path traffic). Backups via tailnet-replicated snapshots if you need it.

## Bake the plugin into caddy

`caddy-storage-redis` isn't in the base `caddy:2` image. The plugin part is straightforward — `proxy.xcaddy:` does the build on every proxy host:

```yaml
proxy:
  email: ops@example.com
  xcaddy:
    plugins:
      - github.com/pberkel/caddy-storage-redis
```

That gets the storage *module* compiled into the caddy binary on each host. Wiring caddy to actually *use* Redis as its storage backend takes one more step — see the [Wire caddy's storage backend](#wire-caddys-storage-backend) section below.

See [Caddy plugins (xcaddy, no registry)](/docs/recipes/caddy-plugins) for the full xcaddy story (build cost, idempotency, debugging).

## Run Redis as a yoink service

Pin Redis to one host and publish only on the tailnet IP:

```yaml
hosts:
  - { address: prod-1, user: deploy }
  - { address: prod-2, user: deploy }
  - { address: prod-3, user: deploy }

services:
  - name: redis
    image: redis
    tag: "7"
    hosts: [prod-1]                # one canonical host
    networks: [yoink-ingress]      # so the proxies can reach it by name from inside the network
    run:
      port: 6379
      healthcheck_path: /          # Redis doesn't speak HTTP; skip and let yoink TCP-probe
      healthcheck_path: null
      publish:
        - "100.10.0.1:6379:6379"   # tailnet IP only — adjust to your tailscale assignment
      volumes:
        - "redis-data:/data"
```

The published port on Tailscale's IP makes Redis reachable from `prod-2` and `prod-3` over the tailnet, but **not** from the public internet (Hetzner / your hosting provider's external interface).

## Wire caddy's storage backend

`proxy.xcaddy:` gets the plugin compiled in. Telling caddy to *use* Redis instead of the local `/data` volume needs a top-level `storage` block in caddy's config — and yoink doesn't yet expose that as a typed field.

Until yoink ships a `proxy.storage:` field, the practical path is to drop `proxy.xcaddy:` for this case and use `proxy.image:` with a hand-built image that bakes both the plugin compile and a storage-bootstrap entrypoint:

```dockerfile
# Dockerfile.caddy-redis
FROM caddy:2-builder AS builder
RUN xcaddy build --with github.com/pberkel/caddy-storage-redis

FROM caddy:2
COPY --from=builder /usr/bin/caddy /usr/bin/caddy
COPY caddy-bootstrap.json /etc/caddy/bootstrap.json
ENTRYPOINT ["caddy", "run", "--resume", "--config", "/etc/caddy/bootstrap.json"]
```

`caddy-bootstrap.json`:

```json
{
  "storage": {
    "module": "redis",
    "address": "redis:6379"
  },
  "admin": {"listen": ":2019"}
}
```

Build it once (locally or in CI), push to a registry your hosts can reach, and reference it from `proxy.image:`:

```yaml
proxy:
  image: ghcr.io/me/caddy-redis:2.7
  email: ops@example.com
```

Caddy's `--resume` flag loads the bootstrap config first; yoink's `/load` push then layers the rendered routing config on top. The storage block stays Redis-backed across reloads.

> Why not `proxy.xcaddy:` here? Because xcaddy bakes plugins in but not entrypoints or bootstrap files. The redis-storage path needs both. A future yoink release will likely add `proxy.storage:` as a typed field, at which point this whole section collapses to a one-line `proxy.xcaddy:` config. Track yoink issues for the cleaner path.

## Tailscale on every host

For the proxies on `prod-2` and `prod-3` to reach `redis:6379` on `prod-1`'s tailnet IP, every host needs to be on the tailnet. See [Pairing with Tailscale](/docs/guide/pairing) for the setup.

## See also

- [Reverse proxy guide](/docs/guide/proxy)
- [Cloudflare Origin Certificates](/docs/recipes/cloudflare-origin-certs) — simpler path if you're already on Cloudflare's edge.
- [`caddy-storage-redis`](https://github.com/pberkel/caddy-storage-redis) — the upstream plugin.
