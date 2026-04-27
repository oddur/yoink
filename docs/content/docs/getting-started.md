---
title: Getting started
weight: 1
---

## Install

### Homebrew

```sh
brew install oddur/yoink/yoink
```

### Cargo

```sh
cargo install --git https://github.com/oddur/yoink yoink
```

### Pre-built binaries

Grab the right `tar.xz` for your platform from the [latest release](https://github.com/oddur/yoink/releases/latest), or run the curl-installer:

```sh
curl -LsSf https://github.com/oddur/yoink/releases/latest/download/yoink-installer.sh | sh
```

## Five-minute first deploy

The fastest path: **drop a `yoink.yaml` next to your `Dockerfile`, run one command**. No CI, no registry, no pre-existing image.

```yaml
# yoink.yaml — sits alongside your Dockerfile
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

```sh
yoink up --build --no-registry
```

That builds the image locally, streams it directly to `my-server` over ssh (no registry involved), and runs it. Edit the Dockerfile, rerun, watch the new version roll. ~15 seconds for a small image.

This is the **standalone mode** — see [Three deploy modes](/docs/deploy-modes) for when this fits and when it doesn't, plus the kamal-style and CI-built alternatives.

## Prerequisites

- **Docker on the host.** Yoink doesn't bootstrap docker; it expects it already running. The host's deploy user must be in the `docker` group.
- **SSH access from the operator's machine to the host.** Yoink uses `ssh://user@host` via bollard's SSH transport — no docker socket exposed over TCP. Pair with [Tailscale](https://tailscale.com) for stable hostnames + cert-based auth (see [Pairing](/docs/pairing)).
- **For local-build modes**: docker on the operator's machine too, since `yoink build` shells out to `docker build`.
