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

### Generate a `yoink.yaml`

```sh
yoink init my-server
```

That produces a `yoink.yaml` with every field already filled in. Yoink reads what's around the cwd — your `Dockerfile`, `git remote get-url origin`, `~/.ssh/config` — and infers the right answer for each field. Output looks like:

```
✓ wrote yoink.yaml (15 lines, validates clean)

inferred:
  service   my-tool                         (cwd)
  image     ghcr.io/you/my-tool             (git remote)
  host      deploy@my-server                (positional arg)
  port      3000 with /health healthcheck   (Dockerfile EXPOSE)
  user      hono                            (Dockerfile USER)

next:
  yoink validate
  yoink up --tag my-tool=$(git rev-parse HEAD)
```

The summary tells you exactly what was inferred and where each value came from. If something's wrong, edit the one line in `yoink.yaml`. Common overrides via flags: `--service`, `--image`, `--port`, `--no-port`. See [the CLI reference](/docs/reference/cli) for the full surface.

If the host can't be inferred from `~/.ssh/config`, pass it as a positional arg as shown. `--interactive` engages stdio prompts as a fallback.

For this walkthrough we'll change one line: edit `image:` to a bare name (no registry prefix) so we can use **standalone mode** — build locally, ship directly to the host, no registry involved:

```yaml
services:
  - name: my-tool
    image: my-tool       # bare name, no registry prefix
    tag: dev
    build:
      context: .         # build the Dockerfile next to this yoink.yaml
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
- Hardened defaults yoink applies to every container: [Secure by default](/docs/guide/security-defaults)
