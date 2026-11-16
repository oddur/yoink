---
title: What & why
description: What yoink does, why it exists, and who it is for.
weight: 1
---

## The 30-second pitch

```sh
yoink up                  # reconcile every service in dep order
yoink tui                 # k9s-style dashboard, drift, logs, shell-into
yoink prune               # clean up stale containers
yoink history api         # who deployed what, when
yoink rollback -s api     # roll back to the previous version
```

yoink is a small, opinionated container deploy CLI and TUI. It is for people who run a small number of services on a small number of bare-metal hosts.

- **Low ceremony.** A `yoink.yaml` file next to your code describes where the app goes. Then `yoink up` deploys it.
- **Batteries and best practices included.** These are on by default, with no plugins to install: sealed secrets, hardened container defaults, healthcheck-gated rolling swaps, drift detection, dependency-ordered waves, and a bundled Caddy reverse proxy.
- **CLI + YAML, no GUI.** Each operation is a subcommand or a config field. It is the same from your laptop, from CI, or from an AI coding agent.

## Why it exists

Single-host PaaS tools are good for "one app, one host". They become difficult when you want a second app on the same host, replicas of one service, or network isolation between tiers. Kubernetes does all of that, and much more that you do not need. The price is a control plane to operate and a YAML schema and vocabulary that each new operator must learn.

`yoink` is the thinnest tool that gives you the small number of Kubernetes ideas that matter at small scale. It stays one binary that connects to the host over SSH and drives Docker directly:

- **Multiple services per host.** You declare the services and deploy them. A prune removes the services that are no longer in the config.
- **Replicas.** For two `api` containers behind a reverse proxy, the config is `replicas: 2`. yoink deploys them with a healthcheck-gated rolling swap.
- **Per-service network tiers.** Each service joins a named network. Only services on the same network can connect to each other. This gives basic blast-radius isolation without a CNI plugin.
- **Dependency-ordered deploys.** Services declare `depends_on:`. `yoink up` runs them in waves in topological-sort order, and independent services run in parallel.
- **Drift detection.** yoink calculates a deterministic hash of each effective spec (image, env, networks, mounts, options, file content). It writes the hash to the container as a label. The TUI reads the label to show drift across the cluster.
- **Build where it makes sense, ship how it makes sense.** You can build on a laptop or in CI. You can ship through a registry or directly to each host. yoink selects the path for each service from the `build:` block, and each path has layer-level dedup. A service without `build:` is CI-built infra, and a service with `build:` is app code. One `yoink up` can mix them. See [deploy modes](/docs/explanation/deploy-modes).
- **Secure by default.** yoink hardens each container that it creates:
  - Non-root uid (`65534` / nobody)
  - `cap_drop=ALL`
  - `no-new-privileges`
  - read-only rootfs
  - `init=tini`
  - tmpfs `noexec`
  - binds default `:ro`

  When an image must have root or specific file ownership, you can override these defaults for that service.
- **Port-forward without a weaker deploy posture.** `yoink pf <service>` opens a kubectl-style tunnel to any container, published or not. Thus the "no `publish:` for backends" default does not stop the on-call person who must point a browser at a service. The TUI has the same function on one key. See [port-forward](/docs/explanation/networking#port-forward).
- **Sealed secrets by default.** One `secrets.age` file is in the repo, and yoink decrypts it at deploy time with one key. In CI, the key comes from the `YOINK_AGE_KEY` env variable. On a laptop, yoink finds the key in `~/.config/yoink/keys/<recipient>.key`, where `yoink secrets key generate` saves it by default. Each project has one identity, and there is no env variable to set. A remote vault is not necessary.
- **Bundled reverse proxy.** When a service has `domain:`, the bundled Caddy of yoink serves it over HTTPS. The certificates are automatic Let's Encrypt certs or sealed Cloudflare origin certs (with optional origin-pull mTLS). h2c for gRPC, HSTS, compression, and multi-host canonical redirects are each a one-line opt-in.
- **CLI + YAML, no GUI.** The full control surface is the `yoink` binary and `yoink.yaml`. The workflow that you run by hand is the same workflow that CI runners and AI coding agents run. Claude Code, Cursor, or a GitHub Actions job can operate yoink.
- **k9s-style TUI.** The TUI is a keyboard-only terminal dashboard with one-key reconcile, prune, kill, shell-into, debug-sidecar, and log filter. It also has deploy history with one-press rollback.

## Batteries included, extensible at the edges

Without yoink, an operator connects the boring but important pieces by hand: hardened defaults, healthcheck-gated swaps, drift detection, dependency ordering, and sealed secrets. yoink includes them. With the age-sealed path, you can ship a real production deploy from a new laptop. You need only `yoink`, ssh access, and a docker daemon on the host. There is no vault to operate and no third-party account to provision.

Where the default is not the correct answer for a team, the team can replace it. Secrets are the primary example:

| | Default (`provider: age`) | Opt-in (`provider: command`) |
|---|---|---|
| Where the values are | committed `secrets.age` in the repo | where your secret manager already keeps them |
| Initial work | `yoink secrets key generate` | configure the CLI of the manager as usual, then point yoink at it |
| When it is the correct choice | small team, single environment, the repo is the trusted source of truth | shared rotation across many repos, audit trail, fine-grained ACLs, secrets that you cannot commit |
| Examples | `secrets.age` + `YOINK_AGE_KEY` | `["doppler","secrets","download","--no-file","--format","env"]`, `["op","inject","-i","secrets.tpl"]`, `["vault","kv","get","-format=json","..."]`, and others |

Because of the `command` path, yoink needs no first-party integration or SDK for each secret manager. Any CLI that writes dotenv or JSON to stdout works: Doppler, 1Password, Vault, AWS Secrets Manager, the Infisical CLI, and others. See [external secrets via CLI](/docs/explanation/secrets) for the model, and [Read secrets from an external secret manager](/docs/how-to/external-secrets) for the config for each tool.

The same pattern applies to registry credentials, CI integrations, and host-level networking. The obvious choice works without config, and you can replace a piece with something specific.

## What it deliberately does not do

- **No control plane, no agent on the hosts.** yoink is a single Rust binary on your laptop or in CI. Hosts run only Docker and sshd.
- **No service discovery across hosts.** The per-host network DNS of Docker is sufficient for a small number of services on one host. Cross-host discovery is your routing decision (for example Tailscale or public DNS).
- **No scheduling or auto-scaling.** yoink deploys to a fixed set of hosts that you declare. Capacity decisions stay with you.
- **No web dashboard or REST API.** All operations go through the CLI and YAML. The TUI is a keyboard-driven inspector, not a control plane.
- **No multi-cluster, HA failover, or geo-distribution.** One operator, one config, one deploy at a time. For multi-region, each region has its own yoink config.
- **No load balancer or geo-routing.** yoink includes a [bundled Caddy reverse proxy](/docs/explanation/proxy) for HTTPS and per-host routing. It does not coordinate traffic across hosts. [Tailscale](/docs/explanation/networking) is a clean way to connect several hosts into one network.

## See also

- [Compared to other tools](/docs/explanation/compared): a side-by-side matrix against Kamal, Coolify, Dokku, Komodo, Kubernetes, and plain compose.
- [First deploy](/docs/tutorials/first-deploy): a five-minute tutorial against a real host.
- [Architecture](/docs/explanation/architecture): the deploy engine in detail.
- [CLI reference](/docs/reference/cli): each subcommand and flag.
