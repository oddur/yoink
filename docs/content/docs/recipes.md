---
title: Recipes
weight: 8
---

Common patterns. Living document — add as new use cases come up.

## Running the same stack twice on the same hosts (staging alongside prod)

Common scenario: you want a `staging` environment that mirrors `prod` for pre-merge verification, ideally on the same hardware to avoid paying for a second machine. yoink doesn't have a built-in `environment` concept — instead you use **two config files** that don't collide.

The two things that have to differ between envs:

1. **Service names** — Docker container names are unique per host. `api` and `api-staging` can both run; `api` and `api` cannot.
2. **Network names** — `api` (prod tier) and `api-staging` (staging tier) keep traffic separated even though both stacks live on the same docker daemon.

Everything else (volumes, hostnames, secrets, ports) follows from those two namespaces.

### Layout

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

## Authenticating with Infisical

When `secrets.provider: infisical` is set, yoink resolves a bearer token in this order:

1. **Universal Auth** (machine identity) — `INFISICAL_CLIENT_ID` + `INFISICAL_CLIENT_SECRET`. The CI path; create a machine identity in Infisical's UI and expose the pair as repo secrets.
2. **Raw bearer token** — `INFISICAL_TOKEN`. For one-off runs where you already have a token in hand.
3. **Cached browser-flow login** — yoink reads the session that the `infisical` CLI persists after `infisical login`. Run it once on your laptop:
   ```sh
   brew install infisical/get-cli/infisical
   infisical login                        # add --domain=… for self-hosted
   ```
   yoink then reuses that session — the CLI binary itself is not invoked at deploy time, only its keychain entry is read.

## Self-hosted registry as a yoink service

See [Three deploy modes → Self-hosted registry](/docs/deploy-modes#self-hosted-registry-as-a-yoink-service).
