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
- **Set `RUST_LOG=info`** to see live build output. Yoink forwards bollard's build stream to `tracing::info!`. Without it, the operator stares at a blank terminal during the compile.
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
RUST_LOG=info yoink up
```

The build stream is forwarded line-by-line through `tracing::info!`. You'll see Go module fetches, compile progress, and the final tag step.

### Build failed

xcaddy errors are surfaced verbatim through the deploy error chain. Common shapes:

- **`The command '/bin/sh -c xcaddy build ...' returned a non-zero code: 1`** — xcaddy compile failure. Re-run with `RUST_LOG=info` to see the actual Go error. Most often an incompatible plugin version or a transitive module pin clash.
- **DNS / network errors during `go get`** — host can't reach `proxy.golang.org`. Fix host egress or override `GOPROXY` in a custom `builder_image:`.
- **`module not found`** — typo in the module path, or you're using a private module without `GOPRIVATE` set on the builder.

## See also

- [Reverse proxy guide](/docs/guide/proxy) — full schema reference for the routing primitives yoink models, including the `proxy.xcaddy:` field table.
- [Caddy snippets cookbook](/docs/recipes/caddy-snippets) — JSON shapes for `caddy_extra_json:` (the path you need when your snippet uses plugin directives).
- [Multi-host Let's Encrypt with Redis](/docs/recipes/multi-host-redis-storage) — the canonical "I need a plugin" worked example.
- [xcaddy on GitHub](https://github.com/caddyserver/xcaddy) — upstream tool docs.
- [caddy-dns providers](https://github.com/caddy-dns) — directory of DNS-provider plugins for DNS-01 / wildcard certs.
