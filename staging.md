# Staging environment runbook

This file documents how staging *will* work and what's needed to bring it up.
Nothing under this plan is wired today — there's no `yoink.staging.yaml`, no
`services/staging/`, no Cloudflare `staging.*` records. The prod side has been
restructured (`yoink.prod.yaml`, `services/prod/`) so the operator-side work
when staging actually lands is purely additive.

## Architecture

- **Same host as prod.** Both envs run on `backtrack-eu-1`. Cheaper than a
  second Hetzner box; trade-off is that a host outage takes both envs down.
- **Network isolation.** Prod uses the existing `kamal` docker network.
  Staging gets a new `yoink-staging` network. Containers on different docker
  networks can't reach each other — staging can't accidentally hit prod
  redis or vice versa.
- **Two caddies, one host.** Prod caddy publishes `:80` and `:443`. Staging
  caddy publishes `:8080` and `:8443`. Cloudflare's **Origin Rules** route
  `staging.backtrack.studio` and `staging-api.backtrack.studio` to the
  origin's `:8443` port; the staging caddy then host-header-routes inside.
  This is what lets us have two completely independent caddies on one host
  — a staging caddy misconfig can't take prod down.
- **Same TLS / Authenticated Origin Pulls.** Cloudflare's wildcard origin
  cert covers `*.backtrack.studio`, so the staging caddy mounts the same
  `/root/certs` bind that prod's caddy uses. AOP is zone-level and applies
  to any port the origin listens on.

### Port allocation on `backtrack-eu-1`

| service | prod | staging |
|---|---|---|
| caddy http | `:80` | `:8080` |
| caddy https | `:443` | `:8443` |
| pgadmin (loopback) | `127.0.0.1:5050` | `127.0.0.1:5051` |
| api / web / redis / otel | unpublished (network-only) | unpublished (network-only) |

## Prerequisites (operator-side, before any `yoink up --config yoink.staging.yaml`)

### 1. Infisical: add a `staging` environment

The yoink config block stays the same project (`0b9dd64d-b7ee-4d44-8477-0da48672a4a1`,
domain `https://infisical.story-halibut.ts.net`); only the `environment`
selector flips to `staging`. Create a new Infisical environment named
`staging` under that project and populate the keys each staging service
declares in its `secrets:` and `env_from_secrets:` blocks. As a starting
point this mirrors prod:

- `DATABASE_URL`, `DATABASE_MIGRATE_URL`, `APP_RUNTIME_ROLE` (api)
- `AUTH_DATABASE_URL`, `AUTH_DATABASE_MIGRATE_URL`, `AUTH_RUNTIME_ROLE`,
  `BETTER_AUTH_SECRET`, `BETTER_AUTH_API_KEY`, `GOOGLE_CLIENT_SECRET`,
  `TURNSTILE_SECRET_KEY` (web + the web-migrate hook)
- `STRIPE_SECRET_KEY`, `STRIPE_WEBHOOK_SECRET`, `BREVO_API_KEY`,
  `BREVO_WEBHOOK_SECRET`, `SMTP_PASS` (web)
- `B2_*`, `DOWNLOAD_*` (api object storage)
- `GRAFANA_CLOUD_OTEL_TOKEN`, `GRAFANA_CLOUD_OTEL_AUTH_HEADER` (otel)
- `PGADMIN_DEFAULT_EMAIL`, `PGADMIN_DEFAULT_PASSWORD` (only if running staging pgadmin)

Registry credentials (`KAMAL_REGISTRY_USERNAME`, `KAMAL_REGISTRY_PASSWORD`)
can stay shared from prod — depot is org-wide.

### 2. PlanetScale: provision staging databases

Two databases live in prod: `postgres` (api owns `public.*`) and
`backtrack_auth` (web/Better Auth owns `auth.*`). Memory note
`project_planetscale_branching_d3_limitation.md` flags that `pscale branch`
only branches the default DB (`postgres`), not `backtrack_auth`. Two paths:

- **Separate cluster.** Cleanest isolation; costs another PlanetScale instance.
  `terraform/planetscale/` would grow a `staging` module mirroring `prod`.
- **Parallel databases in the existing cluster.** Add `staging_postgres` and
  `staging_backtrack_auth` to the prod cluster. Cheaper, single point of
  failure (cluster outage = prod + staging both down). Same role-pair pattern
  (`<service>_migrate` for DDL, `<service>` for DML), same `task
  planetscale:db:bootstrap` / `planetscale:schema:setup` flow.

Operator picks at bring-up; document the choice when it lands.

### 3. Cloudflare: add staging records + Origin Rule

In `terraform/cloudflare/` add:

- Two **proxied A records**:
  - `staging.backtrack.studio` → `176.9.142.169` (the backtrack-eu-1 public IP)
  - `staging-api.backtrack.studio` → `176.9.142.169`
- One **Origin Rule** (matches `http.host in {"staging.backtrack.studio", "staging-api.backtrack.studio"}`),
  setting destination port to `8443`. Available on Free/Pro/Business/Enterprise.
  Compatible with the project's zone-level Authenticated Origin Pulls.

`terraform plan` should show only those additions; existing prod records
stay untouched.

## Files to create at bring-up time

The structural moves on the prod side are already done (`yoink.prod.yaml`,
`services/prod/`). What's needed for staging:

```
yoink.staging.yaml                                   # mirrors yoink.prod.yaml
services/staging/api.yaml                            # full mirror of prod, replicas: 1
services/staging/web.yaml                            # full mirror of prod, replicas: 1
services/staging/caddy.yaml                          # publish: ["8080:80", "8443:443"]
services/staging/redis.yaml                          # full mirror; alias still backtrack-redis (network-isolated)
services/staging/otel.yaml                           # full mirror; alias still backtrack-otel
services/staging/pgadmin.yaml                        # publish: ["127.0.0.1:5051:80"] (or skip)
backtrack-deploy/config/Caddyfile.staging            # listens on :8443/:8080, routes staging.* hostnames
.github/workflows/yoink-deploy-api-staging.yml       # copy of prod workflow, --config yoink.staging.yaml
.github/workflows/yoink-deploy-web-staging.yml       # ditto
```

`yoink.staging.yaml` knobs that differ from `yoink.prod.yaml`:

| field | prod | staging |
|---|---|---|
| `deploy.network` | `kamal` | `yoink-staging` |
| `hosts` | `backtrack-eu-1` | same |
| `secrets.environment` | `prod` | `staging` |
| `secrets.project_id` / `domain` | unchanged | unchanged |
| `registry` | unchanged | unchanged |
| `include` | `services/prod/*.yaml` | `services/staging/*.yaml` |

Service fragment names stay the same (`api`, `web`, `caddy`, etc.) —
they live in different yoink configs so there's no collision. Container
names on the host get `deploy.network` baked into the spec hash, so
prod's `api-<hash>` and staging's `api-<hash>` will have *different* hashes
and coexist in `docker ps`.

## Bring-up sequence

```
# 1. Cloudflare changes (DNS + Origin Rule)
cd terraform/cloudflare && terraform apply

# 2. Populate the Infisical staging env (UI or CLI)
# 3. Provision the PlanetScale staging DB(s) per the chosen path

# 4. Author the staging YAML files (yoink.staging.yaml, services/staging/*.yaml,
#    backtrack-deploy/config/Caddyfile.staging) and the new GH workflows

# 5. First deploy — caddy first so the staging hostnames respond
cd /repo
task yoink:up -- --config yoink.staging.yaml --service caddy

# 6. Then api + web (they run their own per-service migrate hooks)
task yoink:up -- --config yoink.staging.yaml --service api --tag api=<sha>
task yoink:up -- --config yoink.staging.yaml --service web  --tag web=<sha>

# 7. Smoke-test
curl -fsS https://staging.backtrack.studio/         # → 200
curl -fsS https://staging-api.backtrack.studio/health  # → 200
```

## Rollback

Same shape as prod's `yoink rollback`:

```
task yoink:run -- rollback api --config yoink.staging.yaml
```

The host advisory lock keys by `Host.address` so a prod deploy and a
staging deploy can't collide on the lock — but they also won't *block*
each other, which is the right answer (different containers, different
networks, no shared state).

## Teardown

```
task yoink:run -- prune --config yoink.staging.yaml         # remove yoink-managed staging containers
ssh root@backtrack-eu-1 'docker network rm yoink-staging'    # remove the network
# revert the Cloudflare changes via terraform if you want the records gone too
```

Prod is untouched.

## Open questions to settle when staging is genuinely brought up

1. **PlanetScale staging shape.** Separate cluster or parallel DBs in the
   prod cluster.
2. **CI trigger for staging deploys.** A long-lived `staging` branch, a
   `release-staging` branch, or just `workflow_dispatch` for manual rolls.
3. **Staging pgadmin.** Whether it's worth running a second pgadmin or
   the operator can just `psql` against the staging DB directly when
   debugging.
