---
title: Cloudflare Origin Certificates
weight: 10
---

If you're already on Cloudflare's edge, **Origin Certificates** are the simplest TLS story for your origin servers: free, valid for 15 years, no ACME, no Let's Encrypt rate limits, no port-80 reachability requirement. Yoink supports them out of the box.

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

Note: `proxy.email:` isn't required when no service uses `tls: auto`. The proxy block can be empty (or omitted entirely).

## Cloudflare side

Make sure the zone's SSL mode is **Full (strict)** (Cloudflare → SSL/TLS → Overview). This is what asks Cloudflare to validate the origin cert.

## Rotation

Origin certs are good for 15 years, but you'll probably want to rotate every 1–3 years anyway. When the time comes:

1. Generate a new cert in Cloudflare's dashboard.
2. `yoink secrets edit`, paste the new values into `CF_ORIGIN_CERT` + `CF_ORIGIN_KEY`.
3. `git commit -am 'rotate cf origin cert'`.
4. `yoink up`.

Yoink notices the cert bytes have changed (via the proxy's spec hash including the cert reference labels), redeploys the proxy container (~10s), and the new cert is live.

For zero-downtime rotation, use `yoink up --service yoink-proxy` from one host at a time if you're multi-host.

## Multi-host

Each host serves the same domain with the same cert — no Let's Encrypt rate-limit problem because no ACME is happening. Origin certs work the same on every host.

The cert + key live in `secrets.age` (one copy in the repo). Every host's proxy reads it from the same source. No Redis, no shared storage needed.

## See also

- [Reverse proxy guide](/docs/guide/proxy) — full schema reference.
- [Multi-host LE with Redis storage](/docs/recipes/multi-host-redis-storage) — for the Let's Encrypt path when you don't want to use Cloudflare.
- [Cloudflare Origin CA docs](https://developers.cloudflare.com/ssl/origin-configuration/origin-ca/) — Cloudflare's side of the setup.
