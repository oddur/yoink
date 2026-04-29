---
title: Cloudflare Origin Certificates
weight: 13
---

If you're already on Cloudflare's edge, **Origin Certificates** are the simplest TLS story for your origin hosts: free, valid for 15 years, no ACME, no Let's Encrypt rate limits, no port-80 reachability requirement. Yoink supports them out of the box.

## Why this and not Let's Encrypt?

- **No rate limits.** LE caps you to 5 duplicate certs / week / domain. With multi-host and forgetful redeploys, hitting that is easy.
- **No port-80 / DNS-01 dance.** Origin certs are issued out-of-band from Cloudflare's dashboard.
- **15-year validity.** Set a calendar reminder, rotate quarterly, never think about it.
- **Works with locked-down origins.** Your origin can be on a private network, behind WAF rules, etc. — only Cloudflare's edge needs to reach it, and it does so via TLS on port 443 using the origin cert.

The downside: only Cloudflare-fronted clients trust the cert. That's the point — origin certs are not for direct browser access.

## One-time setup (Cloudflare dashboard)

1. Log into Cloudflare → your zone → SSL/TLS → Origin Server.
2. Click **Create Certificate**. Defaults are fine: ECC, 15 years, hostnames `*.example.com` + `example.com` (or list each host explicitly).
3. Copy the **Origin Certificate** (PEM, full chain) and the **Private Key**.

## Add the secrets to `secrets.age`

```sh
yoink secrets edit
```

In the editor, add:

```dotenv
CF_ORIGIN_CERT=-----BEGIN CERTIFICATE-----
MIIE...
-----END CERTIFICATE-----
CF_ORIGIN_KEY=-----BEGIN PRIVATE KEY-----
MIIE...
-----END PRIVATE KEY-----
```

Save and exit. The values land in `secrets.age` (committed to the repo, encrypted).

## Wire it up in `yoink.yaml`

**Single service, single cert** — per-service:

```yaml
services:
  - name: api
    image: ghcr.io/me/api
    tag: v1
    domain: api.example.com
    tls: cert                              # ← skip ACME, use the inline cert
    tls_cert_secret: CF_ORIGIN_CERT
    tls_key_secret: CF_ORIGIN_KEY
    run:
      port: 8080
```

**Many services, one wildcard cert** (the common case — `*.example.com` covers everything) — proxy-level:

```yaml
proxy:
  tls:
    cert_secret: CF_ORIGIN_CERT
    key_secret:  CF_ORIGIN_KEY

services:
  - name: api
    domain: api.example.com
    run: { port: 8080 }
  - name: web
    domain: example.com
    run: { port: 3000 }
  - name: admin
    domain: admin.example.com
    run: { port: 9090 }
```

Every routed service inherits the proxy-level cert. Per-service `tls_cert_secret:` still overrides for the rare different-cert case.

Note: `proxy.email:` isn't required when no service uses `tls: auto`. ACME is implicitly off when `proxy.tls.cert_secret` is set.

## Origin-pull mTLS (lock your origin to Cloudflare's edge)

By default, anyone who knows your origin IP can hit it directly with a `Host:` header — bypassing Cloudflare's WAF, rate limits, bot blocks, etc. **Origin-pull mTLS** fixes that: the origin requires every request to present a Cloudflare-signed client certificate. Anything else gets a TLS handshake error.

### One-time setup

Cloudflare provides a static **Origin Pull CA bundle** that signs every cert their edge presents to your origin:

1. Download the bundle from <https://developers.cloudflare.com/ssl/static/authenticated_origin_pull_ca.pem>.
2. Add it to `secrets.age` alongside the cert/key:

```dotenv
CF_ORIGIN_PULL_CA=-----BEGIN CERTIFICATE-----
<bundle contents>
-----END CERTIFICATE-----
```

3. Enable Authenticated Origin Pulls in the Cloudflare dashboard: SSL/TLS → Origin Server → **Authenticated Origin Pulls** → toggle on.

### `yoink.yaml`

Add `client_auth:` to the `proxy.tls:` block:

```yaml
proxy:
  tls:
    cert_secret: CF_ORIGIN_CERT
    key_secret:  CF_ORIGIN_KEY
    client_auth:
      mode: require_and_verify             # strict — reject anything not signed by the CA
      trust_pool_secret: CF_ORIGIN_PULL_CA
```

That applies to **every** routed service. From now on, requests that don't come through Cloudflare are dropped at the TLS layer — your origin simply doesn't appear to exist for them.

`mode:` options (default `require_and_verify`):

- `require_and_verify` — strict; reject anything without a valid client cert. **Recommended.**
- `require` — require a cert but skip verification (don't use this).
- `verify_if_given` — accept anonymous; verify if present. Mixed-mode.
- `request` — request but don't enforce. Almost never useful.

### Verify

After `yoink up`, this should fail (no client cert):

```sh
curl -v https://your-origin-ip/health -H 'Host: api.example.com'
# → tls: handshake failure
```

This should succeed (through Cloudflare's edge):

```sh
curl https://api.example.com/health
# → 200 OK
```

## Real client IP from `CF-Connecting-IP`

Cloudflare proxies, so by default your origin (and Caddy access logs, Coraza, any rate-limit handler) sees Cloudflare's edge IPs in `RemoteAddr`. That's almost never what you want — log analytics, geo-IP, app-side rate limits, and any IP-based abuse handling all collapse to "every request is from Cloudflare." The fix is two lines: a caddy plugin that auto-refreshes Cloudflare's IP ranges, plus a `proxy.config_extra:` block telling Caddy to trust those ranges as proxies and read the real IP from `CF-Connecting-IP`.

```yaml
proxy:
  email: ops@example.com
  tls:
    cert_secret: CF_ORIGIN_CERT
    key_secret:  CF_ORIGIN_KEY
    client_auth:
      mode: require_and_verify
      trust_pool_secret: CF_ORIGIN_PULL_CA
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

> ⚠ **Note the path: `apps.http.servers.main.<…>`.** Yoink's rendered server is named `main`, not the Caddy convention `srv0` you'll see in upstream docs and Caddyfile-adapted output. If you write `srv0` instead, the deep-merge silently creates a second server config that listens on nothing, and your `trusted_proxies` is dead config.

[`caddy-cloudflare-ip`](https://github.com/WeidiDeng/caddy-cloudflare-ip) is the Caddy module that registers a `cloudflare` source for `trusted_proxies` and refreshes the IP list periodically. Without it, `trusted_proxies: {source: cloudflare}` would fail at config load.

This pairs with the origin-pull mTLS above for a clean two-layer story: the origin only TLS-terminates for Cloudflare (mTLS), and within that pipe the real client IP propagates correctly via `CF-Connecting-IP`.

## Cloudflare side

Make sure the zone's SSL mode is **Full (strict)** (Cloudflare → SSL/TLS → Overview). This is what asks Cloudflare to validate the origin cert.

## Rotation

Origin certs are good for 15 years, but you'll probably want to rotate every 1–3 years anyway. When the time comes:

1. Generate a new cert in Cloudflare's dashboard.
2. `yoink secrets edit`, paste the new values into `CF_ORIGIN_CERT` + `CF_ORIGIN_KEY`.
3. `git commit -am 'rotate cf origin cert'`.
4. `yoink up`.

Yoink notices the cert bytes have changed, redeploys the proxy container (~10s), and the new cert is live.

For zero-downtime rotation, use `yoink up --service yoink-proxy` from one host at a time if you're multi-host.

## Multi-host

Each host serves the same domain with the same cert — no Let's Encrypt rate-limit problem because no ACME is happening. Origin certs work the same on every host.

The cert + key live in `secrets.age` (one copy in the repo). Every host's proxy reads it from the same source. No Redis, no shared storage needed.

## See also

- [Reverse proxy guide](/docs/guide/proxy) — full schema reference.
- [Defense-in-depth web serving](/docs/recipes/defense-in-depth) — Origin Certs are the bottom layer; this recipe stacks CrowdSec + Coraza on top.
- [Multi-host LE with Redis storage](/docs/recipes/multi-host-redis-storage) — for the Let's Encrypt path when you don't want to use Cloudflare.
- [Cloudflare Origin CA docs](https://developers.cloudflare.com/ssl/origin-configuration/origin-ca/) — Cloudflare's side of the setup.
