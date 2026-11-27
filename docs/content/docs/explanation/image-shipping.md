---
title: "Image shipping: transports and trade-offs"
description: How yoink ships an image without a registry, how the unregistry and tarball transports work, and where a self-hosted registry fits.
---

A host pulls an image from a registry, or yoink ships the image from your local Docker daemon over SSH. For the model of build origin and distribution, see [Deploy modes](/docs/explanation/deploy-modes).

## How yoink chooses

The choice is per service, and the `build:` block makes it:

| Service | Path |
|---|---|
| Has a `build:` block | yoink ships the image from your local Docker daemon over SSH. |
| Has no `build:` block | Each host pulls the image from the registry in its `image:` ref. |

Two flags change the path:

- `--transport tarball` changes local shipping from the default **unregistry transport** to the whole-image stream.
- `--no-registry` forces local shipping for every service and skips all registry pulls. It serves offline or air-gapped deploys, and a deploy of a locally modified version of a public image.

## How the unregistry transport works (default)

For each host, the sequence is:

1. yoink starts an ephemeral [`ghcr.io/psviderski/unregistry`](https://github.com/psviderski/unregistry) sidecar. Unregistry is an OCI registry that reads and writes the image store of the host through the containerd socket. It has no separate blob storage. Images are in `docker images` immediately after a push.
2. yoink opens a local port with an SSH tunnel to the sidecar.
3. yoink reads the image bytes from your local Docker daemon and pushes each blob through the SSH tunnel. It sends `HEAD` first, and sends `PUT` only if the blob is missing. **This is layer-level dedup: a redeploy ships only the changed layers.**
4. yoink removes the sidecar after the push. The sidecar also runs with `--rm`. On the next `up`, a label sweep force-removes a sidecar that a crashed run left behind. The sweep removes only sidecars that are older than one hour. Thus it does not stop the push of another operator on the same host.

The push runs from the yoink process on your computer, not from `docker push` on your local daemon. On macOS Docker Desktop (and Rancher Desktop and Colima), the daemon lives in a Linux VM. A `docker push 127.0.0.1:<port>/...` from the daemon connects to the loopback of the VM, not to the loopback of your computer. A push from the yoink process needs no `insecure-registries` config.

yoink ships one image to all of its hosts in parallel. The time for one image is max(per-host), not sum(per-host). Different images ship one after the other.

The unregistry transport can fail to start: for example, the host cannot pull the unregistry image, or the host refuses the SSH forward. With `--transport=auto` (the default), yoink then uses the tarball transport and prints one warning line. With `--transport=unregistry`, these failures are hard errors.

## Tarball transport (opt-out)

For each pair of image and host, yoink streams `docker save <image>:<tag>` from your local Docker daemon into the Docker daemon of the host. The stream uses the same SSH-tunnelled Docker API connection that `up` uses (`POST /images/load`). The whole image crosses the wire on every deploy, with no dedup, so a redeploy is slower than with unregistry. The host needs nothing more than Docker.

The tarball transport serves air-gapped hosts that cannot pull the unregistry image. It also helps when you debug a transport failure. A progress bar for each host shows the transferred bytes and the rate.

## When standalone mode is the wrong fit

| Standalone mode fits | Standalone mode does not fit |
|---|---|
| A single host. | Many hosts. A registry is the natural hub. |
| A hobby project or a prototype. | Rollback by tag. A registry keeps every pushed tag, and the local Docker cache does not. |
| Air-gapped hosts, or hosts on a restricted network. | Audit requirements. |

## The self-hosted registry as a middle ground

A self-hosted registry is the middle ground between a real remote registry (for example ghcr.io) and no registry. A `registry:2` container runs as a yoink service on one of your hosts. Tailscale exposes it, and `image:` points at the tailnet hostname. It fits when you want a registry, but you do not want to pay for one or run it on a separate host.

The design works for three reasons:

- **Auth**: the Tailscale ACLs are the auth surface of the registry. If only operators, CI runners, and hosts can reach the registry, no `docker login` or registry password is necessary.
- **Storage**: the `registry-data` volume keeps tags across restarts.
- **Distribution**: every host that you deploy to must be on the same tailnet, or must be able to reach `:5000` on the registry host.

The design has three costs:

- The registry host is a single point of failure for image pulls. If it stops during a deploy, the pull fails on every host that has no local copy of the image. A real cloud registry is the mitigation for prod-critical workloads. The self-hosted registry then serves staging, hobby, and non-critical workloads.
- The registry has no replication. For image distribution across regions, a real registry (Depot, ghcr.io, ECR) is the correct answer.
- The registry has no automatic garbage collection. Old image layers accumulate until you run the [registry GC](https://distribution.github.io/distribution/about/garbage-collection/).

## See also

- [Standalone (no-registry) deploys](/docs/how-to/standalone-mode): the commands for a deploy without a registry.
- [Self-hosted registry on a yoink host](/docs/how-to/self-hosted-registry): the steps to run `registry:2` as a yoink service.
- [Deploy modes](/docs/explanation/deploy-modes): the model of build origin and distribution.
- [CLI reference](/docs/reference/cli): the `--transport` and `--no-registry` flags.
