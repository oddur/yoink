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

Even at this minimum size, the container yoink creates is hardened:

- **read-only rootfs** (default `read_only: true`) — exploits can't drop binaries on disk
- **no Linux caps** (default `cap_drop: [ALL]`) — `CAP_NET_ADMIN`, `CAP_SYS_ADMIN`, … all dropped
- **no setuid escalation** (default `security_opt: [no-new-privileges:true]`)
- **`pids_limit: 1024`** — fork-bomb bound
- **tini as PID 1** (default `init: true`) — zombie reaping + proper SIGTERM forwarding
- **healthcheck-gated swap** — yoink runs a TCP-connect probe on port 8080 before declaring the new container live

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
