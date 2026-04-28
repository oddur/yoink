---
title: Defense-in-depth web serving (Cloudflare + CrowdSec + Coraza)
weight: 14
---

A no-license-fee defense-in-depth stack for public-facing services — Cloudflare Free at the edge, [CrowdSec](https://www.crowdsec.net/) for behavioural log-based bans at the proxy, [Coraza](https://www.coraza.io/) for signature-based payload inspection, and Cloudflare Origin Certs for mTLS origin-pull. Each layer earns its place by doing something the others can't.

The reason this recipe is short rather than a 5,000-word setup guide: **yoink already does most of the work.** What's normally days of yak-shaving — compiling a custom caddy with `xcaddy`, building the multi-stage Dockerfile, choosing a registry and pushing to it, fanning the resulting image out to every proxy host, threading sealed secrets through, keeping it all in sync as you add or remove plugins — collapses into a `proxy.xcaddy.plugins:` list and a `proxy.tls:` block. Plugin set changes flip the content hash on the local image tag, every host rebuilds idempotently on the next `yoink up`, secrets stay in `secrets.age`. See the [caddy-plugins recipe](/docs/recipes/caddy-plugins) for the mechanics.

What yoink *can't* automate is the security engineering: tuning CRS false positives, picking CrowdSec scenarios, reading audit logs to decide whether the rule that blocked a real customer was right or wrong, deciding when to flip Coraza from `DetectOnly` to `On`. That's the actual cost of this stack, and it's worth pricing honestly before you commit.

## Architecture

```
client → Cloudflare Free      → Caddy [crowdsec → coraza] → app
         (DDoS, IP rep,         (cheap IP block            (your service)
          Bot Fight, free WAF)   then full CRS scan)
```

What each layer brings:

- **Cloudflare Free** — DDoS absorption, implicit IP-reputation filtering across millions of zones, Bot Fight Mode, and the Cloudflare Free Managed Ruleset (a curated subset focused on high-profile/emergency CVEs like Log4j and Shellshock). Hides your origin IP if origin firewalling is set up correctly. Limits: 1 MB request-body inspection cap; broader managed rules and OWASP CRS at the edge are Pro+ ($25/mo/zone).
- **CrowdSec + caddy-crowdsec-bouncer** — log-based behavioural detection. The CrowdSec agent reads your Caddy/app logs for failed logins, 404 scanning, brute force, path traversal probes, and emits decisions specific to *your* traffic. The bouncer enforces them at the proxy, microseconds per request. Plus a crowdsourced blocklist from the global CrowdSec community.
- **Coraza** — drop-in ModSecurity replacement with the full OWASP Core Rule Set (PL1–PL4). Signature-based payload inspection — scans request bodies, headers, and URLs against thousands of CVE/exploit patterns. Complements CrowdSec (CrowdSec catches *who*, Coraza catches *what*).
- **Cloudflare Origin Certs** — 15-year certificates issued by Cloudflare for origin-pull only; combined with `client_auth: require_and_verify`, your origin rejects any TLS connection that isn't presenting a Cloudflare-signed client cert. Locks the origin to Cloudflare's edge. See the [dedicated recipe](/docs/recipes/cloudflare-origin-certs) for the dashboard step.

The point of the stack: each layer catches what the others miss. Cloudflare absorbs volumetric attacks at the edge; CrowdSec catches what makes it through using *your* logs (Cloudflare can't see app-level brute-force attempts on your `/login`); Coraza adds signature inspection on the surviving requests.

## yoink config

The plugin compile is one line of `proxy.xcaddy:`. The supporting CrowdSec agent is a yoink service. Cloudflare Origin Cert mTLS is `proxy.tls:`. Everything else (the handler wiring) goes into `caddy_extra_json:`.

```yaml
deploy:
  networks: [yoink]

hosts:
  - { address: prod-1, user: deploy }

# Cloudflare Origin Certificate + origin-pull mTLS at the proxy.
# Origin rejects any connection without a Cloudflare-signed client cert.
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
      - github.com/hslatman/caddy-crowdsec-bouncer
      - github.com/corazawaf/coraza-caddy/v3

services:
  # CrowdSec local API — runs alongside the proxy on the same host.
  # Parses Caddy access logs, applies CrowdSec hub scenarios, and
  # serves decisions to the bouncer over yoink-ingress.
  - name: crowdsec
    image: crowdsecurity/crowdsec
    tag: latest
    networks: [yoink-ingress]
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

`SecRuleEngine On` is critical — Coraza ships in `DetectOnly` mode by default, which logs attacks but doesn't block them. Coraza's own maintainers say so explicitly. If you skip this line, you have visibility, not protection.

The two handlers in `caddy_extra_json:` run in declaration order — CrowdSec (microsecond IP lookup) first, Coraza (expensive request parsing and CRS evaluation) second. Reversed, you'd burn CPU running CRS on traffic you were going to drop anyway.

The CrowdSec agent runs as a regular yoink service on the proxy host, reachable from the bouncer at `crowdsec:8080` over `yoink-ingress`. For multi-host fleets, run one agent per host (each host's bouncer talks to its local agent — keeps the hot path entirely on-box).

## What `proxy.xcaddy:` can't do here (BYO-image escape hatch)

Two pieces of this stack need top-level Caddy server config — which yoink doesn't yet expose as a typed field. If you need either, drop `proxy.xcaddy:` for `proxy.image:` with a hand-built image that bakes in both the plugin compile and a bootstrap config.

### Real client IP from `CF-Connecting-IP`

When Cloudflare proxies, your origin sees Cloudflare's edge IPs in `RemoteAddr`. If the bouncer reads that, the *first* malicious request bans Cloudflare, and your site goes dark globally. **This is the single most common screwup in this exact stack.**

The fix is server-level: declare Cloudflare's IP ranges as trusted proxies and read the real client IP from `CF-Connecting-IP`. In Caddy JSON:

```json
{
  "servers": {
    "srv0": {
      "trusted_proxies": { "source": "cloudflare" },
      "client_ip_headers": ["CF-Connecting-IP"]
    }
  }
}
```

`trusted_proxies: { "source": "cloudflare" }` uses caddy's [Cloudflare module](https://github.com/WeidiDeng/caddy-cloudflare-ip) that auto-refreshes CF's published ranges. Add it to `proxy.xcaddy.plugins:` (`github.com/WeidiDeng/caddy-cloudflare-ip`); but the *server config* using it lives at the top-level Caddy config, not on a route. yoink renders the server block, so today this needs a bootstrap config.

### CrowdSec / Coraza global config

Both plugins also accept top-level Caddy app config blocks (Coraza: global `directives`, custom rule sets; CrowdSec: global API URL, ticker, app-sec endpoints). The per-route handler config in `caddy_extra_json:` covers the common case, but anything global needs the BYO-image path.

### The BYO-image pattern

```dockerfile
# Dockerfile.caddy-defense
FROM caddy:2-builder AS builder
RUN xcaddy build \
    --with github.com/hslatman/caddy-crowdsec-bouncer \
    --with github.com/corazawaf/coraza-caddy/v3 \
    --with github.com/WeidiDeng/caddy-cloudflare-ip

FROM caddy:2
COPY --from=builder /usr/bin/caddy /usr/bin/caddy
COPY caddy-bootstrap.json /etc/caddy/bootstrap.json
ENTRYPOINT ["caddy", "run", "--resume", "--config", "/etc/caddy/bootstrap.json"]
```

`caddy-bootstrap.json` (server config + the trust-the-proxy bit):

```json
{
  "admin": { "listen": "0.0.0.0:2019" },
  "apps": {
    "http": {
      "servers": {
        "srv0": {
          "listen": [":80", ":443"],
          "trusted_proxies": { "source": "cloudflare" },
          "client_ip_headers": ["CF-Connecting-IP"]
        }
      }
    }
  }
}
```

Caddy's `--resume` loads this bootstrap config first; yoink's `/load` then layers the rendered routing config on top. The `trusted_proxies` and `client_ip_headers` settings stick across reloads. Build once (locally or in CI), push to a registry your hosts can reach, reference from `proxy.image:` instead of `proxy.xcaddy:`.

Same trade-off as the [redis-storage recipe](/docs/recipes/multi-host-redis-storage) and the cache-handler advanced backend case — `proxy.xcaddy:` covers the plugin compile beautifully but doesn't yet have a story for top-level server config.

## Gotchas

### Banning Cloudflare's whole edge

Covered above — get `trusted_proxies` right or your first attack takes the site offline globally. This is the #1 failure mode of this stack. Test it deliberately: deploy with `SecRuleEngine DetectOnly`, hit your site from a known browser, then `curl` from the proxy host's shell with a payload that should trigger CRS. Inspect Coraza's logs for the *real* client IP, not Cloudflare's. If you see Cloudflare's IP, you have not configured trusted proxies correctly.

### Origin IP leakage defeats Cloudflare entirely

Cloudflare protects your origin only if attackers can't find it directly. DNS history (SecurityTrails, ViewDNS), Certificate Transparency logs, your mail server's A record, that one subdomain you forgot to proxy through Cloudflare — any of these expose the origin and let attackers skip the entire edge.

Defence: lock down your origin firewall to *only* accept connections from [Cloudflare's published IP ranges](https://www.cloudflare.com/ips/) on ports 80/443. Combined with the `client_auth: require_and_verify` mTLS check above, that's two layers — even if a leaked IP is found, the origin won't TLS-terminate without a Cloudflare-signed client cert.

### Push CrowdSec's local decisions up to Cloudflare's edge

Locally-detected bad IPs are blocked at *your* proxy — but the same IP keeps trying to reach Cloudflare's edge first, eating bandwidth and triggering rate-limit budget. CrowdSec ships a [Cloudflare bouncer](https://docs.crowdsec.net/docs/bouncers/cloudflare-workers) that pushes your local decisions to Cloudflare's IP firewall via API. Free-plan API works fine for this; the limit is the per-zone IP-rule count.

Wire it as another yoink service, give it a sealed `CF_API_TOKEN` secret, point it at the same CrowdSec local API. Now bans propagate edge-ward, and the next attempt from that IP dies before reaching your bandwidth.

### Handler ordering

`crowdsec` first, `waf` (Coraza) second. CrowdSec is a hashmap lookup; Coraza is regex-heavy CRS evaluation. Reversed, you pay CRS cost on traffic you were going to drop anyway.

### CRS false positives

CRS at PL1 (the default) is conservative but still throws false positives on real traffic. Coraza's maintainers and the CRS docs both call out that tuning is step 1, not optional. **Run with `SecRuleEngine DetectOnly` for at least a week before flipping `On`** — read the audit logs, decide which rules to disable per-route, then enable blocking. Skipping this step is how you reject 5% of legit users without knowing.

CRS provides exceptions for vanilla WordPress; install any plugin and you'll hit FPs again. Same applies to most CMSes and frameworks.

A November 2025 study comparing Coraza+Caddy vs Coraza+Envoy across PL1–PL4 found significant FP-rate variation between proxies. Translation: which proxy you pick changes how many legit users get blocked, and you only learn this by measuring on your traffic.

## Cost

This is not a "free" stack. It's a no-license-fee stack — and yoink takes the deploy out of the cost column, but the security work is still on you.

What yoink absorbs:

- **Compiling and shipping caddy with the plugins** is `proxy.xcaddy.plugins:`. No Dockerfile, no registry push, no per-host coordination — content-addressed local tag, idempotent rebuilds when the plugin set changes.
- **Origin Cert + mTLS origin-pull** is `proxy.tls:` plus three sealed secrets. Single-source-of-truth in `secrets.age`; every host's proxy reads the same encrypted material.
- **Multi-host fan-out** for the proxy + CrowdSec agent is `applicable_hosts` plus the existing reconcile loop. Adding a host to the fleet means one entry under `hosts:` and `yoink up` — that host pays its first-time xcaddy compile (~2-5 minutes), then joins steady state.
- **Plugin-set rotation** is editing `proxy.xcaddy.plugins:`. Hash flips → fresh build on next `up` → old image labelled with `yoink.caddy.xcaddy_hash` for human-driven cleanup.
- **Updates** to caddy, plugins, base/builder images: change a pin, `yoink up`, every host rebuilds.

What's still on you (and these are the actual time sinks):

- **CRS false-positive tuning**: weeks. `SecRuleEngine DetectOnly` for at least a week of real traffic, audit-log review, per-rule exclusions, then `On` and another week of close monitoring before you trust blocking. Repeat for new endpoints. Yoink has nothing to add here — it's measurement and judgement against your traffic.
- **CrowdSec scenario tuning**: a day to wire log shipping (Caddy access logs → CrowdSec agent), dashboard, and confirm detections aren't catching benign cron jobs / health checks. Ongoing as you add new app surfaces.
- **The trusted_proxies trap**: see the gotcha section above. The first time you set this up, budget half a day for the BYO-image bootstrap config and verifying the real client IP makes it through to both bouncer and Coraza. After that, it's stable.
- **Incident response**: when a FP nukes a real customer flow during business hours, someone who reads CRS audit logs needs to disable the right rule without opening a hole. That's a skill, not a config knob.
- **Updates that change behaviour**: a CRS minor release can shift FP rates; a CrowdSec scenario hub update can flip detection thresholds. Budget monthly attention.

So: the deploy is solved. The security operations are the cost. Choose accordingly.

## When NOT to do this

- **Revenue-bearing services** where false positives silently churn customers. With yoink the *deploy* is no longer the question — the question is whether you'll do the security operations. If nobody on your team will read CRS audit logs weekly and triage FPs, **Cloudflare Pro at $25/mo/zone** sells you that operational outcome (managed OWASP CRS, attack-score ML, Cloudflare-tuned bot management). Not because Pro is technically superior to a tuned Coraza + CrowdSec — it isn't always — but because outsourcing the operations to Cloudflare is the actual product.
- **Solo operators on production traffic.** A misconfigured Coraza is worse than no Coraza: false sense of protection, no visibility into the FPs you're blocking, users churning silently when their request hits PL2 with a custom payload Cloudflare's never seen but CRS' generic regex catches.
- **Personalised content** where caching/blocking based on coarse signals is unsafe (per-user dashboards, signed-in feeds, payment flows).
- **You haven't set up Cloudflare Origin Certs first.** The whole stack assumes the origin is only reachable via Cloudflare. If your DNS is `A` records pointing directly at the origin, the layered defence is illusory — attackers go around it via DNS history or CT logs. The [Origin Certs recipe](/docs/recipes/cloudflare-origin-certs) is a prerequisite, not a co-equal.

## When this *is* right

- **Personal projects, side businesses, hobby fleets** where you've got the operational appetite to read logs and tune. Yoink makes the build/deploy effort minimal; the recurring work is interesting security engineering, not yak-shaving.
- **Internal tools and pre-production environments** with predictable traffic. FP tuning is one-time investment, not a rolling incident.
- **Building security operations muscle** before evaluating paid edges. Running this stack on a low-stakes service teaches CRS, scenario authoring, and FP triage — skills that transfer to honestly evaluating Cloudflare Pro/Business or running in-house elsewhere.
- **Compliance contexts** where "no third-party processor sees customer payloads" matters. The whole stack runs on your hosts; Cloudflare sees TLS-terminated traffic in transit but custom rule evaluation is yours.

## See also

- [Caddy plugins (xcaddy, no registry)](/docs/recipes/caddy-plugins) — the building block. Each plugin in this recipe is one line of `proxy.xcaddy.plugins:`.
- [Cloudflare Origin Certificates](/docs/recipes/cloudflare-origin-certs) — the mTLS origin-pull setup the `proxy.tls:` block above relies on.
- [Multi-host Let's Encrypt with Redis storage](/docs/recipes/multi-host-redis-storage) — pattern reference for the BYO-image bootstrap-config trick used here for `trusted_proxies`.
- [CrowdSec docs](https://docs.crowdsec.net/) — local API setup, scenarios, hub.
- [Coraza docs](https://coraza.io/docs/) — directives, paranoia levels, audit log shape.
- [OWASP Core Rule Set](https://coreruleset.org/docs/) — what the rules actually do.
