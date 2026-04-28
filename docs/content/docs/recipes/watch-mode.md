---
title: Edit-save-deploy with `--watch`
weight: 8
---

`yoink up --watch` re-reconciles whenever the config file (or any include fragment) changes on disk. Combined with `--build --no-registry`, it gives you a hot-reload loop for prod-like development without CI, a registry, or a deploy command in your shell history.

## The loop

```sh
yoink up --watch --build --no-registry --service my-tool
```

Then edit `yoink.yaml` (or the Dockerfile for `my-tool`). On save:

1. Yoink polls every 2s and notices the config differs from the last reconcile.
2. `--build` rebuilds `my-tool`'s image against your local docker daemon.
3. `--no-registry` ships the new image directly to the host (unregistry transport by default — only the changed layers cross the wire).
4. The reconcile loop swaps the running container with the standard healthcheck-gated rolling deploy.

Ctrl-C exits the loop.

## Why polling?

Yoink's TUI uses the same 2 s tick to reload the config. Polling avoids a `notify`/`fsnotify`-style file-watcher dependency and is plenty fast for an operator typing in their editor — the reconcile latency itself dominates anything below ~500 ms.

If the reload fails (yaml syntax error, missing required field), the watch loop prints the error and keeps polling. Fix the file and the next save tries again. There's no need to restart `yoink up`.

## Common patterns

**Iterating on a single service** — pin it with `--service` so unrelated services aren't redeployed on every save:

```sh
yoink up --watch --build --no-registry --service api
```

**Iterating on an include fragment** — the `include:` glob is re-resolved on every reload, so a fresh `services/new-thing.yaml` is picked up automatically:

```yaml
# yoink.yaml
include:
  - services/*.yaml
```

```sh
yoink up --watch --build --no-registry        # all services, re-resolve includes on save
```

**Plan first, watch second** — `yoink up --plan` is a one-shot read-only view; `--watch` cannot be combined with `--plan` (the watch loop is for the apply path):

```sh
yoink up --plan                                # what would change?
yoink up --watch --build --no-registry         # ok, ship it on every save
```

## Tradeoffs vs. an inotify-based watcher

Polling means up to 2 s of latency between save and reconcile. For an editor flow that's typically below the threshold of "did I actually save?" — but if you're chaining `yoink up --watch` into a tighter feedback loop (test runner, screen recorder), be aware of the floor.

The reconcile itself is cheap when nothing changed: yoink computes a `yoink.spec_hash` from the desired spec and short-circuits when the running container's label already matches. So the cost of a fired but no-op reconcile is one round-trip per host.

## See also

- [TanStack Start + postgres](/docs/recipes/tanstack-stack) — uses `--build --no-registry` as the deploy loop; pair with `--watch` for save-to-deploy.
- [Three deploy modes](/docs/guide/deploy-modes) — when local-build vs. CI-build vs. standalone fits.
- [CLI reference: up](/docs/reference/cli) — `--watch` and the rest of the `yoink up` flag surface.
