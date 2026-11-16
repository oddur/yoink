---
title: Run staging alongside prod
description: Run a staging environment on the same hosts as production with separate configs that have namespaced services and networks.
weight: 10
---

yoink has no built-in `environment` concept. To run staging on the same hosts as prod, use two config files that do not collide. Two things must differ between the environments:

1. **Service names**: Docker container names are unique per host. `api` and `api-staging` can both run. Two `api` services cannot.
2. **Network names**: `api` (prod) and `api-staging` (staging) keep the traffic apart on the same Docker daemon.

The volumes, hostnames, secrets, and ports follow from those two namespaces.

## Create the two configs

### Create one entry config for each environment [step]

Use this layout:

```
yoink.prod.yaml             # prod entry; includes services/prod/*.yaml
yoink.staging.yaml          # staging entry; includes services/staging/*.yaml
services/prod/api.yaml
services/prod/web.yaml
services/staging/api.yaml   # name: api-staging, networks: [api-staging, redis-staging, ...]
services/staging/web.yaml   # name: web-staging
```

### Deploy each config independently [step]

```bash
yoink up --config yoink.prod.yaml          # prod deploy
yoink up --config yoink.staging.yaml       # staging deploy
yoink dump --config yoink.staging.yaml     # staging snapshot
yoink tui --config yoink.staging.yaml      # TUI scoped to staging containers
```

`yoink up` reconciles only the services that the loaded config declares. It does not touch the containers of the other environment. `yoink up --dry-run` lists those containers as orphans.

Do not run `yoink prune` while two configs share a host. `yoink prune` removes every yoink-managed container whose service is not in the loaded config. With the staging config, that includes the prod containers.

## Public surface (bundled Caddy)

The reverse proxy is the one shared part. The bundled Caddy serves the two environments from the same `:443` listener and routes by hostname. Set a different `domain:` on each service:

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

## Stateful accessories (redis, postgres)

Select one of two methods for each service:

| Method | Config | Trade-off |
|---|---|---|
| One instance for each environment | `redis` and `redis-staging` services, each on its own network. | Full data isolation. |
| One shared instance | One `redis`. The app in staging uses the key prefix `staging:`. | Half the memory, and no data isolation. |

For a managed Postgres, use separate databases (`myapp` and `myapp_staging`) in the same cluster. yoink does not deploy that database. Put the connection strings in the env vars or secrets of each environment.

## Tag overrides between envs

The two environments use the same image with different tags. Use one of these two methods:

- Leave `tag:` unset in the service YAML. When CI deploys staging, pass `--tag api-staging=$MAIN_SHA`.
- Commit a digest in `tag:` in the staging config (build once, promote many).

## Per-environment sealed secrets

Give each config its own sealed file with `secrets.file:`:

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

yoink resolves `secrets.file:` relative to the directory of the loaded config. The CLI, CI, and the TUI all use this path.

The secrets pane of the TUI (`e` key) shows the active filename in the title bar. Before you edit, make sure that the filename is the one for the correct environment:

```
yoink secrets · age — secrets.prod.age (2 recipients) · 7 keys · writable
```

To change the environment, change the config:

```sh
yoink -c yoink.prod.yaml tui          # editing prod's secrets
yoink -c yoink.staging.yaml tui       # editing staging's secrets
```

Rotation, recipients, and the `YOINK_AGE_KEY` env var are independent for each environment. With different `recipients:`, staging CI and prod CI have different keys.

## See also

- [Sealed secrets (age)](/docs/explanation/secrets): recipients and rotation per environment.
- [Multi-host distribution](/docs/explanation/networking#multi-host-distribution): host pinning for when the staging and prod split is not sufficient.
- [Pre-merge dry-run on every PR](/docs/how-to/pr-comment-dry-run): find staging and prod diffs at review time.
- [Config reference](/docs/reference/config#includes): `include:` globs and the other config keys.
