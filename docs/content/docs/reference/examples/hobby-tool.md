---
title: Hobby tool / utility
description: A yoink.yaml for one host and one service that builds locally with no registry. This is the minimal config.
weight: 1
---

This config is for one container that needs a host, but no CI, no registry, and no Kubernetes. Examples are a Slack bot, an admin CLI, an internal status board, and a cron container.

It is the minimum `yoink.yaml`: one host, one service, and a `Dockerfile` next to the config.

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

The deploy command for this config:

```sh
yoink up --build
```

A deploy of a small image takes approximately 15 seconds. yoink sends the image from your docker daemon to the host over SSH through [unregistry](https://github.com/psviderski/unregistry). Only changed layers go to the host.

## What yoink does without you asking

The container gets the hardened defaults of yoink:

- read-only rootfs
- no Linux caps
- no setuid escalation
- a limit against fork bombs
- tini as PID 1
- healthcheck-gated swap

[Secure by default](/docs/explanation/security) has the full list.

Because the rootfs is read-only, a service that writes files needs a tmpfs:

```yaml
run:
  port: 8080
  options:
    tmpfs:
      /tmp: "size=64m,mode=1777"           # auto-noexec,nosuid,nodev applied
```

## When to graduate

| Need | Limit of this config | Config change |
|---|---|---|
| **Replicas** | One container handles all the requests. | `replicas: 2` runs two containers of the service on each host. |
| **Multiple hosts** | yoink sends the build artifact from your daemon to each host on each deploy. | A [self-hosted tailnet registry](/docs/how-to/self-hosted-registry) lets hosts pull from a shared cache. |
| **CI-triggered deploys** | The config has no registry. | The `build:` block stays, a real registry is added, and the commands become `yoink build --push` and `yoink up`. |

## See also

- [First deploy](/docs/tutorials/first-deploy): the tutorial for the same standalone deploy pattern.
- [Standalone (no-registry) deploys](/docs/how-to/standalone-mode): how `--build` sends images over SSH without a registry.
- [Polyglot stack](/docs/reference/examples/polyglot-stack): the next example, with multiple services, networks, and a reverse proxy.
- [Secure by default](/docs/explanation/security): the reason for each hardened default.
