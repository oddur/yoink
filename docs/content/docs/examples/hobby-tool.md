---
title: Hobby tool / utility
weight: 1
---

The minimum viable `yoink.yaml`: one host, one service, `Dockerfile` next to the config, no registry. Drop in a repo, `yoink up --build`, done.

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

That's it. ~15 seconds for a small image. The `build:` block tells yoink the image is local-only, so it ships from your docker daemon to the host over SSH (via [unregistry](https://github.com/psviderski/unregistry) by default — only changed layers cross the wire).

## What yoink does for you (without you asking)

Even at this minimum size your container inherits yoink's hardened defaults — read-only rootfs, no Linux caps, no setuid escalation, fork-bomb bound, tini as PID 1, healthcheck-gated swap. See [secure by default](/docs/guide/security-defaults) for the full list and per-field rationale.

If your `my-tool` actually needs to write somewhere, give it a tmpfs:

```yaml
run:
  port: 8080
  options:
    tmpfs:
      /tmp: "size=64m,mode=1777"           # auto-noexec,nosuid,nodev applied
```

## When to graduate

This shape works fine forever for hobby / utility / internal-tool deployments. You'd outgrow it when:

- **You need replicas** — a single container's downtime during the swap is your downtime. Add `replicas: 2` (yoink does a rolling swap, capacity stays N-1).
- **You need multiple hosts** — yoink ships the build artifact from your local daemon to every host on every deploy. Once that's painful, set up a [self-hosted tailnet registry](/docs/recipes/self-hosted-registry) so the hosts pull from a shared cache instead.
- **You need deploys triggered from CI** — keep the `build:` block + add a real registry, switch to `yoink build --push` + `yoink up`.
