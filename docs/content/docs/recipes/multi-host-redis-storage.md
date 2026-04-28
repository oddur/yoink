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

## Build a custom Caddy image with the storage plugin

`caddy-storage-redis` isn't in the base `caddy:2` image. Build one with [`xcaddy`](https://github.com/caddyserver/xcaddy):

```dockerfile
# Dockerfile.caddy-redis
FROM caddy:2-builder AS builder
RUN xcaddy build \
    --with github.com/pberkel/caddy-storage-redis

FROM caddy:2
COPY --from=builder /usr/bin/caddy /usr/bin/caddy
```

```sh
docker buildx build --platform linux/amd64,linux/arm64 \
  -f Dockerfile.caddy-redis \
  -t ghcr.io/me/caddy-redis:2.7 \
  --push .
```

(Or use `yoink build` against a service that wraps this Dockerfile, then `proxy.image:` it.)

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

## Point Caddy at Redis

Use the custom image, and add the storage block via `caddy_extra_json:` at the proxy level — wait, this doesn't exist as a field yet. For v1, the cleanest path is to bake the storage config into a custom `Caddyfile` baked into the image, OR use a small `proxy.config_extra:` field if/when that lands.

For v1 today, the practical path: build the xcaddy image with the storage config baked in via a custom entrypoint that writes the storage block before `caddy run`. Example wrapper:

```dockerfile
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

Caddy's `--resume` flag loads this bootstrap config first; yoink's `/load` then merges in the rendered routing config on top. Storage stays Redis-backed.

A future yoink release will likely add `proxy.storage:` as a typed field; until then, the bootstrap-image trick is the working path.

## Tailscale on every host

For the proxies on `prod-2` and `prod-3` to reach `redis:6379` on `prod-1`'s tailnet IP, every host needs to be on the tailnet. See [Pairing with Tailscale](/docs/guide/pairing) for the setup.

## See also

- [Reverse proxy guide](/docs/guide/proxy)
- [Cloudflare Origin Certificates](/docs/recipes/cloudflare-origin-certs) — simpler path if you're already on Cloudflare's edge.
- [`caddy-storage-redis`](https://github.com/pberkel/caddy-storage-redis) — the upstream plugin.
