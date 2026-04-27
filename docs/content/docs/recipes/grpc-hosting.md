---
title: gRPC hosting
weight: 9
---

Native gRPC backends speak HTTP/2. To route gRPC traffic from clients (which terminate at Caddy with TLS) all the way through to the backend, Caddy needs to dial the upstream as **HTTP/2 cleartext** (h2c). One field in yoink turns this on:

```yaml
services:
  - name: api
    image: ghcr.io/me/grpc-api
    tag: v1
    domain: api.example.com
    upstream_h2c: true            # ← that's it
    run:
      port: 50051
```

That's the whole opt-in. `yoink up`, point your gRPC client at `api.example.com:443`, done.

## How it works

Without `upstream_h2c:`, Caddy talks to the backend over HTTP/1.1. gRPC clients (which use HTTP/2 framing) send requests Caddy can't proxy correctly — you'll see `INTERNAL` errors or empty responses.

`upstream_h2c: true` renders an HTTP/2-cleartext transport on the auto-generated `reverse_proxy` handler:

```json
{
  "handler": "reverse_proxy",
  "transport": {"protocol": "http", "versions": ["h2c"]},
  "upstreams": [{"dial": "api:50051"}]
}
```

Caddy's TLS termination on the public side stays HTTP/2 (h2 with ALPN). The h2c is purely how Caddy talks to your container on the internal docker network.

## Mixed gRPC + HTTP/1.1 backends

Servers that handle both gRPC and plain HTTP on the same port (Tonic + Axum, grpc-go + http.Handler, grpc-java + servlet) usually serve HTTP/1.1 requests over h2c just fine — HTTP/2 carries HTTP/1.1 semantics natively. **A single `upstream_h2c: true` covers both protocols** for these servers.

If your backend rejects HTTP/1.1 over h2c (some older grpc-only servers do), you can still serve mixed traffic by routing only gRPC requests through h2c and everything else through HTTP/1.1, using `caddy_extra_caddyfile:`:

```yaml
services:
  - name: api
    image: ghcr.io/me/api
    domain: api.example.com
    caddy_extra_caddyfile: |
      @grpc header Content-Type application/grpc*
      reverse_proxy @grpc {
        to api:50051
        transport http {
          versions h2c
        }
      }
    # Yoink's auto-generated reverse_proxy (without h2c) handles
    # everything else.
    run:
      port: 50051
```

## gRPC-Web

Browsers can't speak native gRPC — they need [gRPC-Web](https://github.com/grpc/grpc-web), which uses HTTP/1.1 or HTTP/2 (no h2c required) plus a translation layer. Two options:

- **Translation in your backend**: Tonic has [`tonic-web`](https://docs.rs/tonic-web/), grpc-go has [improbable-eng/grpc-web](https://github.com/improbable-eng/grpc-web/tree/master/go/grpcwebproxy). Backend speaks both gRPC and gRPC-Web on the same port. `upstream_h2c: true` on yoink covers both because gRPC-Web traffic is regular HTTP/1.1 over h2c.
- **Caddy plugin**: there's a `caddy-grpc-web` plugin that translates at the proxy layer. Requires an [`xcaddy`](https://github.com/caddyserver/xcaddy)-built image; set `proxy.image:` to your build.

## Reflection / grpcurl

`grpcurl` works against `api.example.com:443` because it speaks gRPC over TLS. Verify with:

```sh
grpcurl api.example.com:443 list
grpcurl api.example.com:443 grpc.health.v1.Health/Check
```

If your backend has reflection enabled, the `list` should print your service descriptors. If reflection is off, pass `-proto path/to/your.proto` instead.

## Mutual TLS (Cloudflare origin-pull)

If you're fronting via Cloudflare, the [Cloudflare Origin Certificates recipe](/docs/recipes/cloudflare-origin-certs) covers the mTLS setup that locks your origin to Cloudflare's edge IPs. gRPC + mTLS works the same way — `upstream_h2c: true` + `proxy.tls.client_auth` together:

```yaml
proxy:
  tls:
    cert_secret: CF_ORIGIN_CERT
    key_secret:  CF_ORIGIN_KEY
    client_auth:
      mode: require_and_verify
      trust_pool_secret: CF_ORIGIN_PULL_CA

services:
  - name: api
    image: ghcr.io/me/grpc-api
    domain: api.example.com
    upstream_h2c: true
    run:
      port: 50051
```

This is the configuration backtrack uses today: Tonic + Axum behind Cloudflare with origin-pull mTLS, single `upstream_h2c: true` covers both gRPC and the REST surface.

## See also

- [Reverse proxy guide](/docs/guide/proxy) — full schema reference.
- [Cloudflare Origin Certificates](/docs/recipes/cloudflare-origin-certs) — for the mTLS edge story.
- [Caddy snippets cookbook](/docs/recipes/caddy-snippets) — for content-type matchers, request body limits, and other escape-hatch patterns.
- [Caddy reverse_proxy docs](https://caddyserver.com/docs/caddyfile/directives/reverse_proxy#transport) — the transport options for the underlying primitive.
