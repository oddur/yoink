---
title: Defense-in-depth web serving (Cloudflare + CrowdSec + Coraza)
description: Deploy a hardened web stack with no license fee from Cloudflare Free, CrowdSec, Coraza WAF, and sealed origin certificates.
weight: 14
---

This stack puts Cloudflare Free, [CrowdSec](https://www.crowdsec.net/), and [Coraza](https://www.coraza.io/) in front of a public-facing service. For what each layer does and why the config has this shape, see [Defense-in-depth web serving, layer by layer](/docs/explanation/defense-in-depth).

## Prerequisites

- A Cloudflare Origin Certificate and origin-pull mTLS, with `CF_ORIGIN_CERT`, `CF_ORIGIN_KEY`, and `CF_ORIGIN_PULL_CA` in the sealed bundle. See [Cloudflare Origin Certificates](/docs/how-to/cloudflare-origin-certs).
- A `CROWDSEC_LAPI_KEY` value in the sealed bundle.
- Time for continuous security operations: you tune OWASP CRS false positives, write CrowdSec scenarios, and read audit logs. yoink does only the deploy.

## Configure and deploy the stack

### Add the config to `yoink.yaml` [step]

```yaml
deploy:
  networks: [yoink]

hosts:
  - { address: prod-1, user: deploy }

proxy:
  email: ops@example.com
  # Cloudflare Origin Certificate + origin-pull mTLS at the proxy.
  # Origin rejects any connection without a Cloudflare-signed client cert.
  tls:
    cert_secret: CF_ORIGIN_CERT
    key_secret:  CF_ORIGIN_KEY
    client_auth:
      mode: require_and_verify
      trust_pool_secret: CF_ORIGIN_PULL_CA
  xcaddy:
    plugins:
      - github.com/WeidiDeng/caddy-cloudflare-ip
      - github.com/hslatman/caddy-crowdsec-bouncer
      - github.com/corazawaf/coraza-caddy/v3
  # Trust Cloudflare's edge as a proxy and read the real client IP
  # from CF-Connecting-IP. WITHOUT THIS, the bouncer reads
  # Cloudflare's IPs and the first attack bans Cloudflare's whole
  # edge — taking the site offline globally. This is the #1 failure
  # mode of this stack; do not skip.
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
  # CrowdSec + Coraza run for every routed service — wire them once
  # at the proxy level instead of repeating per-service handlers.
  # Order matters: CrowdSec (microsecond IP lookup) before Coraza
  # (full CRS regex evaluation) so we don't burn CPU on traffic
  # we were going to drop anyway.
  global_handlers:
    - |
      {
        "handler": "crowdsec",
        "appsec_url": "http://crowdsec:8080"
      }
    - |
      {
        "handler": "waf",
        "directives": [
          "Include @coraza.conf-recommended",
          "Include @crs-setup.conf.example",
          "Include @owasp_crs/*.conf",
          "SecRuleEngine On"
        ]
      }

services:
  # CrowdSec local API. The proxy bouncer reaches it by name over
  # `yoink-ingress`. One agent per host keeps the hot path on-box.
  - name: crowdsec
    image: crowdsecurity/crowdsec
    tag: latest
    env:
      COLLECTIONS: "crowdsecurity/caddy crowdsecurity/base-http-scenarios"
    secrets: [CROWDSEC_LAPI_KEY]
    run:
      port: 8080
      volumes:
        - "crowdsec-data:/var/lib/crowdsec/data"
        - "crowdsec-config:/etc/crowdsec"

  - name: app
    image: ghcr.io/me/app
    tag: v1.0.0
    domain: app.example.com
    run:
      port: 8080
```

If you change the config, obey these rules:

| Rule | Reason |
|---|---|
| Keep `SecRuleEngine On`. | By default, Coraza is in `DetectOnly` mode: it logs attacks but does not block them. |
| Keep CrowdSec before Coraza in `global_handlers:`. | The list order is the order of the request pipeline. In the reverse order, Coraza uses CPU on traffic that CrowdSec then rejects. |
| Keep the `config_extra` block. | Without it, the bouncer reads the IPs of Cloudflare, and the first attack bans all of the Cloudflare edge. Then the site is offline globally. |
| Use `apps.http.servers.main` in `config_extra:`, not `srv0`. | The server that yoink renders has the name `main`. Upstream docs use `srv0`. With `srv0`, Caddy gets a second server block that never listens, and `trusted_proxies` has no effect. |
| Keep the `caddy-cloudflare-ip` plugin. | `trusted_proxies: {source: cloudflare}` in `config_extra` uses this module. |
| Do not add a per-service `caddy_extra_json:` for the two plugins. | `proxy.global_handlers:` applies them to every routed service. |
| Do not add a `domain:` to the `crowdsec` service. | Without `domain:`, the service is not public, and only the proxy talks to it. |

### Lock the host firewall to Cloudflare [step]

Permit inbound traffic on `:80` and `:443` only from the [published IP ranges of Cloudflare](https://www.cloudflare.com/ips/). Then a leaked origin IP cannot get a TLS connection. The `client_auth: {mode: require_and_verify}` mTLS layer is the second barrier.

### Deploy [step]

Run `yoink up`.

### Make sure that Coraza sees the real client IP [step]

Send a request to your site with `curl` and a payload that triggers CRS. Then make sure that the audit log of Coraza shows the real client IP, not a Cloudflare IP.

## See also

- [Defense-in-depth web serving, layer by layer](/docs/explanation/defense-in-depth): the architecture, what each layer catches, and the reasons for the handler order, the real client IP, and the networks.
- [Caddy plugins (xcaddy, no registry)](/docs/how-to/caddy-plugins): each plugin in this stack is one line of `proxy.xcaddy.plugins:`.
- [Cloudflare Origin Certificates](/docs/how-to/cloudflare-origin-certs): the mTLS origin-pull setup that the `proxy.tls:` block needs.
- [Reverse proxy § `proxy.config_extra:`](/docs/reference/proxy#proxyconfig_extra-block-global-caddy-config-escape-hatch) and [§ `proxy.global_handlers:`](/docs/reference/proxy#proxyglobal_handlers-block-proxy-wide-middleware-chain): the proxy-wide config and the middleware chain that this stack uses.
- [CrowdSec docs](https://docs.crowdsec.net/) and [`caddy-crowdsec-bouncer`](https://github.com/hslatman/caddy-crowdsec-bouncer): agent setup, scenarios, hub, and bouncer config.
- [Coraza docs](https://coraza.io/docs/) and [`coraza-caddy`](https://github.com/corazawaf/coraza-caddy): directives, paranoia levels, and audit logs.
- [OWASP Core Rule Set](https://coreruleset.org/docs/): what the CRS rules do.
- [`caddy-cloudflare-ip`](https://github.com/WeidiDeng/caddy-cloudflare-ip): the trust source that refreshes the IP ranges of Cloudflare automatically.
