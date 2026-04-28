---
title: Deploy modes
weight: 2
---

Yoink treats **where the image is built** and **how it gets to the host** as independent choices. Both axes are first-class — pick the combination that fits, mix per-service or per-environment.

**Build origin:**
- **In CI.** GitHub Actions / GitLab / etc. builds and tags the image on every merge. Operator config has no `build:` block — yoink just pulls and rolls. Fits production with an existing CI/CD pipeline.
- **On the operator's machine.** A `build:` block on the service tells yoink to run `docker build` locally before deploying. Same `yoink up` flow either way; a config can mix CI-built infrastructure (`caddy`, `redis`) with locally-built app code. Fits indie / one-person teams and rapid local iteration.

**Distribution:**
- **Via a container registry.** The standard path: `docker push` to a registry (real, self-hosted, or pull-through), `docker pull` from each host. Registry-protocol dedup means only changed layers cross the wire. Fits multi-host production, audit, and tag-based rollback.
- **Direct to host, no registry.** Plain `yoink up` ships any service with a `build:` block straight from the operator's docker daemon to each host over SSH — no registry needed for those services. By default this uses an ephemeral [unregistry](https://github.com/psviderski/unregistry) sidecar so you still get layer-level dedup — only the changed blobs cross the wire on redeploy. Fits rapid iteration, hobby/indie/prototype hosts, and air-gapped environments where opening a registry is overkill.

| Build origin → / Distribution ↓ | In CI | On operator's machine |
|---|---|---|
| **Via registry** | CI pushes, hosts pull. The default for production. | `yoink build --push` then `yoink up` — the classic two-step build+deploy. |
| **Direct to host (no registry)** | Less common, but valid: CI builds then runs `yoink up --transport=unregistry --tag api=<sha>` on a config whose service has a `build:` block (or pass `--no-registry` to force local shipping for non-build services too). | `yoink up --build`, one command. The standalone loop. |

The four cells share a deploy engine — drift detection, healthcheck-gated rolling swap, dependency-ordered waves work the same regardless of how the image arrived. Mixed configs (some services pull from a registry, others ship from local) work out of the box: yoink detects which is which from the `build:` blocks and runs both paths in parallel under a single `yoink up`.

## CI-built — the default

Operator config has no `build:` block; CI builds the image and pushes it to a registry on every merge. `yoink up` pulls and rolls. The image reference is fully qualified (`image: ghcr.io/you/api`) and the tag typically gets overridden at deploy time via `--tag api=<sha>`.

## Local-build (push-then-deploy)

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

`yoink build --push` runs `docker build` against your local docker daemon, tagging as `<image>:<tag>` (which equals the registry path because of how `image:` is configured), then runs `docker push`. After that the standard deploy path takes over.

Two-step instead of one-shot is deliberate: `yoink build && yoink up` keeps the two phases separate so you can build without pushing (for your own CI), push without rebuilding (for re-tagging), or run the steps on different machines (build on a beefy laptop, deploy from a thin runner).

## Standalone (no registry)

The whole loop in one command for a low-ceremony tool repo: drop a `yoink.yaml` next to your `Dockerfile`, describe where the thing should land, run `yoink up --build`. Build, ship, run. No CI to set up, no registry account, no auth dance.

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
yoink up --build                    # build + save+load + run, in one command
```

That's it. Edit Dockerfile, rerun, watch the new version roll. Useful for utilities, internal admin tools, prototypes — anything where the GitHub Actions + container-registry overhead is more friction than the deploy is worth.

If you'd rather keep build and deploy as separate steps (e.g. share the build artifact with another shell, or build on a beefy laptop while deploying from a thin runner), the explicit two-command form still works:

```sh
yoink build my-tool                                # docker build → tag local image as my-tool:dev
yoink up --service my-tool                         # save+load to each host
```

How yoink chooses: any service with a `build:` block is shipped from the operator's local docker daemon over SSH (the only place that image:tag exists). Any service without `build:` is pulled by each host from whatever registry is in its `image:` ref. You don't have to flag the choice — yoink figures it out per service. By default the local-shipping path uses the **unregistry transport** (described below); pass `--transport tarball` to opt out and use the legacy whole-image stream.

If you want to force local-shipping for non-build services too — for offline / airgapped deploys, or to ship a locally-modified version of a public image — pass `--no-registry`. That widens the local-shipping path to every service and skips registry pulls entirely. You usually don't need it.

### How the unregistry transport works (default)

For each host:

1. yoink starts an ephemeral [`ghcr.io/psviderski/unregistry`](https://github.com/psviderski/unregistry) sidecar container on the host. Unregistry is a tiny OCI registry that reads/writes the host's image store directly via the containerd socket — no separate blob storage, images land in `docker images` immediately on push.
2. yoink opens an SSH-tunnelled local port to the sidecar.
3. yoink reads the image bytes from the operator's docker daemon (via `docker save`-style export over the local socket) and pushes blob-by-blob over the SSH tunnel using the standard OCI registry HTTP protocol — `HEAD` first, `PUT` only if missing. **Layer-level dedup means redeploys only ship the layers that actually changed**, the same way pushing to a real registry would.
4. The sidecar is `--rm` and is also force-removed by name on the next `up` run via a label sweep, so a crashed deploy doesn't leak.

The push runs entirely from the operator process — yoink never invokes `docker push` on the operator's daemon. This matters on macOS Docker Desktop (and Rancher Desktop / Colima): the daemon lives in a Linux VM, so a `docker push 127.0.0.1:<port>/...` from the daemon would hit the VM's loopback, not the operator's. By pushing from the operator process directly, yoink works on every platform without `insecure-registries` config.

Multi-host fan-out across services is fully concurrent, so deploy time = max(per-host) instead of sum(per-host).

```
✓ api:dev → host-1     done · unregistry
✓ api:dev → host-2     done · unregistry
✓ web:dev → host-1     done · unregistry
```

If the unregistry setup fails for any reason (host can't pull the unregistry image, ssh forward refused, …), `--transport=auto` (the default) falls back to the tarball transport with a single warning line and continues. Use `--transport=unregistry` to make those failures hard errors instead.

### Tarball transport (opt-out)

```sh
yoink up --build --transport tarball
```

For each (service, host), streams `docker save <image>:<tag>` from the operator's local docker daemon directly into the host's docker daemon via the same SSH-tunnelled Docker API connection that `up` already uses (calling `POST /images/load`). The whole image crosses the wire every deploy — no dedup. Slower than unregistry on redeploy, but has zero dependencies on the host beyond docker. Useful when you can't pull the unregistry image (truly air-gapped hosts) or when you want to debug a transport issue.

Per-host progress bars track bytes transferred + rate live in tarball mode:

```
⠋ api:dev → host-1     234.5 MiB @  47.0 MiB/s (tarball)
✓ web:dev → host-1     done · 89.3 MiB (tarball)
```

{{< callout type="info" >}}
**Standalone mode fits when**: rapid iteration on a single host; hobby/indie/prototype deployments; air-gapped or restricted-network hosts.

**It doesn't fit when**: many hosts (a registry is naturally a hub); you want rollback by tag (registry keeps every pushed tag indefinitely; local docker cache doesn't); auditability matters.
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
