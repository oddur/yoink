---
title: First deploy
weight: 2
---

The story is **"deploy this repo to this host with these creds."** A fresh VPS (Hetzner, Linode, Hetzner-Cloud, your home lab) with docker installed and an SSH key authorized is enough — no registry, no CI, no Tailscale.

{{< callout type="info" >}}
This walkthrough uses **standalone mode** — built locally, shipped to the host over SSH. See [Deploy modes](/docs/guide/deploy-modes) when you want CI-built or push-to-registry instead.
{{< /callout >}}

## Prerequisites

- Yoink [installed](/docs/start/install) on your laptop.
- A host with docker installed and an SSH login that works without a password — i.e. `ssh root@<host>` (or `ssh deploy@<host>`) drops you in. A fresh Hetzner / Linode / DigitalOcean box with your SSH public key dropped in `~/.ssh/authorized_keys` qualifies.
- Docker running on your laptop (`yoink build` shells out to `docker build`).

That's it. Yoink uses your operating system's SSH client, so anything you've already set up (`~/.ssh/config`, `ssh-agent`, hardware keys, jump hosts) just works.

{{< callout type="info" >}}
**Tailscale is opt-in, not required.** It's a great fit when you want stable hostnames across hosts that move IPs or live behind NAT — see [Pairing](/docs/guide/pairing). For a single-VPS deploy, plain SSH to the host's IP or DNS name is the simplest path.
{{< /callout >}}

## Walkthrough

{{% steps %}}

### Write a `yoink.yaml`

Drop this next to your `Dockerfile`. The `image:` is a bare name (no registry prefix) — that signals "I'm building locally."

```yaml
# yoink.yaml
hosts:
  - { address: 1.2.3.4, user: root }   # raw IP works; DNS hostname works; tailnet hostname works

services:
  - name: my-tool
    image: my-tool                      # bare name, no registry prefix
    tag: dev
    build:
      context: .                        # `.` = same directory as yoink.yaml
    run:
      port: 8080
```

`address` accepts whatever your SSH client accepts: an IP, a DNS hostname, an `~/.ssh/config` alias, a tailnet hostname. `user` is the SSH user.

### Run one command

```sh
yoink up --build --no-registry
```

That builds the image locally, ships it directly to the host over SSH (no registry involved), and runs it. By default the image push uses an [unregistry](https://github.com/psviderski/unregistry) sidecar transport so only changed layers cross the wire on redeploy — see [Deploy modes](/docs/guide/deploy-modes) for the details and the tarball opt-out.

### Iterate

Edit the Dockerfile or your code. Re-run the same command:

```sh
yoink up --build --no-registry
```

Yoink rebuilds, ships only the changed layers to the host, and rolls the new container through the healthcheck-gated swap. ~5–15 seconds for a small image.

### Inspect what's running

```sh
yoink status                              # snapshot table
yoink tui                                 # k9s-style dashboard
yoink logs my-tool -f | hl                # live tail with `hl` highlighting (optional)
```

{{% /steps %}}

## What happens under the hood

1. **`yoink up --build`** sees that `my-tool` has a `build:` block and runs `docker build` on your laptop, tagging as `my-tool:dev`.
2. **`--no-registry`** ships the image straight to the host. The default transport spins up an ephemeral [unregistry](https://github.com/psviderski/unregistry) sidecar on the host, opens an SSH-tunnelled local port, and pushes layers over the OCI registry protocol — only blobs the host doesn't already have cross the wire.
3. The host's docker daemon now has `my-tool:dev` cached. Yoink's normal reconcile runs — the image is `docker run`'d with the runtime options, and the rolling swap completes after the healthcheck passes.

## Different setups

### Fresh VPS, root user, password-only SSH

A brand-new Hetzner / DigitalOcean / Linode box typically gives you a `root` password and no key. Two-step: drop your SSH public key first, then `yoink up`.

```sh
# one-time, from your laptop
ssh-copy-id root@1.2.3.4                  # asks for the root password once

# from then on
yoink up --build --no-registry
```

Yoink itself never asks for a password — it relies on key-based auth via your SSH client. This keeps the secret on your laptop (in `~/.ssh/`) and out of any config yoink reads.

### Non-default SSH key

Use `~/.ssh/config` to bind a key to the host. Yoink picks it up automatically because it shells out to your `ssh`:

```
# ~/.ssh/config
Host my-server
  HostName 1.2.3.4
  User root
  IdentityFile ~/.ssh/my-server.pem
```

Then in `yoink.yaml`:
```yaml
hosts:
  - { address: my-server, user: root }
```

### Tailscale (opt-in)

If you're already on Tailscale, point `address:` at the tailnet hostname and you're done — no IP allowlist, no DNS, no jump host. See [Pairing](/docs/guide/pairing) for the why.

## What's next

- Other deploy modes (real registry, kamal-style): [Deploy modes](/docs/guide/deploy-modes)
- The `yoink.yaml` schema in full: [Configuration](/docs/reference/config)
- The complete CLI surface: [CLI](/docs/reference/cli)
- Hardened defaults yoink applies to every container: [Secure by default](/docs/guide/security-defaults)
