---
title: What & why
weight: 1
---

## The 30-second pitch

```
yoink up                  # reconcile every service in dep order
yoink tui                 # k9s-style dashboard, drift, logs, shell-into
yoink prune               # clean up stale containers
yoink history api         # who deployed what, when
yoink rollback api        # roll back to the previous version
```

A small, opinionated container deploy CLI + TUI for people who run a handful of services on a handful of bare-metal hosts. **Low ceremony**: drop a `yoink.yaml` next to your code describing where the app should go, run `yoink up`. **Batteries and best practices included** — the boring-but-important pieces (sealed secrets, hardened container defaults, healthcheck-gated rolling swaps, drift detection, dependency-ordered waves, a bundled Caddy reverse proxy) are all on by default with no plugins to install. **CLI + YAML, no GUI** — everything is a subcommand or a config field, identical from your laptop, CI, or an AI coding agent.

## Why it exists

Single-host PaaS tools are wonderful for "one app, one host" but creak the moment you want a second app on the same box, replicas of the same service, or network isolation between tiers. Kubernetes solves all that — and a hundred other problems you don't have, in exchange for a control plane to operate, a YAML schema with a learning curve, and a vocabulary you have to teach every new operator.

`yoink` is the thinnest tool that gives you the few Kubernetes ideas that actually matter at small scale, while keeping the "one binary, ssh into the host, drive Docker directly" simplicity:

- **Multiple services per host.** Declare them, deploy them, prune the ones that fall out of config.
- **Replicas.** Want two `api` containers behind a reverse proxy? Set `replicas: 2`. Healthcheck-gated rolling swap.
- **Per-service network tiers.** Each service joins a named network; only services on the same network can dial each other. Basic blast-radius isolation without a CNI plugin.
- **Dependency-ordered deploys.** Services declare `depends_on:` and `yoink up` runs them in topological-sort waves (independent services in parallel).
- **Drift detection.** Every effective spec (image, env, networks, mounts, options, file content) hashes deterministically and lands as a label. The TUI shows drift across the cluster without guessing.
- **Build where it makes sense, ship how it makes sense.** Yoink treats build origin and distribution as independent choices. Build on a developer laptop OR in CI (both first-class — same `yoink up`). Ship from a real container registry OR push the locally-built image straight to each host with no registry in between. **Incremental layer push works on both paths**: a real registry dedupes natively, and the registry-less path (`--no-registry`) uses an ephemeral [unregistry](https://github.com/psviderski/unregistry) sidecar to get the same layer-level dedup over SSH. Pick whichever combination fits — drop the `build:` block for CI-built infra, keep it for app code, mix freely.
- **Secure by default.** Containers run as a non-root uid (65534) with cap_drop=ALL, no-new-privileges, read-only rootfs, init=tini, tmpfs noexec, binds default :ro. Override per service when an image genuinely needs root or specific file ownership.
- **Sealed secrets out of the box.** A single `secrets.age` file committed to the repo, decrypted at deploy time with one key resolved from `YOINK_AGE_KEY` (env in CI) or `YOINK_AGE_KEY_FILE` (a gitignored `age.key` next to your `yoink.yaml`). No remote vault required. Infisical stays available as an opt-in for teams already running it.
- **Bundled reverse proxy.** Set `domain:` on a service and yoink's bundled Caddy fronts it with HTTPS — automatic Let's Encrypt or sealed Cloudflare origin certs (with optional origin-pull mTLS). h2c for gRPC, HSTS, compression, multi-host canonical redirects — all one-line opt-ins.
- **CLI + YAML, no GUI.** The entire control surface is the `yoink` binary plus `yoink.yaml`. No web dashboard to click, no API to script. The same workflow that you run by hand drives CI runners and AI coding agents identically — yoink is happy to be driven by Claude Code, Cursor, or a GitHub Actions job.
- **k9s-style TUI.** A `ratatui` dashboard with one-key reconcile, prune, kill, shell-into, debug-sidecar, log filter, deploy history with one-press rollback. Keyboard-only.

## Batteries included, extensible at the edges

Yoink picks "batteries included" over "framework" for the boring-but-important pieces an operator would otherwise have to glue together themselves: hardened container defaults, healthcheck-gated swaps, drift detection, dependency ordering, and **sealed secrets**. The age-sealed path means you can ship a real production deploy from a fresh laptop with nothing more than `yoink` itself, ssh access, and a docker daemon on the host — no vault to operate, no third-party account to provision.

Where the in-the-box default isn't the right answer for a team, yoink extends rather than blocks. **Secrets** is the canonical example:

| | Default (`provider: age`) | Opt-in (`provider: infisical`) |
|---|---|---|
| Where the values live | committed `secrets.age` in the repo | external Infisical instance |
| Setup | `yoink secrets keygen` | machine identity, project, env config |
| When it's the right choice | small team, single environment, trust the repo as the source of truth | already running Infisical, want a UI for non-engineers, need centralized audit |

The schema is an enum (`SecretsConfig::Age` / `SecretsConfig::Infisical`), so future providers (Vault, AWS Secrets Manager, …) plug in as additional variants without changing the per-service `secrets:` surface. Existing configs keep working as new providers land.

The same shape applies elsewhere: registry credentials, CI integrations, host-level networking. Yoink's job is to make the obvious choice work without configuration, and to stay out of the way when the operator needs to swap a piece for something specific.

## What it deliberately doesn't do

- **No control plane, no agent on the hosts.** yoink is a single Rust binary on your laptop or in CI. Hosts run nothing but Docker + sshd.
- **No service discovery beyond a host.** Docker's per-host network DNS is plenty for a few services on a box. Cross-host discovery is your routing problem (Tailscale, public DNS, etc.).
- **No scheduling or auto-scaling.** Yoink deploys to a fixed set of hosts you declare; capacity decisions stay with you.
- **No web dashboard or REST API.** Everything happens through the CLI and YAML. The TUI is a keyboard-driven inspector, not a control plane.
- **No multi-cluster, HA failover, or geo-distribution.** One operator, one config, one deploy at a time. For multi-region, run separate yoink configs per region.
- **No load balancer or geo-routing.** Yoink ships a [bundled Caddy reverse proxy](/docs/guide/proxy) for HTTPS + per-host routing, but it does not coordinate traffic across hosts. [Tailscale](/docs/guide/pairing) is a clean way to bridge several hosts into one network.
