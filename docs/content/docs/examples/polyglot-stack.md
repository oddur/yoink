---
title: Polyglot stack
weight: 2
---

A single host running a Rust API + Node web + Caddy reverse proxy + Redis cache. Per-tier networks isolate the blast radius. Image references are CI-built, deployed via `yoink up` after the registry already has the SHA-tagged images.

```yaml
# yoink.yaml
deploy:
  networks: [public, api, web, redis]    # named tiers — services join the ones they need

hosts:
  - { address: prod, user: deploy }

include:
  - services/*.yaml                        # split fragments per service

# (the rest of this example is split across services/*.yaml below)
```

## `services/redis.yaml`

```yaml
services:
  - name: redis
    image: redis
    tag: 7-alpine                          # stable tag — no --tag override at deploy
    networks: [redis]                      # only `api` (which joins `redis`) can dial this
    run:
      port: 6379
      cmd:
        - redis-server
        - --maxmemory
        - 256mb
        - --maxmemory-policy
        - allkeys-lru
        - --save
        - ""                               # no RDB
        - --appendonly
        - "no"                             # no AOF — pure cache
      options:
        memory: "300Mi"
        cpus: "0.5"
        user: redis                        # non-root from the get-go
        # The image needs CHOWN on /data; /data lives on tmpfs; rootfs
        # stays read-only (yoink default).
        tmpfs:
          /data: "size=32m,mode=1777"      # auto noexec/nosuid/nodev
        network_aliases: [redis]           # api uses `redis://redis:6379`
```

## `services/api.yaml`

```yaml
services:
  - name: api
    image: ghcr.io/you/api
    # tag: <required at deploy time via --tag api=<sha>>
    domain: api.example.com                # bundled Caddy fronts this with HTTPS
    depends_on: [redis]                    # redis comes up first in the wave plan
    networks: [api, redis]                 # can dial redis; web/public dial api
    secrets: [DATABASE_URL]                # injected as env var of same name
    pre_deploy:
      - name: api-migrate
        image: ghcr.io/you/api
        tag: { service: api }              # mirror the runtime tag
        cmd: ["migrate"]
        secrets: [DATABASE_MIGRATE_URL]    # different role, same secret store
    run:
      port: 8080
      replicas: 2                          # rolling swap keeps 1 alive at all times
      healthcheck_path: /health
      healthcheck_timeout: 60s
      drain_timeout: 30s
      cmd:
        - /usr/local/bin/api
      options:
        memory: "1Gi"
        cpus: "1.5"
        # cap_drop / security_opt / read_only / pids_limit / init all
        # default to the hardened profile.
        tmpfs:
          /tmp: "size=64m,mode=1777"
        network_aliases: [api]             # the proxy upstreams to `api:8080`
```

## `services/web.yaml`

```yaml
services:
  - name: web
    image: ghcr.io/you/web
    # tag: <--tag web=<sha>>
    domain: example.com                    # bundled Caddy fronts this with HTTPS
    depends_on: [api]
    networks: [web, api]                   # dial api by name
    run:
      port: 3000
      replicas: 2
      healthcheck_path: /
      cmd:
        - node
        - /app/dist/server/index.js
      options:
        memory: "512Mi"
        cpus: "1"
        tmpfs:
          /tmp: "size=64m,mode=1777"       # V8 writes JIT cache here
        network_aliases: [web]
```

## Reverse proxy

No `services/caddy.yaml` needed — the moment any service has `domain:` set, yoink synthesizes a `yoink-proxy` service running `caddy:2`, joins it to the right networks, and pushes the routing config via Caddy's admin API on every deploy. ACME for Let's Encrypt happens in-container; certs persist across redeploys in a managed volume. See [Reverse proxy](/docs/guide/proxy) for the full schema (forward_auth, basic_auth, h2c for gRPC, etc.).

## Deploying

```sh
SHA=$(git rev-parse HEAD)
yoink up --tag api=$SHA --tag web=$SHA
```

`redis` and `caddy` use literal tags from their config, so no `--tag` needed. `api` + `web` get `--tag` overrides at deploy time (typical CI pattern: pass the just-built SHA).

## What this exercises

- **Per-tier networks**: `redis` only on the `redis` network; `caddy` only sees `api` and `web` because it joins those nets, never `redis` directly. A compromised `caddy` can't enumerate redis via docker DNS.
- **`depends_on` waves**: `redis` first, then `api`, then `web`, then `caddy`. Independent services in the same wave run concurrently.
- **Pre-deploy hooks**: `api-migrate` runs once per `up` (not per replica) before the api swap starts. Ships the same image as the runtime container so migrations match the code shape.
- **Replicas**: `api` and `web` each have 2 — rolling swap keeps 1 alive while the new one healthchecks.
- **Surgical security opt-out**: `caddy` re-adds `NET_BIND_SERVICE` only. `read_only` and `cap_drop=ALL` and `no-new-privileges` all stay default-on for it. `binds` for the docker socket is `:ro`; the data dir is explicit `:rw`.
