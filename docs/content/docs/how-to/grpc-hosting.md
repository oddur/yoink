---
title: gRPC hosting
description: Expose a native gRPC backend (Tonic, grpc-go, grpc-java) with upstream_h2c, which also works for REST.
weight: 14
---

Native gRPC backends speak HTTP/2. Caddy terminates TLS and must then dial the backend as **HTTP/2 cleartext** (h2c). The `upstream_h2c:` field makes Caddy do this on the internal docker network.

Without `upstream_h2c:`, Caddy talks to the backend over HTTP/1.1, and gRPC clients get `INTERNAL` errors or empty responses.

## Expose a gRPC service

### Set `upstream_h2c: true` on the service [step]

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

### Deploy [step]

Run `yoink up`.

### Point the gRPC client at the domain [step]

Use `api.example.com:443` as the address in the gRPC client.

> **Behind Cloudflare?** Cloudflare does not proxy gRPC by default. In the dashboard, go to **Network → gRPC** and enable it for the zone. If gRPC is off, Cloudflare returns HTTP/2 connection errors with any Caddy config. The regular CDN features (caching, WAF) continue to apply after you enable gRPC.

## Reflection / grpcurl

To make sure that the service answers, run `grpcurl` against the domain:

```sh
grpcurl api.example.com:443 list
grpcurl api.example.com:443 grpc.health.v1.Health/Check
```

If the backend has reflection enabled, `list` prints the service descriptors. If reflection is off, pass `-proto path/to/your.proto` instead.

## Mixed gRPC + HTTP/1.1 backends

Some servers handle gRPC and plain HTTP on the same port. Examples are Tonic with Axum, grpc-go with http.Handler, and grpc-java with servlet. These servers usually accept plain HTTP requests over h2c. **One `upstream_h2c: true` covers the two protocols** for these servers.

If the backend rejects plain HTTP requests over h2c, route only the gRPC requests through h2c with `caddy_extra_caddyfile:`. All other requests then go through HTTP/1.1.

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

Browsers cannot speak native gRPC. They need [gRPC-Web](https://github.com/grpc/grpc-web), which uses HTTP/1.1 or HTTP/2 and a translation layer. There are two methods:

- **Translation in the backend**: Tonic has [`tonic-web`](https://docs.rs/tonic-web/), and grpc-go has [improbable-eng/grpc-web](https://github.com/improbable-eng/grpc-web/tree/master/go/grpcwebproxy). The backend speaks gRPC and gRPC-Web on the same port, and `upstream_h2c: true` covers the two.
- **Caddy plugin**: the `caddy-grpc-web` plugin translates at the proxy layer. Add it with [`proxy.xcaddy:`](/docs/how-to/caddy-plugins).

## Mutual TLS (Cloudflare origin-pull)

If Cloudflare is in front of the origin, configure mTLS with the procedure in [Cloudflare Origin Certificates](/docs/how-to/cloudflare-origin-certs). Then set `upstream_h2c: true` and `proxy.tls.client_auth` together:

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

## See also

- [Reverse proxy](/docs/explanation/proxy): how yoink routes traffic through Caddy.
- [Config reference](/docs/reference/config): the `upstream_h2c:` and `proxy.tls:` fields.
- [Cloudflare Origin Certificates](/docs/how-to/cloudflare-origin-certs): the mTLS procedure for the Cloudflare edge.
- [Caddy snippets cookbook](/docs/how-to/caddy-snippets): content-type matchers, request body limits, and other `caddy_extra_caddyfile:` patterns.
- [Caddy reverse_proxy docs](https://caddyserver.com/docs/caddyfile/directives/reverse_proxy#transport): the transport directives of the Caddy primitive.
