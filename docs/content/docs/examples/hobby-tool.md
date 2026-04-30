---
title: Hobby tool / utility
description: A single-host, single-service yoink.yaml built locally with no registry — the minimal five-line config.
weight: 1
---

A Slack bot, admin CLI, internal status board, cron-as-container — one container that needs a host but doesn't justify CI + a registry + Kubernetes.

**The minimum `yoink.yaml`**: one host, one service, `Dockerfile` next to the config, no registry. `yoink up --build`.

```yaml
# yoink.yaml — sits alongside the Dockerfile
hosts:
  - { address: my-server, user: deploy }   # tailnet hostname

services:
  - name: my-tool
    image: my-tool                         # bare name; `build:` block makes it local-only
    tag: dev
    build:
      context: .                           # `.` = same directory as yoink.yaml
    run:
      port: 8080
```

```sh
yoink up --build
```

~15 seconds for a small image. The `build:` block makes the image local-only, so yoink ships it from your docker daemon to the host over SSH via [unregistry](https://github.com/psviderski/unregistry) — only changed layers cross the wire.

## What yoink does without you asking

The container inherits yoink's hardened defaults: read-only rootfs, no Linux caps, no setuid escalation, fork-bomb bound, tini as PID 1, healthcheck-gated swap. See [secure by default](/docs/guide/security) for the full list.

If `my-tool` needs to write somewhere, give it a tmpfs:

```yaml
run:
  port: 8080
  options:
    tmpfs:
      /tmp: "size=64m,mode=1777"           # auto-noexec,nosuid,nodev applied
```

## When to graduate

This shape stays fine for hobby / utility / internal-tool deployments. Outgrow it when:

- **Replicas** — single-container swap downtime is your downtime. Add `replicas: 2` for rolling swap (capacity N-1).
- **Multiple hosts** — yoink ships the build artifact from your daemon to every host on every deploy. Once painful, add a [self-hosted tailnet registry](/docs/how-to/self-hosted-registry) so hosts pull from a shared cache.
- **CI-triggered deploys** — keep `build:`, add a real registry, switch to `yoink build --push` + `yoink up`.

## See also

- [First deploy](/docs/start/first-deploy) — the tutorial form of the same standalone deploy pattern.
- [Standalone (no-registry) deploys](/docs/how-to/standalone-mode) — how `--build` ships images over SSH without a registry.
- [Polyglot stack](/docs/examples/polyglot-stack) — the next shape up: multiple services, networks, and a reverse proxy.
