---
title: First deploy
weight: 2
---

The fastest path: **drop a `yoink.yaml` next to your `Dockerfile`, run one command**. No CI, no registry, no pre-existing image.

{{< callout type="info" >}}
This is the **standalone mode** — see [Three deploy modes](/docs/guide/deploy-modes) when you want CI-built or kamal-style instead.
{{< /callout >}}

## Prerequisites

- Yoink [installed](/docs/start/install)
- A host you can ssh into (a tailnet hostname is ideal — see [Pairing](/docs/guide/pairing))
- Docker running on the host, deploy user in the `docker` group
- Docker on the operator's machine (`yoink build` shells out to `docker build`)

## Walkthrough

{{% steps %}}

### Write a `yoink.yaml`

Drop this next to your `Dockerfile`. The `image:` is a bare name (no registry prefix) — that signals "I'm building locally."

```yaml
# yoink.yaml
hosts:
  - { address: my-server, user: deploy }   # tailnet hostname

services:
  - name: my-tool
    image: my-tool                         # bare name, no registry prefix
    tag: dev
    build:
      context: .                           # `.` = same directory as yoink.yaml
    run:
      port: 8080
```

### Run one command

```sh
yoink up --build --no-registry
```

That builds the image locally, streams it directly to `my-server` over ssh (no registry involved), and runs it.

### Iterate

Edit the Dockerfile or your code. Re-run the same command:

```sh
yoink up --build --no-registry
```

Yoink rebuilds, ships the new image to the host, and rolls it through the healthcheck-gated swap. ~15 seconds for a small image.

### Inspect what's running

```sh
yoink status                              # snapshot table
yoink tui                                 # k9s-style dashboard
yoink logs my-tool -f | hl                # live tail with `hl` highlighting (optional)
```

{{% /steps %}}

## What happens under the hood

1. **`yoink up --build`** sees that `my-tool` has a `build:` block and runs `docker build` locally, tagging as `my-tool:dev`.
2. **`--no-registry`** spawns `docker save my-tool:dev` and streams the tarball over the existing ssh+bollard transport into the host's docker daemon (`POST /images/load`). Per-host progress bars show bytes + rate.
3. The host's docker daemon now has `my-tool:dev` cached. Yoink's normal reconcile runs — `image_present` short-circuits the would-be `docker pull`, the image gets `docker run`'d with the runtime options, and the rolling swap completes after the healthcheck passes.

## What's next

- Other deploy modes (real registry, kamal-style): [Three deploy modes](/docs/guide/deploy-modes)
- The `yoink.yaml` schema in full: [Configuration](/docs/reference/config)
- The complete CLI surface: [CLI](/docs/reference/cli)
- Hardened defaults yoink applies to every container: [Sane security defaults](/docs/guide/security-defaults)
