---
title: Production-shape
weight: 3
---

A real prod-shape config: multiple hosts (in case you scale out later — one host today is fine), Infisical secrets, registry-pulled images that CI builds, fragmented across files for sanity, with a separate staging entry.

## Layout

```
yoink.prod.yaml             # prod entry; includes services/prod/*.yaml
yoink.staging.yaml          # staging entry; includes services/staging/*.yaml
services/prod/api.yaml
services/prod/web.yaml
services/prod/redis.yaml
services/prod/caddy.yaml
services/prod/otel.yaml
services/staging/api.yaml   # name: api-staging, networks: [api-staging, …]
services/staging/web.yaml   # name: web-staging
```

## `yoink.prod.yaml`

```yaml
deploy:
  networks: [public, api, web, redis, otel]

hosts:
  - { address: prod-eu-1, user: deploy }
  # add more as needed; replicas + services[].hosts handle distribution
  # — see /docs/recipes/multi-host-distribution

# Infisical-resolved secrets, fetched once per `up` and folded into
# spec_hash. See /docs/recipes/infisical-auth for the auth chain.
secrets:
  provider: infisical
  project_id: <your-project-uuid>
  environment: prod
  domain: https://infisical.example.com   # omit for app.infisical.com

# Registry credentials live in Infisical too. Yoink looks up these
# named secrets and passes them to docker as X-Registry-Auth on every
# pull. Skip this block when running with `--no-registry` or when
# pulling from public registries.
registry:
  server: ghcr.io
  username_secret: GHCR_USERNAME
  password_secret: GHCR_TOKEN

include:
  - services/prod/*.yaml
```

## `services/prod/api.yaml`

```yaml
services:
  - name: api
    image: ghcr.io/you/api
    # tag: provided at deploy via `--tag api=$(git rev-parse HEAD)`
    depends_on: [redis, otel]
    networks: [api, redis, otel]            # api dials redis + otel
    secrets: [DATABASE_URL, JWT_SIGNING_KEY]
    env_from_secrets:
      # Same secret store, different env-var name on the container
      OTEL_EXPORTER_OTLP_HEADERS: GRAFANA_CLOUD_OTEL_AUTH_HEADER
    env:
      OTEL_EXPORTER_OTLP_ENDPOINT: http://otel:4318
      RUST_LOG: info
    pre_deploy:
      - name: api-migrate
        image: ghcr.io/you/api
        tag: { service: api }               # mirror the runtime tag exactly
        cmd: ["migrate"]
        secrets: [DATABASE_MIGRATE_URL]     # different role, same store
    run:
      port: 8080
      replicas: 2
      healthcheck_path: /health
      healthcheck_timeout: 60s
      drain_timeout: 30s
      cmd: [/usr/local/bin/api]
      options:
        memory: "1Gi"
        cpus: "1.5"
        tmpfs:
          /tmp: "size=64m,mode=1777"
        network_aliases: [api]
```

## `services/prod/redis.yaml`

```yaml
services:
  - name: redis
    image: redis
    tag: 7-alpine
    networks: [redis]
    run:
      port: 6379
      cmd:
        - redis-server
        - --maxmemory
        - 512mb
        - --maxmemory-policy
        - allkeys-lru
        - --appendonly
        - "no"
      options:
        memory: "600Mi"
        cpus: "0.5"
        user: redis                          # avoid the root → drop dance
        tmpfs:
          /data: "size=64m,mode=1777"
        network_aliases: [redis]
```

## `services/prod/caddy.yaml`

```yaml
services:
  - name: caddy
    image: lucaslorentz/caddy-docker-proxy
    tag: 2.10-alpine
    depends_on: [api, web]
    networks: [public, api, web]
    binds:
      - "/var/run/docker.sock:/var/run/docker.sock:ro"
      - "/srv/caddy/data:/data:rw"           # cert storage; explicit :rw
    run:
      publish:
        - "80:80"
        - "443:443"
        - "443:443/udp"                       # HTTP/3
      options:
        memory: "128Mi"
        cpus: "0.5"
        cap_add: [NET_BIND_SERVICE]           # needed for :80/:443
```

## `services/prod/otel.yaml`

```yaml
services:
  - name: otel
    image: otel/opentelemetry-collector-contrib
    tag: "0.150.1"                            # pin exact — minor versions ship breaking renames
    networks: [otel]
    secrets: [GRAFANA_CLOUD_OTEL_AUTH_HEADER]
    env:
      HOST_NAME: prod-eu-1
    run:
      port: 13133                             # health_check extension
      healthcheck_path: /
      healthcheck_timeout: 30s
      files:
        # File content hashes into spec_hash, so a config edit reroles
        - "services/prod/config/otel-collector.yaml:/etc/otelcol-contrib/config.yaml:ro"
      binds:
        - "/proc:/hostfs/proc:ro"
        - "/sys:/hostfs/sys:ro"
        - "/:/hostfs:ro"
        - "/var/run/docker.sock:/var/run/docker.sock:ro"
      options:
        memory: "256Mi"
        cpus: "0.5"
        # Host metrics receiver needs root + reads under /proc/<pid>;
        # opt out of the hardened defaults for this one.
        user: "0:0"
        cap_drop: []
        security_opt: []
        read_only: false
        network_aliases: [otel]
```

## CI deploys

Two repo workflows do the work. Skeleton:

```yaml
# .github/workflows/deploy.yml — runs on push to live
- run: |
    yoink up --tag api=${{ github.sha }} --tag web=${{ github.sha }}
  env:
    INFISICAL_CLIENT_ID: ${{ secrets.INFISICAL_CLIENT_ID }}
    INFISICAL_CLIENT_SECRET: ${{ secrets.INFISICAL_CLIENT_SECRET }}
```

```yaml
# .github/workflows/pr-diff.yml — runs on PRs touching services/**
- run: |
    yoink up --dry-run --format=markdown \
      --tag api=${{ github.event.pull_request.head.sha }} \
      --tag web=${{ github.event.pull_request.head.sha }} > diff.md
- uses: marocchino/sticky-pull-request-comment@v2
  with:
    header: yoink-pr-diff
    path: diff.md
```

See [PR-comment dry-run](/docs/recipes/pr-comment-dry-run) for the complete workflow.

## What this exercises

- **Multi-tier networks** — `redis` only on `redis`; `caddy` only on `public/api/web`; otel isolated on `otel`. Per-service blast radius.
- **Pre-deploy hooks** — `api-migrate` runs once per up, before the api swap, with the same image as the runtime.
- **`env_from_secrets`** — secret stored under one name, exposed under a different env-var name on the container. Pattern for legacy env-var conventions.
- **`files:` mounts** — content-hashed bind that feeds into `spec_hash`. Edit the otel config, redeploy → the otel container reroles automatically because its hash changed.
- **Surgical security opt-outs** — `caddy` keeps everything default-on except `NET_BIND_SERVICE`; `otel` opts out of the hardened defaults entirely (with a comment explaining why); everything else inherits the defaults.
- **Config fragmentation** — one file per service, glob-included. Each fragment is independent; renaming a service is a one-file operation.
- **Staging alongside prod** — see [the recipe](/docs/recipes/staging-alongside-prod) for the same hosts running a `yoink.staging.yaml` with `name: api-staging` etc.
