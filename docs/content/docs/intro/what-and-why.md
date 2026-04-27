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

A small, opinionated container deploy CLI + TUI for people who run a handful of services on a handful of bare-metal hosts. **Low ceremony**: drop a `yoink.yaml` next to your code describing where the app should go, run `yoink up`. **Batteries and best practices included** — the boring-but-important pieces (sealed secrets, hardened container defaults, healthcheck-gated rolling swaps, drift detection, dependency-ordered waves) are all on by default with no plugins to install. Sits between [Kamal](https://kamal-deploy.org) and Kubernetes — opinionated about the same things Kamal is, borrowing the few Kubernetes ideas that actually pay off at this scale.

## Why it exists

Kamal is a lovely fit for "one app, one binary per host" but starts to creak the moment you want a second app on the same box, replicas of the same service, or any kind of network isolation between containers. Kubernetes solves all that — and a hundred other problems you don't have, in exchange for a control plane to operate, a YAML schema with a learning curve, and a vocabulary you have to teach every new operator.

`yoink` is the thinnest tool that gives you the Kubernetes ideas that actually matter at small scale, while keeping Kamal's "one binary, ssh into the host, drive Docker directly" simplicity:

- **Multiple services per host.** Declare them, deploy them, prune the ones that fall out of config.
- **Replicas.** Want two `api` containers behind a reverse proxy? Set `replicas: 2`. Healthcheck-gated rolling swap.
- **Per-service network tiers.** Each service joins a named network; only services on the same network can dial each other. Basic blast-radius isolation without a CNI plugin.
- **Dependency-ordered deploys.** Services declare `depends_on:` and `yoink up` runs them in topological-sort waves (independent services in parallel).
- **Drift detection.** Every effective spec (image, env, networks, mounts, options, file content) hashes deterministically and lands as a label. The TUI shows drift across the cluster without guessing.
- **Three deploy modes.** CI-built (the default), kamal-style local-build with `yoink build --push`, or fully standalone with `yoink up --build --no-registry` — no CI, no registry, drop a `yoink.yaml` next to your Dockerfile and go.
- **Secure by default.** cap_drop=ALL, no-new-privileges, read-only rootfs, init=tini, tmpfs noexec, binds default :ro. Override per service when needed.
- **Sealed secrets out of the box.** A single `secrets.age` file committed to the repo, decrypted at deploy time with one key resolved from `YOINK_AGE_KEY` (env in CI) or `YOINK_AGE_KEY_FILE` (a gitignored `age.key` next to your `yoink.yaml`). No remote vault required. Infisical stays available as an opt-in for teams already running it.
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

- No control plane, no agent on the hosts. yoink is a single Rust binary on your laptop or in CI.
- No service discovery, no scheduling — Docker's network alias DNS is plenty for a few services on a host.
- No multi-cluster, no HA, no auto-scaling. One operator, one config, one deploy at a time.
- No reverse proxy, load balancer, or stateful-accessory orchestration. Use the right tool for each — yoink [pairs cleanly with Tailscale + caddy-docker-proxy](/docs/guide/pairing) for the networking and routing pieces it punts on.
