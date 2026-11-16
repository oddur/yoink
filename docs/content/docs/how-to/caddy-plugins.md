---
title: Caddy plugins (xcaddy, no registry)
description: Build a custom Caddy binary with rate-limit, l4, redis-storage, and third-party DNS providers on each host, with no registry.
weight: 6
---

The bundled `caddy:2` image does not contain plugins such as rate limiting, the L4 module, third-party DNS providers, or shared ACME storage. With `proxy.xcaddy:`, each proxy host builds a custom caddy binary with [`xcaddy`](https://github.com/caddyserver/xcaddy) into its local image cache. **A registry is not necessary.**

For the design of the build, see [How yoink builds Caddy plugins](/docs/explanation/xcaddy-builds). For the image tags, the rebuild rules, and a list of plugins, see the [xcaddy build reference](/docs/reference/xcaddy).

## Prerequisites

- `proxy.image:` is not set. `proxy.image:` and `proxy.xcaddy:` are mutually exclusive.
- Each proxy host has egress to `proxy.golang.org` and `sum.golang.org`. xcaddy uses `go get`, which connects to these hosts by default.

A host without this egress fails at compile time with an opaque Go error. Open the egress, or set `GOPROXY` and `GOSUMDB` in `proxy.xcaddy.env:` to point at a proxy that the host can reach.

## Add plugins to the proxy

### List the plugins under `proxy.xcaddy:` [step]

Write one entry for each caddy module. To pin a version, use the `module@version` form.

```yaml
proxy:
  email: ops@example.com
  xcaddy:
    plugins:
      - github.com/caddyserver/caddy-l4
      - github.com/mholt/caddy-ratelimit@v0.1.0      # `module@version` to pin
      - github.com/caddy-dns/cloudflare
```

### Deploy [step]

Run `yoink up`. The first build on a host takes approximately 2-5 minutes for typical plugin sets. Subsequent `yoink up` runs on that host use the cached image. To see the build output, see [Watch a live build](#watch-a-live-build).

### Make sure that the plugin is in the binary [step]

After `yoink up`, run this command on the proxy host:

```sh
docker run --rm --entrypoint /usr/bin/caddy yoink-caddy:<hash> list-modules
```

Make sure that the output contains the module of the plugin, for example `http.handlers.rate_limit`.

## Choose where the plugin config goes

The upstream README of each plugin documents the shape of its config. The table shows where that config goes.

| Scope of the plugin | Config key |
|---|---|
| One service | `caddy_extra_json:` on the service |
| **Every** request (CrowdSec, Coraza, fleet-wide rate limiting) | `proxy.global_handlers:`. yoink adds the handlers one time, and they apply automatically to new services. |
| Top-level Caddy app config (storage, `trusted_proxies` at the Caddy server level) | `proxy.config_extra:` |

The [reverse proxy page](/docs/explanation/proxy) describes `proxy.global_handlers:` and `proxy.config_extra:`.

> ⚠ **Server-scoped config goes under `apps.http.servers.main`, not `srv0`.** `main` is the name that yoink gives the Caddy server. The upstream docs use the Caddy convention `srv0`. `srv0` makes a second server config that never listens. The config then has no effect and gives no error.

## Use a plugin directive in a snippet

Write plugin directives in `caddy_extra_json:`, not in `caddy_extra_caddyfile:`. yoink adapts `caddy_extra_caddyfile:` snippets on your computer with the stock `caddy:2` image. That adapter does not know the directives that plugins supply. Thus this snippet **does not work**:

```yaml
caddy_extra_caddyfile: |
  rate_limit {  # ← rate_limit is a plugin directive
    zone per_ip { key {remote_host}; events 100; window 1m }
  }
```

The result is a `caddy adapt` error such as `unrecognized directive: rate_limit`. Write the snippet as `caddy_extra_json:` instead:

```yaml
caddy_extra_json: |
  [{
    "handler": "rate_limit",
    "zones": {"per_ip": {"key": "{remote_host}", "events": 100, "window": "1m"}}
  }]
```

`caddy_extra_json:` does not use the adapter. Caddy validates the JSON at `/load` time on the host, where the binary contains the plugin. For the JSON shapes, see the [Caddy snippets cookbook](/docs/how-to/caddy-snippets).

## Common plugin recipes

### Rate limiting

Add the plugin. Then add the handler with `caddy_extra_json:` on each service.

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

[caddyserver/cache-handler](https://github.com/caddyserver/cache-handler) does RFC-compliant HTTP caching at the proxy. The key is the URL and the `Vary` headers, and the plugin obeys the `Cache-Control` of the origin. To start caching in memory, add the plugin and one handler on each route.

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

Responses that carry `Cache-Control: public, max-age=…` get the benefit immediately. To decide if a proxy cache is useful for a service, see [When a proxy cache helps](/docs/explanation/xcaddy-builds#when-a-proxy-cache-helps).

The in-memory backend is local to each proxy instance. For a cache that many hosts share, or that stays after a proxy restart, use the Redis, Badger, or etcd backend. Put the top-level `cache` app block in `proxy.config_extra:`.

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

yoink deep-merges this block into the generated Caddy config. The `apps.cache` block does not overwrite the routes of yoink.

### Multi-host LE certs (`caddy-storage-redis`)

To share the ACME state of many proxies through Redis, see [Multi-host Let's Encrypt with Redis storage](/docs/how-to/multi-host-redis-storage).

### Layer-4 (TCP/UDP) routing

The L4 module of Caddy forwards raw TCP and UDP, for example Postgres, SSH, and custom protocols. yoink does not model L4 routes. Add the plugin with `proxy.xcaddy:`. Then write the `layer4` app config yourself in `proxy.config_extra:`. The proxy container publishes only ports 80 and 443.

```yaml
proxy:
  xcaddy:
    plugins:
      - github.com/caddyserver/caddy-l4
```

### Third-party DNS providers (DNS-01 challenges, wildcards)

Wildcard certs need the DNS-01 challenge. Caddy supports DNS-01 only through a plugin for each provider. Select one from the [caddy-dns directory](https://github.com/caddy-dns):

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

Configure the DNS challenge as the README of the provider documents. This config is top-level Caddy config, so it goes in `proxy.config_extra:`. yoink does not resolve sealed secrets inside `config_extra:` or `caddy_extra_json:`.

## Force a rebuild

A new release of caddy or of an unpinned plugin does not start a rebuild. The [rebuild rules](/docs/reference/xcaddy#rebuild-rules) list the config changes that start one. To rebuild with no config change, run this command:

```sh
yoink up --rebuild-proxy
```

Every proxy host then pulls the builder image, runs the build again, and tags the image again.

## Remove stale images

When the plugin set changes, yoink tags the new build `yoink-caddy:<new-hash>`. The old `yoink-caddy:<old-hash>` stays in the image cache of the host, because yoink does not prune old images. The current image is the one that the running `yoink-proxy-*` container uses. All other `yoink-caddy` images are stale. To remove them, run these commands:

```sh
# On each host:
docker images yoink-caddy        # list all xcaddy builds
docker image prune -a            # remove anything unreferenced (broad)
docker rmi yoink-caddy:<old-hash>  # surgical
```

As an alternative, prune images from the Resources pane of the TUI.

## Debugging

### Print the synthesized Dockerfile

```sh
yoink proxy-dockerfile
```

This command makes no docker calls. Use the output for code review, to pin a Dockerfile in CI, or as input to `docker build -`.

### Watch a live build

```sh
RUST_LOG=debug yoink up
```

yoink logs each line of the build stream at debug level: the Go module fetches, the compile progress, and the final tag step. At the default log level, the terminal shows nothing during the compile.

### Build failed

yoink shows xcaddy errors verbatim in the deploy error. These are the common errors:

| Error | Cause | Correction |
|---|---|---|
| **`The command '/bin/sh -c xcaddy build ...' returned a non-zero code: 1`** | An xcaddy compile failure. The usual cause is an incompatible plugin version or a clash of transitive module pins. | Run the command again with `RUST_LOG=debug` to see the Go error. |
| **DNS or network errors during `go get`** | The host cannot reach `proxy.golang.org`. | Correct the egress of the host, or set `GOPROXY` in `proxy.xcaddy.env:`. |
| **`module not found`** | A typo in the module path, or a private module without `GOPRIVATE`. | Correct the module path, or set `GOPRIVATE` in `proxy.xcaddy.env:`. |

## See also

- [How yoink builds Caddy plugins](/docs/explanation/xcaddy-builds): the per-host build model, its costs, and its trade-offs.
- [xcaddy build reference](/docs/reference/xcaddy): image tags, labels, rebuild rules, the generated Dockerfile, and a plugin list.
- [Config reference](/docs/reference/config#xcaddy): the fields of `proxy.xcaddy:`.
- [Defense-in-depth web serving](/docs/how-to/defense-in-depth): a Cloudflare, CrowdSec, and Coraza stack from the security plugins.
