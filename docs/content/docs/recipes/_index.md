---
title: Recipes
weight: 4
sidebar:
  open: true
---

Outcome-oriented how-tos. Each recipe is a series of steps that take you from "I want X" to "X is running in production." Pick the one that matches what you're trying to accomplish — for the deeper "how does this work" content, see the [Guide](/docs/guide).

{{< cards cols="2" >}}
  {{< card link="/docs/recipes/hetzner-quickstart" title="Hetzner cx23 quickstart with HTTPS" subtitle="Empty Hetzner project → live HTTPS endpoint in ~90 seconds. €3.99/mo cx23, AGE-sealed deploy key, real Let's Encrypt cert via `<ip>.nip.io`." icon="lightning-bolt" >}}
  {{< card link="/docs/recipes/tanstack-stack" title="TanStack Start + postgres from scratch" subtitle="End-to-end: scaffold the app, add postgres via `yoink add`, deploy, verify with `yoink pf`." icon="lightning-bolt" >}}
  {{< card link="/docs/recipes/volume-backups" title="Volume backups + postgres PITR" subtitle="Nightly restic snapshots to any S3 (or self-hosted RustFS over Tailscale) plus postgres WAL archiving for point-in-time recovery." icon="archive" >}}
  {{< card link="/docs/recipes/cloudflare-origin-certs" title="Cloudflare Origin Certificates" subtitle="Skip Let's Encrypt — 15-year cert + origin-pull mTLS that locks your origin to Cloudflare's edge." icon="cloud" >}}
  {{< card link="/docs/recipes/defense-in-depth" title="Defense-in-depth web serving" subtitle="No-license-fee hardened stack — Cloudflare Free + CrowdSec + Coraza + Origin Certs. The architecture, the gotchas, and the honest cost." icon="shield-check" >}}
  {{< card link="/docs/recipes/caddy-plugins" title="Caddy plugins (xcaddy, no registry)" subtitle="`proxy.xcaddy.plugins:` builds a custom caddy on each host with rate-limit, l4, redis-storage, third-party DNS providers — no registry required." icon="puzzle" >}}
  {{< card link="/docs/recipes/grpc-hosting" title="Hosting gRPC backends" subtitle="`upstream_h2c: true` for native gRPC (Tonic, grpc-go, grpc-java) — covers REST too." icon="switch-horizontal" >}}
  {{< card link="/docs/recipes/age-in-github-actions" title="AGE secrets in GitHub Actions" subtitle="Generate a CI-only identity, paste into a GitHub secret, deploy. One env var." icon="key" >}}
  {{< card link="/docs/recipes/multi-host-redis-storage" title="Multi-host Let's Encrypt with Redis" subtitle="Share ACME state across hosts to avoid Let's Encrypt rate limits." icon="database" >}}
  {{< card link="/docs/recipes/staging-alongside-prod" title="Run staging alongside prod" subtitle="Same hosts, two configs, namespaced services + networks." icon="duplicate" >}}
  {{< card link="/docs/recipes/self-hosted-registry" title="Self-hosted registry on a yoink host" subtitle="Run registry:2 as a yoink service, expose via tailnet, push to it." icon="cube" >}}
  {{< card link="/docs/recipes/pr-comment-dry-run" title="Pre-merge dry-run on every PR" subtitle="Sticky GitHub PR comment showing what `yoink up` would change before the merge." icon="annotation" >}}
  {{< card link="/docs/recipes/watch-mode" title="Edit-save-deploy with --watch" subtitle="`yoink up --watch --build` polls the config and redeploys on save. Hot-reload for prod-like dev without CI." icon="refresh" >}}
{{< /cards >}}
