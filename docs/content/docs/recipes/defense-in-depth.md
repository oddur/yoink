---
title: Defense-in-depth web serving (Cloudflare + CrowdSec + Coraza)
weight: 14
---

A no-license-fee defense-in-depth stack for public-facing services — Cloudflare Free at the edge with Origin Certs locking the origin to Cloudflare's IPs, [CrowdSec](https://www.crowdsec.net/) for behavioural log-based bans at the proxy, and [Coraza](https://www.coraza.io/) for signature-based payload inspection. Each layer earns its place by doing something the others can't.

This isn't a yoink default — it requires plugins, secrets, a CrowdSec sidecar, and (most importantly) ongoing security operations: tuning OWASP CRS false positives, authoring CrowdSec scenarios, and reading audit logs. Yoink collapses the *deploy* into a single config file; the security engineering is on you. If that's not a budget you can spend, paying $25/mo for Cloudflare Pro and getting their managed CRS + bot management is the honest alternative.

## Architecture

```
client → Cloudflare Free      → Caddy [crowdsec → coraza] → app
         (DDoS, IP rep,         (cheap IP block            (your service)
          Bot Fight, free WAF;   then full CRS scan)
          mTLS origin-pull
          via Origin Certs)
```

What each layer brings:

- **Cloudflare Free + Origin Certs** — DDoS absorption, implicit IP-reputation filtering across millions of zones, Bot Fight Mode, and the Cloudflare Free Managed Ruleset (curated subset focused on high-profile/emergency CVEs like Log4j and Shellshock). Pair with a Cloudflare Origin Certificate and `client_auth: require_and_verify` to lock your origin so it only TLS-terminates for connections presenting a Cloudflare-signed client cert — leaked origin IPs become useless. See the [Origin Certs recipe](/docs/recipes/cloudflare-origin-certs) for the dashboard step. Limits: 1 MB request-body inspection cap; broader managed rules and OWASP CRS at the edge are Pro+.
- **CrowdSec + caddy-crowdsec-bouncer** — log-based behavioural detection. The CrowdSec agent reads your Caddy/app logs for failed logins, 404 scanning, brute force, path traversal probes, and emits decisions specific to *your* traffic. The bouncer enforces them at the proxy, microseconds per request. Plus a crowdsourced blocklist from the global CrowdSec community.
- **Coraza** — drop-in ModSecurity replacement with the full OWASP Core Rule Set (PL1–PL4). Signature-based payload inspection — scans request bodies, headers, and URLs against thousands of CVE/exploit patterns. Complements CrowdSec (CrowdSec catches *who*, Coraza catches *what*).

Each layer catches what the others miss. Cloudflare absorbs volumetric attacks at the edge; CrowdSec catches what makes it through using *your* logs (Cloudflare can't see app-level brute-force attempts on your `/login`); Coraza adds signature inspection on the surviving requests.

## yoink config

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
    caddy_extra_json: |
      [
        {
          "handler": "crowdsec",
          "appsec_url": "http://crowdsec:8080"
        },
        {
          "handler": "waf",
          "directives": [
            "Include @coraza.conf-recommended",
            "Include @crs-setup.conf.example",
            "Include @owasp_crs/*.conf",
            "SecRuleEngine On"
          ]
        }
      ]
    run:
      port: 8080
```

A few non-obvious points:

- `SecRuleEngine On` is critical. Coraza ships in `DetectOnly` by default — it logs attacks but doesn't block them. Coraza's own maintainers say so explicitly. Without flipping this, you have visibility, not protection.
- The two handlers in `caddy_extra_json:` run in declaration order. **CrowdSec first** (microsecond IP lookup), **Coraza second** (expensive request parsing and CRS evaluation). Reversed, you'd burn CPU running CRS on traffic you were going to drop anyway.
- `caddy-cloudflare-ip` auto-fetches Cloudflare's published IP ranges and refreshes them — `trusted_proxies: {source: cloudflare}` in `config_extra` consumes that module. Without the plugin, you'd hardcode the ranges manually and rotate them yourself when Cloudflare updates them.
- Networks: the proxy reaches the app and CrowdSec by container name over `yoink-ingress`. Yoink synthesizes an ingress network and joins every routed service plus the proxy. CrowdSec doesn't have `domain:` so it isn't routed, but it joins `yoink-ingress` (yoink's default for services without an explicit `networks:` list) so the bouncer can reach it as `crowdsec:8080`.
- The `crowdsec` service has no `domain:` — it's not exposed publicly. Only the proxy talks to it.

## Gotchas

### Banning Cloudflare's whole edge

Already handled by `config_extra: trusted_proxies/client_ip_headers` above — but verify it works. Deploy with `SecRuleEngine DetectOnly`, hit your site from a known browser, then `curl` from the proxy host with a payload that should trigger CRS. Inspect Coraza's audit log for the *real* client IP, not Cloudflare's. If you see a Cloudflare IP, the trust setup is wrong.

### Origin IP leakage defeats Cloudflare entirely

Cloudflare protects your origin only if attackers can't find it directly. DNS history (SecurityTrails, ViewDNS), Certificate Transparency logs, your mail server's A record, that one subdomain you forgot to proxy through Cloudflare — any of these expose the origin and let attackers skip the entire edge.

Defence: lock down your origin firewall to *only* accept connections from [Cloudflare's published IP ranges](https://www.cloudflare.com/ips/) on ports 80/443. Combined with the `client_auth: require_and_verify` mTLS check above, that's two layers — even if a leaked IP is found, the origin won't TLS-terminate without a Cloudflare-signed client cert.

### Push CrowdSec's local decisions up to Cloudflare's edge

Locally-detected bad IPs are blocked at *your* proxy — but the same IP keeps trying to reach Cloudflare's edge first, eating bandwidth and triggering rate-limit budget. CrowdSec ships a [Cloudflare bouncer](https://docs.crowdsec.net/docs/bouncers/cloudflare-workers) that pushes your local decisions to Cloudflare's IP firewall via API. Free-plan API works fine; the limit is the per-zone IP-rule count.

Wire it as another yoink service, give it a sealed `CF_API_TOKEN` secret, point it at the same CrowdSec local API. Bans propagate edge-ward, and the next attempt from that IP dies before reaching your bandwidth.

### CRS false positives

CRS at PL1 (the default) is conservative but still throws false positives on real traffic. Coraza's maintainers and the CRS docs both call out that tuning is step 1, not optional. **Run with `SecRuleEngine DetectOnly` for at least a week before flipping `On`** — read the audit logs, decide which rules to disable per-route, then enable blocking. Skipping this step is how you reject 5% of legit users without knowing.

CRS provides exceptions for vanilla WordPress; install any plugin and you'll hit FPs again. Same applies to most CMSes and frameworks. A November 2025 study comparing Coraza+Caddy vs Coraza+Envoy across PL1–PL4 found significant FP-rate variation between proxies. Translation: which proxy you pick changes how many legit users get blocked, and you only learn this by measuring on your traffic.

## Operational cost

Yoink takes the deploy out of the cost column. The plugins compile via `proxy.xcaddy:`, the trusted-proxies and any other global Caddy settings go through `proxy.config_extra:`, secrets stay in `secrets.age`, and adding a host to the fleet is one entry under `hosts:`. What's left is the recurring security work:

- **CRS false-positive tuning**: weeks. `SecRuleEngine DetectOnly` for at least a week of real traffic, audit-log review, per-rule exclusions, then `On` and another week of close monitoring before you trust blocking. Repeat for new endpoints.
- **CrowdSec scenario tuning**: a day to wire log shipping, dashboard, and confirm detections aren't catching benign cron jobs / health checks. Ongoing as you add new app surfaces.
- **Incident response**: when a FP nukes a real customer flow during business hours, someone who reads CRS audit logs needs to disable the right rule without opening a hole.
- **Updates that change behaviour**: a CRS minor release can shift FP rates; a CrowdSec scenario hub update can flip detection thresholds. Budget monthly attention.

## See also

- [Caddy plugins (xcaddy, no registry)](/docs/recipes/caddy-plugins) — the building block. Each plugin in this recipe is one line of `proxy.xcaddy.plugins:`.
- [Cloudflare Origin Certificates](/docs/recipes/cloudflare-origin-certs) — the mTLS origin-pull setup the `proxy.tls:` block above relies on.
- [Reverse proxy guide § `proxy.config_extra:`](/docs/guide/proxy#proxyconfig_extra-block--global-caddy-config-escape-hatch) — schema reference for the global Caddy config injection used here for `trusted_proxies`.
- [CrowdSec docs](https://docs.crowdsec.net/) — local API setup, scenarios, hub.
- [Coraza docs](https://coraza.io/docs/) — directives, paranoia levels, audit log shape.
- [OWASP Core Rule Set](https://coreruleset.org/docs/) — what the rules actually do.
- [`caddy-cloudflare-ip`](https://github.com/WeidiDeng/caddy-cloudflare-ip) — auto-refreshing Cloudflare IP-range trust source.
