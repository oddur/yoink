---
title: yoink
layout: docs
toc: false
---

A small, opinionated container deploy CLI + TUI for people who run a handful of services on a handful of bare-metal hosts. **Batteries and best practices included** — `age`-sealed secrets, hardened container defaults, healthcheck-gated rolling swaps, drift detection, dependency-ordered waves, all on by default with no plugins to install. Sits between [Kamal](https://kamal-deploy.org) and Kubernetes — opinionated about the same things Kamal is, borrowing the few Kubernetes ideas that actually pay off at this scale.

```
yoink up                  # reconcile every service in dep order
yoink tui                 # k9s-style dashboard, drift, logs, shell-into
yoink prune               # clean up stale containers
yoink history api         # who deployed what, when
yoink rollback api        # roll back to the previous version
```

![yoink TUI](https://github.com/user-attachments/assets/bcf956b1-937a-43d7-92f6-0f24a1d0cd98)

{{< cards >}}
  {{< card link="/docs/start/first-deploy" title="Five-minute first deploy" subtitle="Drop a yoink.yaml next to your Dockerfile, run one command." icon="lightning-bolt" >}}
  {{< card link="/docs/intro/compared" title="Is this for me?" subtitle="vs Kamal, vs Kubernetes, vs plain compose. When yoink is the right answer, when it isn't." icon="adjustments" >}}
  {{< card link="/docs/guide/deploy-modes" title="Three deploy modes" subtitle="CI-built, kamal-style local-build, no-registry standalone." icon="server" >}}
  {{< card link="/docs/guide/security-defaults" title="Secure by default" subtitle="cap_drop=ALL, read-only rootfs, no-new-privileges, init=tini, …" icon="shield-check" >}}
  {{< card link="/docs/recipes" title="Recipes" subtitle="Staging alongside prod, self-hosted registry, Infisical secrets, PR-comment dry-run." icon="clipboard-list" >}}
  {{< card link="/docs/reference" title="Reference" subtitle="Every CLI flag, every config field, every TUI keybind." icon="document-text" >}}
{{< /cards >}}

## Why it exists

Kamal is a lovely fit for "one app, one binary per host" but starts to creak the moment you want a second app on the same box, replicas of the same service, or any kind of network isolation between containers. Kubernetes solves all that — and a hundred other problems you don't have, in exchange for a control plane to operate, a YAML schema with a learning curve, and a vocabulary you have to teach every new operator.

`yoink` is the thinnest tool that gives you the Kubernetes ideas that actually matter at small scale, while keeping Kamal's "one binary, ssh into the host, drive Docker directly" simplicity:

- **Multiple services per host** with replicas + dependency-ordered deploys (`depends_on:` topo-sort)
- **Per-service network tiers** for blast-radius isolation without a CNI plugin
- **Drift detection.** Every effective spec hashes deterministically and lands as a label. The TUI shows drift across the cluster without guessing.
- **Three deploy modes**: CI-built (the default), local-build kamal-style with `yoink build --push`, or fully standalone with `yoink up --build --no-registry` (no CI, no registry — drop a `yoink.yaml` next to your Dockerfile and go)
- **Secure by default**: cap_drop=ALL, no-new-privileges, read-only rootfs, init=tini, tmpfs noexec, binds default :ro. Override per service when needed.
- **Sealed secrets out of the box**: commit a single `secrets.age` file, decrypt with one key from `YOINK_AGE_KEY` (env in CI, file on your laptop). No remote vault needed. Infisical is opt-in for teams already running one.
- **k9s-style TUI** with deploy history, one-press rollback, drift cells, logs auto-piped through `hl`

Read more in the [intro](/docs/intro), or skip ahead to [first deploy](/docs/start/first-deploy).
