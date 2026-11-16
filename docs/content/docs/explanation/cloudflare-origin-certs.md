---
title: Cloudflare Origin Certificates and Let's Encrypt
description: Why an origin behind Cloudflare can use an Origin Certificate in place of Let's Encrypt, and how that choice behaves on many hosts.
---

If your hosts are behind the edge of Cloudflare, an **Origin Certificate** is the TLS method with the fewest parts. It is free, it is valid for 15 years, and it uses no ACME. yoink has built-in support for it.

## Why this and not Let's Encrypt?

- **No rate limits.** Let's Encrypt permits 5 duplicate certificates for each domain each week. With many hosts and repeated redeploys, a project reaches that limit quickly.
- **No port-80 or DNS-01 challenge.** Cloudflare issues the certificate from its dashboard, separately from the deploy. Port 80 does not have to be reachable.
- **15-year validity.** A rotation each year is possible. Without rotation, a calendar reminder for the expiry date is sufficient.
- **Works with locked-down origins.** The origin can be on a private network or behind WAF rules. Only the edge of Cloudflare must reach the origin, through TLS on port 443.

The disadvantage: only clients that connect through Cloudflare trust the certificate. An origin certificate is not for direct browser access.

## Multi-host

Every host serves the same domain with the same certificate. The certificate and the key are in `secrets.age`, with one copy in the repo. The proxy on every host reads them from there. No ACME runs, so the hosts need no Redis and no shared storage.

## See also

- [Cloudflare Origin Certificates](/docs/how-to/cloudflare-origin-certs): the procedure to issue, install, and rotate an origin certificate, and to enable origin-pull mTLS.
- [Multi-host LE with Redis storage](/docs/how-to/multi-host-redis-storage): the Let's Encrypt method for many hosts.
- [Reverse proxy](/docs/explanation/proxy): how the yoink proxy terminates TLS.
- [Config reference](/docs/reference/config#proxytls): the fields of `proxy.tls:`.
