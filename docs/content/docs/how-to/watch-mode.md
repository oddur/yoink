---
title: Edit-save-deploy with `--watch`
description: Poll the config for changes and deploy again automatically on every save, for hot reload in prod-like local development.
weight: 8
---

`yoink up --watch` runs one reconcile. Then it reconciles again each time the config file, or an include fragment, changes on disk. With `--build`, it gives an edit, save, deploy loop with no CI and no registry.

## Start the watch loop

### Start the loop [step]

```sh
yoink up --watch --build --service my-tool
```

### Edit and save [step]

Edit `yoink.yaml` or an include fragment. Save the file.

yoink loads the config again every 2 s. If the loaded config changed, yoink builds the image, ships it, and rolls the container behind its healthcheck. A change to a different file, for example the Dockerfile, does not start a reconcile.

### Stop the loop [step]

Press Ctrl-C.

## If a reload fails

A reload can fail, for example on a YAML syntax error or a missing required field. Then the watch loop prints the error and continues to poll. A failed reconcile has the same effect.

Correct the file and save it. A restart of `yoink up` is not necessary.

## Common patterns

### Iterate on a single service

Pass `--service`. Then a save does not redeploy unrelated services.

```sh
yoink up --watch --build --service api
```

### Iterate on an include fragment

yoink resolves the `include:` glob again on every reload, so it finds a new `services/new-thing.yaml` automatically.

```yaml
# yoink.yaml
include:
  - services/*.yaml
```

```sh
yoink up --watch --build        # all services, re-resolve includes on save
```

### Plan first, watch second

`yoink up --plan` is a one-shot, read-only view. yoink rejects `--watch` together with `--plan` or `--dry-run`. Run the plan first. Then start the watch loop.

```sh
yoink up --plan                                # what would change?
yoink up --watch --build                       # ok, ship it on every save
```

## See also

- [Why `--watch` polls](/docs/explanation/watch-polling): the reason for the 2 s tick and the trade-offs against a file watcher.
- [TanStack Start + postgres](/docs/tutorials/tanstack-stack): uses `--build` as the deploy loop. Add `--watch` for save-to-deploy.
- [Deploy modes](/docs/explanation/deploy-modes): when local-build, CI-build, or standalone fits, and how yoink ships the image.
- [CLI reference: up](/docs/reference/cli): `--watch` and the other `yoink up` flags.
