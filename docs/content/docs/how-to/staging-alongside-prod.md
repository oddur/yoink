---
title: Run staging alongside prod
weight: 10
---

Common scenario: you want a `staging` environment that mirrors `prod` for pre-merge verification, ideally on the same hardware to avoid paying for a second machine. yoink doesn't have a built-in `environment` concept — instead you use **two config files** that don't collide.

The two things that have to differ between envs:

1. **Service names** — Docker container names are unique per host. `api` and `api-staging` can both run; `api` and `api` cannot.
2. **Network names** — `api` (prod tier) and `api-staging` (staging tier) keep traffic separated even though both stacks live on the same docker daemon.

Everything else (volumes, hostnames, secrets, ports) follows from those two namespaces.

## Layout

```
yoink.prod.yaml             # prod entry; includes services/prod/*.yaml
yoink.staging.yaml          # staging entry; includes services/staging/*.yaml
services/prod/api.yaml
services/prod/web.yaml
services/staging/api.yaml   # name: api-staging, networks: [api-staging, redis-staging, ...]
services/staging/web.yaml   # name: web-staging
```

Each config is operated independently:

```bash
yoink up --config yoink.prod.yaml          # prod deploy
yoink up --config yoink.staging.yaml       # staging deploy
yoink dump --config yoink.staging.yaml     # staging snapshot
yoink tui --config yoink.staging.yaml      # TUI scoped to staging containers
```

The TUI's drift detection, `prune`, and reconcile are all scoped to whatever `--config` references — `yoink prune --config staging.yaml` won't touch prod containers because their `yoink.service` labels don't match anything declared in the staging config.

## Public surface (bundled Caddy)

The reverse proxy is the one shared piece of infra: yoink's bundled Caddy fronts both envs from the same `:443` listener. Routing is by hostname — set `domain:` on each service:

```yaml
# services/prod/api.yaml
- name: api
  image: ghcr.io/you/api
  domain: api.example.com              # → prod api
  run:
    port: 8080
    network_aliases: [api]

# services/staging/api.yaml
- name: api-staging
  image: ghcr.io/you/api
  domain: staging-api.example.com      # → staging api
  run:
    port: 8080
    network_aliases: [api-staging]     # different alias from prod's `api`
```

Each `yoink up` re-pushes the routing config via Caddy's admin API; both hostnames coexist on the same listener with no collision.

## Stateful accessories (redis, postgres, etc.)

Two options, pick per service:

- **Separate instances per env**: `redis` and `redis-staging` containers, both yoink-managed, on their own networks. Cheap; full isolation.
- **Shared instance, namespaced data**: one `redis`, but staging uses key prefix `staging:` (app-side responsibility). Half the memory; no data isolation.

For SQL — typically a managed Postgres with separate databases (`myapp` and `myapp_staging`) under the same cluster. Yoink doesn't deploy the database; the connection strings live in env vars / secrets per env.

## Tag overrides between envs

Staging usually wants to deploy whatever's on `main`, while prod tracks `live`. Either:

- Keep `tag:` empty in the service yaml; supply via `--tag api-staging=$MAIN_SHA` from CI when deploying staging
- Or commit a digest pointer in `yoink.staging.yaml` (build-once-promote-many) — same image, different deploy targets

The image identity is the same across envs; only the runtime configuration differs.

## Per-environment sealed secrets

Each config carries its **own** sealed file. Set `secrets.file:` so the two environments don't collide:

```yaml
# yoink.prod.yaml
secrets:
  provider: age
  file: secrets.prod.age
  recipients: [age1...laptop, age1...prod-ci]

# yoink.staging.yaml
secrets:
  provider: age
  file: secrets.staging.age
  recipients: [age1...laptop, age1...staging-ci]
```

The path is resolved the same way locally, in CI, and from the TUI — whatever the loaded config's `secrets.file:` says (relative to the config's directory). The TUI's secrets pane (`e` key) surfaces the active filename in the title bar so you can tell at a glance which environment you're editing:

```
yoink secrets · age — secrets.prod.age (2 recipients) · 7 keys · writable
```

Switch environments by switching configs:

```sh
yoink -c yoink.prod.yaml tui          # editing prod's secrets
yoink -c yoink.staging.yaml tui       # editing staging's secrets
```

Rotation, recipients, and the `YOINK_AGE_KEY` env var are independent per environment — staging can have a different CI key than prod, scoped via `recipients:`.

## See also

- [Sealed secrets (age)](/docs/guide/secrets) — per-environment recipients and rotation.
- [Multi-host distribution](/docs/guide/networking#multi-host-distribution) — when staging-vs-prod isn't enough and you need fine-grained host pinning.
- [Pre-merge dry-run on every PR](/docs/how-to/pr-comment-dry-run) — catch staging/prod diffs at review time.
