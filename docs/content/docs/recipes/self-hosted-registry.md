---
title: Self-hosted registry on a yoink host
weight: 11
---

The middle ground between "real remote registry" (ghcr.io, etc.) and "no registry at all" (services with a `build:` block, shipped from your local docker daemon by plain `yoink up`): run a `registry:2` container as a yoink-managed service on one of your hosts, expose it via tailscale, and point `image:` at the tailnet hostname.

Best for "I want a registry but I don't want to pay for one and I don't want to run it on a separate machine."

## Setup

{{% steps %}}

### Declare the registry as just another service in your `yoink.yaml`

```yaml
services:
  - name: registry
    image: registry
    tag: "2"
    networks: [registry]
    run:
      port: 5000
      volumes:
        - "registry-data:/var/lib/registry"   # persistent storage
      options:
        memory: "256Mi"
```

### Bring it up

```sh
yoink up --service registry
```

### Point your app's `image:` at the tailnet hostname

```yaml
services:
  - name: api
    image: registry.my-tailnet.ts.net:5000/api    # tailnet-routed registry path
    tag: dev
    build:
      context: .
```

### Use it like any registry

```sh
yoink build api --push          # docker build → docker push to your tailnet registry
yoink up --service api          # docker pull from the tailnet registry on each host
```

{{% /steps %}}

## Why this works

- **Auth**: tailscale ACLs are the registry's auth surface. If your tailnet is correctly scoped (only operators + CI runners + hosts can reach it), no `docker login` / registry password needed.
- **Storage**: the `registry-data` volume keeps tags across restarts.
- **Distribution**: every host you deploy to has to be on the same tailnet (or be reachable to the registry host's `:5000`). Yoink's existing tailnet-required transport already handles this.

## Caveats

- The registry host is a single point of failure for image pulls. If it dies during a deploy, every host that doesn't already have the image cached locally fails to pull. Mitigation: pin to a real cloud registry for prod-critical workloads, use the self-hosted registry for staging / hobby / non-critical.
- No replication. If you need image distribution across regions, this isn't the right answer — use a real registry (Depot, ghcr.io, ECR).
- No automatic garbage collection. Old image layers accumulate; periodically run the [registry's GC](https://distribution.github.io/distribution/about/garbage-collection/) inside the container.

## See also

- [Three deploy modes](/docs/guide/deploy-modes) — when registry vs. registry-less (`build:` blocks shipped via unregistry over SSH) makes sense.
- [Multi-host distribution](/docs/guide/networking#multi-host-distribution) — pinning services to specific hosts when you've split images across registries.
