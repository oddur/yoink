---
title: xcaddy build reference
description: Image tags, labels, rebuild rules, and the generated Dockerfile of the proxy.xcaddy build, with a list of Caddy plugins.
---

## Config

The fields of `proxy.xcaddy:` are in the [proxy reference](/docs/reference/proxy#proxyxcaddy-block-caddy-plugins-without-a-registry) and the [config reference](/docs/reference/config#xcaddy).

`proxy.xcaddy:` and `proxy.image:` are mutually exclusive. `image:` uses an image that you supply. `xcaddy:` uses a build that yoink manages.

## Image tag and labels

| Item | Value |
|---|---|
| Local tag | `yoink-caddy:<hash>` |
| Hash inputs | `(caddy_version, base_image, builder_image, sorted plugins, sorted env, sorted replace)` |
| Label | `yoink.caddy.xcaddy_hash=<hash>` |
| Label | `yoink.caddy.plugins=<csv>` |
| Build location | The image cache of each proxy host |

## Rebuild rules

Two `yoink up` runs give the same `yoink-caddy:<hash>` tag if and only if all hash inputs are the same. yoink sorts `plugins`, `env`, and `replace` before the hash, so their order has no effect. If the host has the tag, yoink skips the build.

| Change | Hash changes? | Effect |
|---|---|---|
| Reorder `plugins:` entries in the YAML | No | No-op |
| Add or remove a plugin | Yes | Fresh build on the next `up` |
| Change a `module@version` pin (for example `@v0.1.0` → `@v0.2.0`) | Yes | Fresh build |
| Remove the `@version` from a pinned plugin | Yes | Fresh build (different string ≠ same module) |
| Pin `caddy_version: v2.8.4` | Yes | Fresh build |
| Add or change an `env` or `replace` entry | Yes | Fresh build |
| New caddy or plugin release upstream (no config change) | No | yoink uses the cached build. `yoink up --rebuild-proxy` forces a rebuild. |

## Generated Dockerfile

`yoink proxy-dockerfile` prints the Dockerfile and makes no docker calls. The Dockerfile has two stages: `caddy:2-builder` → `caddy:2`. Each `env` entry adds an `ENV` line to the builder stage. Each `replace` entry adds a `--replace` argument. yoink pulls the builder image before each build.

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

## Plugins

yoink does not model these features natively. This list is not complete: the [caddy module index](https://caddyserver.com/download) lists every published module.

| Plugin | Function | Notes |
|---|---|---|
| [`caddyserver/cache-handler`](https://github.com/caddyserver/cache-handler) | RFC-compliant HTTP caching at the proxy. | Not useful behind a CDN edge. [Recipe](/docs/how-to/caddy-plugins#http-caching-caddyservercache-handler). |
| [`mholt/caddy-ratelimit`](https://github.com/mholt/caddy-ratelimit) | In-process rate limiting for each IP, header, or zone. | A second layer behind the limiter of a CDN, or the only layer when the proxy is the public surface. [Recipe](/docs/how-to/caddy-plugins#rate-limiting). |
| [`mholt/caddy-l4`](https://github.com/mholt/caddy-l4) | Layer-4 (TCP/UDP) routing: Postgres, SSH, custom protocols, and gRPC with mTLS passthrough. | yoink does not model L4 routes. The plugin compiles with xcaddy, but you must write the Caddy admin pushes for the routes. |
| [`greenpau/caddy-security`](https://github.com/greenpau/caddy-security) | SSO/AAA at the proxy: JWT validation, OAuth2 / OIDC, SAML, LDAP, and authorization policies for each route. | Auth at the proxy without an extra service. Works with `forward_auth` (in core caddy), or with the `authenticate` and `authorize` directives of the plugin. |
| [`corazawaf/coraza-caddy`](https://github.com/corazawaf/coraza-caddy) | ModSecurity-compatible Web Application Firewall, with the OWASP Core Rule Set. | For a proxy behind a CDN that has no WAF, or as a second layer. Heavier than the other plugins in this list: do a benchmark before you use it on hot endpoints. |
| [`hslatman/caddy-crowdsec-bouncer`](https://github.com/hslatman/caddy-crowdsec-bouncer) | Enforces the community IP blocklists of [CrowdSec](https://www.crowdsec.net/) at the proxy. | Needs a CrowdSec local API instance that the proxy can reach. That instance can run as a yoink service. |
| [`caddy-dns/*`](https://github.com/caddy-dns) | DNS-01 ACME challenge providers (cloudflare, route53, digitalocean, hetzner, and more). | Required for wildcard certs. [Recipe](/docs/how-to/caddy-plugins#third-party-dns-providers-dns-01-challenges-wildcards). |
| [`pberkel/caddy-storage-redis`](https://github.com/pberkel/caddy-storage-redis) | Shared ACME storage backend for fleets of many hosts. | [Recipe](/docs/how-to/multi-host-redis-storage). |
| [`WeidiDeng/caddy-cloudflare-ip`](https://github.com/WeidiDeng/caddy-cloudflare-ip) | Trust source for the Cloudflare IP ranges, with automatic refresh. | Behind Cloudflare, it passes the real client IP to logs, geo-IP, and IP-aware rate limits. Works with `trusted_proxies` in `proxy.config_extra:`. [Recipe](/docs/how-to/cloudflare-origin-certs#real-client-ip-from-cf-connecting-ip). |

## See also

- [Caddy plugins (xcaddy, no registry)](/docs/how-to/caddy-plugins): the procedure to add, rebuild, and debug plugins.
- [How yoink builds Caddy plugins](/docs/explanation/xcaddy-builds): why the build runs on each host and why the hash decides rebuilds.
- [Proxy config](/docs/reference/proxy#proxyxcaddy-block-caddy-plugins-without-a-registry): all fields of `proxy.xcaddy:`.
- [Defense-in-depth web serving](/docs/how-to/defense-in-depth): a stack that uses the security plugins.
