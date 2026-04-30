---
title: Guide
weight: 3
sidebar:
  open: true
---

The mental model. Read this once; come back to how-to / reference for the rest.

{{< cards cols="2" >}}
  {{< card link="/docs/guide/architecture" title="How it works" subtitle="Drift detection, deploy lock, spec_hash, dependency-ordered waves, healthcheck-gated rolling swap." icon="information-circle" >}}
  {{< card link="/docs/guide/deploy-modes" title="Three deploy modes" subtitle="CI-built, local-build push-then-deploy, registry-less standalone." icon="server" >}}
  {{< card link="/docs/guide/security" title="Security" subtitle="Hardened defaults — cap_drop=ALL, read-only rootfs, no-new-privileges. Plus the `devices:` escape hatch for GPUs / FUSE / USB." icon="shield-check" >}}
  {{< card link="/docs/guide/proxy" title="Reverse proxy" subtitle="Bundled Caddy. One field exposes a service over HTTPS with Let's Encrypt or sealed Cloudflare origin certs. Caddy snippets cookbook included." icon="globe" >}}
  {{< card link="/docs/guide/networking" title="Networking" subtitle="Tailscale + SSH connectivity, multi-host distribution (`replicas` + pinned hosts), and `yoink pf` for debugging without publishing ports." icon="link" >}}
  {{< card link="/docs/guide/secrets" title="Secrets" subtitle="age-sealed secrets in the repo (the default) plus `provider: command` recipes for sops / Doppler / Vault / 1Password / AWS SM / Infisical / Bitwarden." icon="lock-closed" >}}
  {{< card link="/docs/guide/templates" title="Templates (`yoink add`)" subtitle="Drop-in service fragments with sealed secrets. How to use the bundled set, and how to author your own." icon="puzzle" >}}
  {{< card link="/docs/guide/ai-agents" title="Driving yoink from an AI agent" subtitle="CLI + YAML, no GUI, deterministic exit codes. Patterns for Claude Code, Cursor, Aider, GitHub Copilot Workspace." icon="terminal" >}}
{{< /cards >}}
