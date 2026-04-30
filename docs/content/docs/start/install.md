---
title: Install
weight: 1
---

Yoink is a single self-contained binary — no daemon, no config bootstrap, no runtime. All four install paths produce the same `yoink` on your `PATH`.

{{< tabs items="Homebrew,Cargo,curl,Pre-built binary" >}}

  {{< tab >}}

```sh
brew install oddur/yoink/yoink
```

The Homebrew tap (`oddur/homebrew-yoink`) is auto-updated by every release. `brew upgrade oddur/yoink/yoink` rolls forward.

  {{< /tab >}}

  {{< tab >}}

```sh
cargo install --git https://github.com/oddur/yoink yoink
```

Builds from source. Useful when you want the latest `main` rather than a tagged release. MSRV is 1.95.

  {{< /tab >}}

  {{< tab >}}

```sh
curl -LsSf https://github.com/oddur/yoink/releases/latest/download/yoink-installer.sh | sh
```

The cargo-dist generated installer. Fetches the right `tar.xz` for your platform, drops the binary in `~/.cargo/bin/`.

  {{< /tab >}}

  {{< tab >}}

Grab the `tar.xz` for your platform from the [latest release](https://github.com/oddur/yoink/releases/latest):

- `yoink-aarch64-apple-darwin.tar.xz` — macOS Apple Silicon
- `yoink-x86_64-apple-darwin.tar.xz` — macOS Intel
- `yoink-x86_64-unknown-linux-gnu.tar.xz` — Linux x86_64

Extract and put the binary on `$PATH`.

  {{< /tab >}}

{{< /tabs >}}

## Verify

```sh
yoink --version
```

## Prerequisites for actual deploys

Yoink doesn't bootstrap docker; it expects:

- **Docker on the host(s)** — installed, running, and the deploy user is in the `docker` group
- **SSH access from the operator's machine to each host** — yoink uses `ssh://user@host` to reach the docker daemon, no docker socket exposed over TCP
- **For local-build workflows**: docker on the operator's machine too (`yoink build` shells out to `docker build`)

[Tailscale](https://tailscale.com) pairs especially cleanly with the SSH transport — see [Pairing](/docs/guide/networking).

## Next: your first deploy

Yoink installed, host with Docker + SSH ready? Head to [**Five-minute first deploy**](/docs/start/first-deploy) — two commands take you from a fresh VPS to a running app behind a healthcheck-gated rolling deploy.

## Upgrading

```sh
brew upgrade oddur/yoink/yoink              # Homebrew
cargo install --git https://github.com/oddur/yoink yoink --force   # cargo
curl -LsSf https://github.com/oddur/yoink/releases/latest/download/yoink-installer.sh | sh   # installer script
```

Release notes for every version are on the [GitHub releases page](https://github.com/oddur/yoink/releases). Yoink follows SemVer; minor bumps may add new fields but won't break existing `yoink.yaml` configs.

## See also

- [First deploy](/docs/start/first-deploy) — drop a `yoink.yaml`, run one command.
- [Hetzner cx23 quickstart with HTTPS](/docs/how-to/hetzner-quickstart) — empty cloud account → live HTTPS in 90 seconds.
