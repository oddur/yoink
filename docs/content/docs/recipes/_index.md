---
title: Recipes
weight: 4
sidebar:
  open: true
---

Task-oriented how-tos. Each recipe stands alone — pick the one that matches what you're doing.

{{< cards cols="2" >}}
  {{< card link="/docs/recipes/ai-agents" title="Driving yoink from an AI agent" subtitle="CLI + YAML, no GUI, deterministic exit codes. Patterns for Claude Code, Cursor, Aider, GitHub Copilot Workspace." icon="terminal" >}}
  {{< card link="/docs/recipes/sealed-secrets" title="Sealed secrets (age)" subtitle="The default. Commit secrets.age, decrypt with one key. No remote vault needed." icon="lock-closed" >}}
  {{< card link="/docs/recipes/age-in-github-actions" title="AGE secrets in GitHub Actions" subtitle="Generate a CI-only identity, paste into a GitHub secret, deploy. One env var." icon="lightning-bolt" >}}
  {{< card link="/docs/recipes/cloudflare-origin-certs" title="Cloudflare Origin Certificates" subtitle="Skip Let's Encrypt — 15-year cert + origin-pull mTLS that locks your origin to Cloudflare's edge." icon="cloud" >}}
  {{< card link="/docs/recipes/grpc-hosting" title="Hosting gRPC backends" subtitle="`upstream_h2c: true` for native gRPC (Tonic, grpc-go, grpc-java) — covers REST too." icon="switch-horizontal" >}}
  {{< card link="/docs/recipes/caddy-snippets" title="Caddy snippets cookbook" subtitle="Copy-paste recipes for forward_auth, basic_auth, IP allowlists, headers, redirects, body limits." icon="book-open" >}}
  {{< card link="/docs/recipes/multi-host-redis-storage" title="Multi-host Let's Encrypt with Redis" subtitle="Share ACME state across hosts to avoid Let's Encrypt rate limits." icon="database" >}}
  {{< card link="/docs/recipes/staging-alongside-prod" title="Run staging alongside prod" subtitle="Same hosts, two configs, namespaced services + networks." icon="duplicate" >}}
  {{< card link="/docs/recipes/self-hosted-registry" title="Self-hosted registry on a yoink host" subtitle="Run registry:2 as a yoink service, expose via tailnet, push to it." icon="cube" >}}
  {{< card link="/docs/recipes/secrets-external-cli" title="External secrets via CLI" subtitle="Per-tool wiring for Doppler, 1Password, Vault, AWS Secrets Manager, Infisical CLI, Bitwarden — anything that emits dotenv or JSON." icon="key" >}}
  {{< card link="/docs/recipes/pr-comment-dry-run" title="Pre-merge dry-run on every PR" subtitle="Sticky GitHub PR comment showing what `yoink up` would change before the merge." icon="annotation" >}}
  {{< card link="/docs/recipes/multi-host-distribution" title="Multi-host distribution" subtitle="`replicas` + `services[].hosts` patterns: scale-out, singletons, region-pinned, stateful + stateless." icon="server" >}}
  {{< card link="/docs/recipes/watch-mode" title="Edit-save-deploy with --watch" subtitle="`yoink up --watch --build --no-registry` polls the config and redeploys on save. Hot-reload for prod-like dev without CI." icon="refresh" >}}
  {{< card link="/docs/recipes/port-forward" title="Port-forward to any service" subtitle="`yoink pf <service>` tunnels to any container, published or not — auto-spawns a sidecar for sealed-network services. Lets the locked-down `no publish:` default be the easy default." icon="link" >}}
{{< /cards >}}
