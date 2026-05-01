---
title: Edit-save-deploy with `--watch`
description: Poll the config for changes and redeploy automatically on every save — hot-reload for prod-like local dev.
weight: 8
---

`yoink up --watch` re-reconciles whenever the config file (or any include fragment) changes on disk. Combined with `--build`, it gives you a hot-reload loop for prod-like development without CI, a registry, or a deploy command in your shell history.

## The loop

```sh
yoink up --watch --build --service my-tool
```

Edit `yoink.yaml` (or the Dockerfile for `my-tool`). On save, yoink rebuilds, ships, and rolls the container behind its healthcheck. Ctrl-C exits.

Polling cadence and reload-failure semantics live in the [Why polling?](#why-polling) section below; the underlying ship + swap mechanics are in [deploy modes](/docs/guide/deploy-modes) and [architecture](/docs/guide/architecture).

## Why polling?

Yoink's TUI uses the same 2 s tick to reload the config. Polling avoids a `notify`/`fsnotify`-style file-watcher dependency and is plenty fast for an operator typing in their editor; the reconcile latency itself dominates anything below ~500 ms.

If the reload fails (yaml syntax error, missing required field), the watch loop prints the error and keeps polling. Fix the file and the next save tries again. There's no need to restart `yoink up`.

## Common patterns

**Iterating on a single service** — pin it with `--service` so unrelated services aren't redeployed on every save:

```sh
yoink up --watch --build --service api
```

**Iterating on an include fragment** — the `include:` glob is re-resolved on every reload, so a fresh `services/new-thing.yaml` is picked up automatically:

```yaml
# yoink.yaml
include:
  - services/*.yaml
```

```sh
yoink up --watch --build        # all services, re-resolve includes on save
```

**Plan first, watch second** — `yoink up --plan` is a one-shot read-only view; `--watch` cannot be combined with `--plan` (the watch loop is for the apply path):

```sh
yoink up --plan                                # what would change?
yoink up --watch --build                       # ok, ship it on every save
```

## Tradeoffs vs. an inotify-based watcher

Polling means up to 2 s of latency between save and reconcile. For an editor flow that's typically below the threshold of "did I actually save?", but if you're chaining `yoink up --watch` into a tighter feedback loop (test runner, screen recorder), be aware of the floor.

The reconcile itself is cheap when nothing changed: yoink computes a [`yoink.spec_hash`](/docs/guide/architecture#drift-detection) from the desired spec and short-circuits when the running container's label already matches. So the cost of a fired but no-op reconcile is one round-trip per host.

## See also

- [TanStack Start + postgres](/docs/how-to/tanstack-stack) — uses `--build` as the deploy loop; pair with `--watch` for save-to-deploy.
- [Three deploy modes](/docs/guide/deploy-modes) — when local-build vs. CI-build vs. standalone fits.
- [CLI reference: up](/docs/reference/cli) — `--watch` and the rest of the `yoink up` flag surface.
