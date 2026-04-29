---
title: Standalone (no-registry) deploys
weight: 15
---

Build, ship, and run a service in one `yoink up --build` — no CI, no registry, no auth dance. The default transport is [unregistry](https://github.com/psviderski/unregistry) (layer-level dedup over SSH); a tarball fallback handles air-gapped hosts. For the mental model behind the build-origin × distribution split, see [Deploy modes](/docs/guide/deploy-modes).

## The minimum config

Drop a `yoink.yaml` next to your `Dockerfile`:

```yaml
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
yoink up --build                    # build + ship + run, in one command
```

That's it. Edit Dockerfile, rerun, watch the new version roll. Useful for utilities, internal admin tools, prototypes — anything where the GitHub Actions + container-registry overhead is more friction than the deploy is worth.

If you'd rather keep build and deploy as separate steps (e.g. share the build artifact with another shell, or build on a beefy laptop while deploying from a thin runner), the explicit two-command form still works:

```sh
yoink build my-tool                                # docker build → tag local image as my-tool:dev
yoink up --service my-tool                         # save+load to each host
```

## How yoink chooses

Any service with a `build:` block is shipped from the operator's local docker daemon over SSH (the only place that image:tag exists). Any service without `build:` is pulled by each host from whatever registry is in its `image:` ref. You don't have to flag the choice — yoink figures it out per service.

By default the local-shipping path uses the **unregistry transport** (described below); pass `--transport tarball` to opt out and use the legacy whole-image stream.

If you want to force local-shipping for non-build services too — for offline / airgapped deploys, or to ship a locally-modified version of a public image — pass `--no-registry`. That widens the local-shipping path to every service and skips registry pulls entirely. You usually don't need it.

## How the unregistry transport works (default)

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

## Tarball transport (opt-out)

```sh
yoink up --build --transport tarball
```

For each (service, host), streams `docker save <image>:<tag>` from the operator's local docker daemon directly into the host's docker daemon via the same SSH-tunnelled Docker API connection that `up` already uses (calling `POST /images/load`). The whole image crosses the wire every deploy — no dedup. Slower than unregistry on redeploy, but has zero dependencies on the host beyond docker. Useful when you can't pull the unregistry image (truly air-gapped hosts) or when you want to debug a transport issue.

Per-host progress bars track bytes transferred + rate live in tarball mode:

```
⠋ api:dev → host-1     234.5 MiB @  47.0 MiB/s (tarball)
✓ web:dev → host-1     done · 89.3 MiB (tarball)
```

## When standalone mode is the wrong fit

{{< callout type="info" >}}
**Standalone mode fits when**: rapid iteration on a single host; hobby/indie/prototype deployments; air-gapped or restricted-network hosts.

**It doesn't fit when**: many hosts (a registry is naturally a hub); you want rollback by tag (registry keeps every pushed tag indefinitely; local docker cache doesn't); auditability matters.
{{< /callout >}}

For the middle ground — your own registry without paying for one — see [Self-hosted registry on a yoink host](/docs/how-to/self-hosted-registry).

## See also

- [Deploy modes guide](/docs/guide/deploy-modes) — the build-origin × distribution mental model.
- [Self-hosted registry on a yoink host](/docs/how-to/self-hosted-registry) — when standalone is too thin.
- [Edit-save-deploy with `--watch`](/docs/how-to/watch-mode) — pair with standalone for the tightest iteration loop.
