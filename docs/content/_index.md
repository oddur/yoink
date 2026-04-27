---
title: yoink
toc: false
---

A small, opinionated container deploy CLI + TUI for people who run a handful of services on a handful of bare-metal hosts. Sits between [Kamal](https://kamal-deploy.org) and Kubernetes — opinionated about the same things Kamal is, borrowing the few Kubernetes ideas that actually pay off at this scale.

```
yoink up                  # reconcile every service in dep order
yoink tui                 # k9s-style dashboard, drift, logs, shell-into
yoink prune               # clean up stale containers
yoink history api         # who deployed what, when
yoink rollback api        # roll back to the previous version
```

{{< cards >}}
  {{< card link="/docs/getting-started" title="Getting started" subtitle="Install, drop a yoink.yaml in your repo, deploy." icon="lightning-bolt" >}}
  {{< card link="/docs/deploy-modes" title="Three deploy modes" subtitle="CI-built, kamal-style local-build, no-registry standalone." icon="server" >}}
  {{< card link="/docs/security-defaults" title="Sane security defaults" subtitle="cap_drop=ALL, read-only rootfs, no-new-privileges, etc." icon="shield-check" >}}
  {{< card link="/docs/pairing" title="Pairing with Tailscale + caddy-docker-proxy" subtitle="What yoink owns vs. what it leaves to other tools." icon="link" >}}
  {{< card link="/docs/cli" title="CLI reference" subtitle="Every subcommand and flag." icon="terminal" >}}
  {{< card link="/docs/tui" title="TUI guide" subtitle="Keybinds, panes, drift detection, deploy history." icon="desktop-computer" >}}
{{< /cards >}}

## Why it exists

Kamal is a lovely fit for "one app, one binary per host" but starts to creak the moment you want a second app on the same box, replicas of the same service, or any kind of network isolation between containers. Kubernetes solves all that — and a hundred other problems you don't have, in exchange for a control plane to operate, a YAML schema with a learning curve, and a vocabulary you have to teach every new operator.

`yoink` is the thinnest tool that gives you the Kubernetes ideas that actually matter at small scale, while keeping Kamal's "one binary, ssh into the host, drive Docker directly" simplicity:

- **Multiple services per host.** Declare them, deploy them, prune the ones that fall out of config.
- **Replicas.** Want two `api` containers behind a reverse proxy? Set `replicas: 2`. Healthcheck-gated rolling swap.
- **Per-service network tiers.** Each service joins a named network; only services on the same network can dial each other. Basic blast-radius isolation without a CNI plugin.
- **Dependency-ordered deploys.** Services declare `depends_on:` and `yoink up` runs them in topological-sort waves (independent services in parallel).
- **Drift detection.** Every effective spec (image, env, networks, mounts, options, file content) hashes deterministically and lands as a label. The TUI shows drift across the cluster without guessing.
- **k9s-style TUI.** A `ratatui` dashboard with one-key reconcile, prune, kill, shell-into, debug-sidecar, log filter. Keyboard-only.

## What it deliberately doesn't do

- No control plane, no agent on the hosts. yoink is a single Rust binary on your laptop or in CI.
- No service discovery, no scheduling — Docker's network alias DNS is plenty for a few services on a host.
- No multi-cluster, no HA, no auto-scaling. One operator, one config, one deploy at a time.
- No reverse proxy, secret store, load balancer, or stateful-accessory orchestration. Use the right tool for each.
