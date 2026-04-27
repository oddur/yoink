---
title: Three deploy modes
weight: 2
---

Yoink supports three ways of getting a container image to a host. Pick the one that fits your setup; they're not mutually exclusive (mix per-service or per-environment).

| | Image origin | Build | Distribution | When it fits |
|---|---|---|---|---|
| **CI-built (default)** | CI builds + pushes on every commit | external | `docker pull` from registry | Production with multiple hosts and an existing CI/CD pipeline |
| **Local-build, kamal-style** | Operator's machine | `yoink build --push` | `docker pull` from registry | Indie / one-person team that wants the full deploy loop in one tool, willing to keep a registry |
| **Standalone (no registry)** | Operator's machine | `yoink up --build --no-registry` (one command) | `docker save \| docker load` over ssh | Rapid iteration on a single host; air-gapped; low-ceremony tools/utilities |

Pick by service if you want — `yoink build` only runs against services with a `build:` block, so a config can mix CI-built infrastructure (`caddy`, `redis`) with locally-built app code.

## CI-built — the default

Operator config has no `build:` block; CI builds the image and pushes it to a registry on every merge. `yoink up` pulls and rolls. The image reference is fully qualified (`image: ghcr.io/you/api`) and the tag typically gets overridden at deploy time via `--tag api=<sha>`.

## Local-build, kamal-style

```yaml
services:
  - name: api
    image: ghcr.io/you/api          # real registry path
    tag: dev
    build:
      context: .
      dockerfile: Dockerfile
      args:
        RUST_VERSION: "1.95"
    run:
      port: 8080
```

```sh
docker login ghcr.io                 # one-time
yoink build api --push               # docker build → docker push ghcr.io/you/api:dev
yoink up --service api               # docker pull on each host → rolling swap
```

`yoink build --push` runs `docker build` against your local docker daemon, tagging as `<image>:<tag>` (which equals the registry path because of how `image:` is configured), then runs `docker push`. After that the standard deploy path takes over — exactly like kamal, except the build step is a separate command (so you can build without pushing for CI of your own; or push without rebuilding for re-tagging).

Two-step instead of one-shot is deliberate: `yoink build && yoink up` is the explicit version of `kamal deploy`, and the separation lets you run them on different machines (build on a beefy laptop, deploy from a thin runner) when you want to.

## Standalone (no registry)

The whole loop in one command for a low-ceremony tool repo: drop a `yoink.yaml` next to your `Dockerfile`, describe where the thing should land, run `yoink up --build --no-registry`. Build, ship, run. No CI to set up, no registry account, no auth dance.

```yaml
# yoink.yaml — sits alongside the Dockerfile in your repo
hosts:
  - { address: my-server, user: deploy }   # tailnet hostname

services:
  - name: my-tool
    image: my-tool                  # bare name — no registry prefix
    tag: dev
    build:
      context: .                    # `.` = same directory as yoink.yaml
    run:
      port: 8080
```

```sh
yoink up --build --no-registry      # build + save+load + run, in one command
```

That's it. Edit Dockerfile, rerun, watch the new version roll. Useful for utilities, internal admin tools, prototypes — anything where the GitHub Actions + container-registry overhead is more friction than the deploy is worth.

If you'd rather keep build and deploy as separate steps (e.g. share the build artifact with another shell, or build on a beefy laptop while deploying from a thin runner), the explicit two-command form still works:

```sh
yoink build my-tool                                # docker build → tag local image as my-tool:dev
yoink up --no-registry --service my-tool           # save+load to each host
```

What `--no-registry` does: for every (service, host) the deploy targets, yoink streams `docker save <image>:<tag>` from the operator's local docker daemon directly into the host's docker daemon via the same ssh+bollard transport that `up` already uses (calling `POST /images/load`). After the load completes, the host has the image cached and the rest of the deploy flow (which already short-circuits when an image is locally present) runs unchanged — healthcheck-gated rolling swap, drift detection, the works.

Per-host progress bars track bytes transferred + rate live:

```
⠋ api:dev → backtrack-eu-1     234.5 MiB @  47.0 MiB/s
⠋ api:dev → backtrack-eu-2      56.0 MiB @  11.2 MiB/s
✓ web:dev → backtrack-eu-1     done · 89.3 MiB
```

Multi-host fan-out is fully concurrent (`try_join_all` per image): each host gets its own `docker save` process + its own bollard connection, so deploy time = max(per-host) instead of sum(per-host).

{{< callout type="info" >}}
**Standalone mode fits when**: rapid iteration on a single host; hobby/indie/prototype deployments; air-gapped or restricted-network hosts.

**It doesn't fit when**: many hosts (N hosts = N save+load streams; a registry is a hub); large images (every deploy ships the full tarball over ssh per host); you want rollback by tag (registry keeps every pushed tag indefinitely; local docker cache doesn't); auditability matters.
{{< /callout >}}

## Self-hosted registry as a yoink service

The middle ground between "real remote registry" and "no registry at all": run `registry:2` as a yoink-managed service on one of your hosts, expose it via tailscale, point `image:` at the tailnet hostname.

```yaml
services:
  - name: registry
    image: registry
    tag: "2"
    networks: [registry]
    run:
      port: 5000
      volumes:
        - "registry-data:/var/lib/registry"
      options:
        memory: "256Mi"

  - name: api
    image: registry.my-tailnet.ts.net:5000/api
    tag: dev
    build:
      context: .
```

Then `yoink build api --push` pushes to your tailnet registry; `yoink up --service api` pulls from it. Tailscale ACLs are the registry's auth surface — no `docker login` / registry password needed if your tailnet is correctly scoped. Persistent storage via the `registry-data` volume.

Best for "I want a registry but I don't want to pay for one and I don't want to run it on a separate machine."
