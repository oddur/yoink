---
title: yoink
layout: docs
toc: false
---

A small, opinionated container deploy CLI + TUI for people who run a handful of services on a handful of bare-metal hosts.

- **Low ceremony.** Drop a `yoink.yaml` next to your code describing where the app should go, and `yoink up`.
- **Batteries and best practices included**, all on by default with no plugins to install:
  - `age`-sealed secrets
  - Hardened container defaults
  - Healthcheck-gated rolling swaps
  - Drift detection
  - Dependency-ordered waves
  - Bundled Caddy reverse proxy (Let's Encrypt or Cloudflare origin certs)
- **CLI + YAML, no GUI.** Perfect for AI agents (Claude Code, Cursor) and CI runners as much as for humans at a terminal.

```
yoink up                  # reconcile every service in dep order
yoink tui                 # k9s-style dashboard, drift, logs, shell-into
yoink prune               # clean up stale containers
yoink history api         # who deployed what, when
yoink rollback api        # roll back to the previous version
```

![yoink TUI](https://github.com/user-attachments/assets/bcf956b1-937a-43d7-92f6-0f24a1d0cd98)

{{< cards >}}
  {{< card link="/docs/start/first-deploy" title="Five-minute first deploy" subtitle="Drop a yoink.yaml next to your Dockerfile, run one command." icon="lightning-bolt" >}}
  {{< card link="/docs/intro/compared" title="Is this for me?" subtitle="Side-by-side matrix vs Kamal, Coolify, Dokku, Komodo, Kubernetes, plain compose. When yoink is the right answer, when it isn't." icon="adjustments" >}}
  {{< card link="/docs/guide/deploy-modes" title="Three deploy modes" subtitle="CI-built, local-build push-then-deploy, registry-less standalone." icon="server" >}}
  {{< card link="/docs/guide/security" title="Secure by default" subtitle="non-root uid, cap_drop=ALL, read-only rootfs, no-new-privileges, init=tini, …" icon="shield-check" >}}
  {{< card link="/docs/guide/ai-agents" title="Driving yoink from an AI agent" subtitle="CLI + YAML, no GUI. Patterns for Claude Code, Cursor, GitHub Actions." icon="terminal" >}}
  {{< card link="/docs/recipes" title="Recipes" subtitle="Staging alongside prod, sealed secrets, Cloudflare origin certs, Caddy snippets." icon="clipboard-list" >}}
  {{< card link="/docs/reference" title="Reference" subtitle="Every CLI flag, every config field, every TUI keybind." icon="document-text" >}}
{{< /cards >}}

## Why it exists

Single-host PaaS tools (Kamal, Dokku) are wonderful for "one app, one host" but creak when you want multiple services on the same box, replicas behind a proxy, or network isolation between tiers. Kubernetes solves all that — and a hundred other problems you don't have, in exchange for a control plane to operate, a YAML schema with a learning curve, and a vocabulary you have to teach every new operator.

`yoink` is the thinnest tool that gives you the few Kubernetes ideas that actually matter at small scale, while keeping the "one binary, ssh into the host, drive Docker directly" simplicity of the single-host PaaS world:

- **Multiple services per host** with replicas + dependency-ordered deploys (`depends_on:` topo-sort)
- **Per-service network tiers** for blast-radius isolation without a CNI plugin
- **Bundled Caddy reverse proxy** — one `domain:` field exposes a service over HTTPS with automatic Let's Encrypt or sealed Cloudflare origin certs. mTLS, h2c (gRPC), HSTS, all defaults
- **Drift detection.** Every effective spec hashes deterministically and lands as a label. The TUI shows drift across the cluster without guessing.
- **Three deploy modes**: CI-built (the default), local-build with `yoink build --push`, or fully standalone with `yoink up --build` (no CI, no registry — drop a `yoink.yaml` next to your Dockerfile and go)
- **Secure by default.** Every container yoink creates is hardened up front:
  - Non-root uid (`65534` / nobody)
  - `cap_drop=ALL`
  - `no-new-privileges`
  - read-only rootfs
  - `init=tini`
  - tmpfs `noexec`
  - binds default `:ro`

  Override per service when an image genuinely needs root.
- **Sealed secrets out of the box**: commit a single `secrets.age` file, decrypt with one key from `YOINK_AGE_KEY` (env in CI, file on your laptop). No remote vault needed. For teams that prefer a managed store, `provider: command` shells out to whatever CLI you already use — Doppler, 1Password, Vault, AWS Secrets Manager, the Infisical CLI — no first-party SDK to vendor.
- **CLI + YAML, no GUI**. Everything is a `yoink` subcommand or a `yoink.yaml` field — no web dashboard, no clicking. Identical experience on your laptop, in CI, and inside an AI coding agent like Claude Code or Cursor.
- **k9s-style TUI** with deploy history, one-press rollback, drift cells, logs auto-piped through `hl`

Read more in the [intro](/docs/intro), or skip ahead to [first deploy](/docs/start/first-deploy).
