---
title: Build locally and deploy through a registry
description: Build an image on your machine, push it to a registry, and deploy it with yoink.
---

Build a service image with your local docker daemon, push it to a registry, and deploy it. For the reasons to use this mode, see [Deploy modes](/docs/explanation/deploy-modes#local-build-push-then-deploy).

## Prerequisites

- A `Dockerfile` for the service.
- A registry that you can push to. The example uses `ghcr.io`.

## Build, push, and deploy

### Add a `build:` block to the service [step]

Set `image:` to the real registry path.

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

### Log in to the registry [step]

```sh
docker login ghcr.io                 # one-time
```

### Build and push the image [step]

```sh
yoink build --service api --push     # docker build → docker push ghcr.io/you/api:dev
```

yoink runs `docker build`, tags the image as `<image>:<tag>`, and runs `docker push`.

### Deploy the service [step]

```sh
yoink up --service api               # ship from the local daemon → rolling swap
```

Because the service has a `build:` block, yoink sends the image from your local docker daemon to each host over SSH. The host does not pull the image from the registry. Then yoink does a rolling swap.

## See also

- [Deploy modes](/docs/explanation/deploy-modes): why the build and the deploy are two steps.
- [Standalone (no-registry) deploys](/docs/how-to/standalone-mode): the same build without a registry.
- [Self-hosted registry on a yoink host](/docs/how-to/self-hosted-registry): operate your own registry.
- [Configuration: `build:` block](/docs/reference/config): the full schema of the `build:` block.
