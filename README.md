# yoink

A small, opinionated container deploy CLI + TUI for people who run a handful of services on a handful of bare-metal hosts. Sits between Kamal and Kubernetes — opinionated about the same things Kamal is, borrowing the few Kubernetes ideas that actually pay off at this scale.

```
yoink up                  # reconcile every service in dep order
yoink tui                 # k9s-style dashboard, drift, logs, shell-into
yoink prune               # clean up stale containers
yoink history api         # who deployed what, when
yoink rollback api        # roll back to the previous version
```

📚 **Full docs: <https://oddur.github.io/yoink/>**

## Why it exists

Kamal is a lovely fit for "one app, one binary per host" but starts to creak the moment you want a second app on the same box, replicas of the same service, or any kind of network isolation between containers. Kubernetes solves all that — and a hundred other problems you don't have, in exchange for a control plane to operate, a YAML schema with a learning curve, and a vocabulary you have to teach every new operator.

`yoink` is the thinnest tool that gives you the Kubernetes ideas that actually matter at small scale, while keeping Kamal's "one binary, ssh into the host, drive Docker directly" simplicity:

- **Multiple services per host** with replicas + dependency-ordered deploys (`depends_on:` topo-sort)
- **Per-service network tiers** for blast-radius isolation without a CNI plugin
- **Drift detection.** Every effective spec hashes deterministically and lands as a label. The TUI shows drift across the cluster without guessing.
- **Three deploy modes**: CI-built (the default), local-build kamal-style with `yoink build --push`, or fully standalone with `yoink up --build --no-registry` (no CI, no registry — drop a `yoink.yaml` next to your Dockerfile and go)
- **Sane security defaults**: cap_drop=ALL, no-new-privileges, read-only rootfs, init=tini, tmpfs noexec, binds default :ro. Override per service when needed.
- **k9s-style TUI** with deploy history, one-press rollback, drift cells, logs auto-piped through `hl`

## Quick start

```sh
brew install oddur/yoink/yoink
```

Drop a `yoink.yaml` next to your `Dockerfile`:

```yaml
hosts:
  - { address: my-server, user: deploy }   # tailnet hostname
services:
  - name: my-tool
    image: my-tool
    tag: dev
    build: { context: . }
    run: { port: 8080 }
```

```sh
yoink up --build --no-registry
```

That builds the image locally, streams it directly to the host over ssh, and runs it. See [Getting started](https://oddur.github.io/yoink/docs/getting-started) for the other modes (CI-built, kamal-style with a real registry, self-hosted tailnet registry).

## Documentation

The full reference lives at **<https://oddur.github.io/yoink/>**:

- [Getting started](https://oddur.github.io/yoink/docs/getting-started) — install + first deploy
- [Three deploy modes](https://oddur.github.io/yoink/docs/deploy-modes) — pick the one that fits your setup
- [Sane security defaults](https://oddur.github.io/yoink/docs/security-defaults) — what yoink hardens, and how to opt out
- [Pairing with Tailscale + caddy](https://oddur.github.io/yoink/docs/pairing) — what yoink owns vs. what it doesn't
- [Configuration](https://oddur.github.io/yoink/docs/configuration) — the `yoink.yaml` schema
- [CLI](https://oddur.github.io/yoink/docs/cli) — every subcommand and flag
- [TUI](https://oddur.github.io/yoink/docs/tui) — keybinds, panes, deploy history
- [Recipes](https://oddur.github.io/yoink/docs/recipes) — staging alongside prod, self-hosted registry, Infisical

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

## Build & test

```bash
cargo build --release                # binary lands at target/release/yoink
cargo test --workspace               # unit + integration tests
cargo clippy --all-targets -- -D warnings
```

## Status

In production use on a small fleet. Pre-1.0 and not (yet) widely adopted, so no semver-stability promise — but the core surface (`up`, `status`, `rollback`, the YAML schema) is unlikely to break.

## License

MIT — see [LICENSE](LICENSE).
