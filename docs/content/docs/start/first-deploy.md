---
title: First deploy
weight: 2
---

The fastest path: **drop a `yoink.yaml` next to your `Dockerfile`, run one command**. No CI, no registry, no pre-existing image.

{{< callout type="info" >}}
This is the **standalone mode** — see [Three deploy modes](/docs/guide/deploy-modes) when you want CI-built or kamal-style instead.
{{< /callout >}}

The story is **"deploy this repo to this host with these creds."** A fresh VPS (Hetzner, Linode, DigitalOcean, your home lab) with docker installed and an SSH login that works is enough — no registry, no CI, no Tailscale.

## Prerequisites

- Yoink [installed](/docs/start/install) on your laptop.
- A host with docker installed and an SSH login that works without a password — i.e. `ssh root@<host>` (or `ssh deploy@<host>`) drops you in. A fresh Hetzner / Linode / DigitalOcean box with your SSH public key dropped into `~/.ssh/authorized_keys` qualifies.
- Docker running on your laptop (`yoink build` shells out to `docker build`).

That's it. Yoink uses your operating system's SSH client, so anything you've already set up (`~/.ssh/config`, `ssh-agent`, hardware keys, jump hosts) just works.

{{< callout type="info" >}}
**Tailscale is opt-in, not required.** It's a great fit when you want stable hostnames across hosts that move IPs or live behind NAT — see [Pairing](/docs/guide/pairing). For a single VPS, plain SSH to the host's IP or DNS name is the simplest path.
{{< /callout >}}

## Walkthrough

{{% steps %}}

### Generate a `yoink.yaml`

```sh
yoink init root@1.2.3.4              # pass <user>@<host> straight in
# or, omit and yoink will ask if it finds candidate hosts
yoink init
```

That produces a `yoink.yaml` with every field already filled in. Yoink reads what's around the cwd — your `Dockerfile`, `git remote get-url origin`, `~/.ssh/config` — and infers the right answer for each field. Output:

```
✓ wrote yoink.yaml (15 lines, validates clean)

inferred:
  service   my-tool                         (cwd)
  image     ghcr.io/you/my-tool             (git remote)
  host      root@1.2.3.4                    (positional arg)
  port      3000 with /health healthcheck   (Dockerfile EXPOSE)
  user      hono                            (Dockerfile USER)

next:
  yoink validate
  yoink up --tag my-tool=$(git rev-parse HEAD)
```

The summary shows exactly what was inferred and where each value came from — edit the one line in `yoink.yaml` if anything's off. Common overrides via flags: `--service`, `--image`, `--port`, `--no-port`. See [the CLI reference](/docs/reference/cli) for the full surface.

If you don't pass a host but `~/.ssh/config` has candidate entries, yoink prompts you to confirm or override (it never silently picks the first one — that's almost never what you want). On a non-tty (CI), it bails with an instruction to pass the host explicitly.

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

## Different setups

### Fresh VPS, root user, password-only SSH

A brand-new Hetzner / DigitalOcean / Linode box typically gives you a `root` password and no key. Two-step: drop your SSH public key once, then `yoink up`.

```sh
# one-time, from your laptop
ssh-copy-id root@1.2.3.4                  # asks for the root password once
# from then on
yoink up --build --no-registry
```

Yoink itself never asks for a password — it relies on key-based auth via your SSH client. The secret stays on your laptop.

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

### Ship the deploy key with the repo (sealed-secrets)

If you don't want each operator to manage the host's private key in their personal `ssh-agent` — handy for fresh VPS hosts a small team rotates onto, or for CI runners — drop the SSH key into the [sealed-secrets bundle](/docs/recipes/sealed-secrets) and reference it from the host:

```yaml
secrets:
  provider: age
  # `secrets.age` next to yoink.yaml, encrypted with `yoink secrets edit`

hosts:
  - address: 1.2.3.4
    user: root
    ssh_key_secret: PROD_HOST_SSH_KEY
```

`yoink secrets edit` to add the key:
```
PROD_HOST_SSH_KEY=-----BEGIN OPENSSH PRIVATE KEY-----
...
-----END OPENSSH PRIVATE KEY-----
```

At deploy time, yoink decrypts the key into a `0o600` tempfile (auto-removed when the deploy finishes) and uses it for both the bollard daemon connection and the pre-flight `ssh_probe`. The key never lands in your operator's personal ssh-agent.

### Tailscale (opt-in)

If you're already on Tailscale, point `address:` at the tailnet hostname and you're done — no IP allowlist, no DNS, no jump host. See [Pairing](/docs/guide/pairing) for the why.

## What's next

- Other deploy modes (real registry, kamal-style): [Deploy modes](/docs/guide/deploy-modes)
- The `yoink.yaml` schema in full: [Configuration](/docs/reference/config)
- The complete CLI surface: [CLI](/docs/reference/cli)
- Hardened defaults yoink applies to every container: [Secure by default](/docs/guide/security-defaults)
