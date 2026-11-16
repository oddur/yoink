---
title: Polyglot stack
description: A Rust API, a Node web frontend, Caddy, and Redis on a single host with per-tier networks.
weight: 2
---

This config is for an app with a backend, a frontend, a cache, and a TLS proxy in front of them. It replaces `docker-compose up` on one host.

The stack is a Rust API, a Node web frontend, Redis, and the bundled Caddy proxy on a single host. Each tier has its own docker network. CI builds the images and pushes them to a registry.

`yoink up` deploys the services with healthcheck-gated swaps, in waves that obey the dependency order. For the same patterns on multiple hosts, see [Production-shape](/docs/reference/examples/production).

```yaml
# yoink.yaml
deploy:
  networks: [api, web, redis]            # named tiers — services join the ones they need

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
    networks: [api, redis]                 # can dial redis; web dials api
    secrets: [DATABASE_URL]                # injected as env var of same name
    pre_deploy:
      - name: api-migrate
        image: ghcr.io/you/api
        tag: { service: api }              # mirror the runtime tag
        cmd: ["migrate"]
        secrets: [DATABASE_MIGRATE_URL]    # different role, same secret store
    run:
      port: 8080
      replicas: 2                          # new replicas pass the healthcheck before old ones stop
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
        network_aliases: [api]             # web uses `http://api:8080`
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

There is no `services/caddy.yaml`. Because `api` and `web` have `domain:`, yoink adds a `yoink-proxy` service that runs `caddy:2`. yoink puts the proxy, `api`, and `web` on the `yoink-ingress` network. On each deploy, yoink sends the routing config through the admin API of Caddy.

ACME runs in the proxy container. A named volume keeps the certs across deploys. [Reverse proxy](/docs/explanation/proxy) has the full schema (forward_auth, basic_auth, h2c for gRPC).

## Deploying

```sh
SHA=$(git rev-parse HEAD)
yoink up --tag api=$SHA --tag web=$SHA
```

`redis` uses the tag from its config. `api` and `web` have no `tag:`, so each one gets a `--tag` override: CI passes the SHA that it built.

## What this exercises

| Feature | In this config |
|---|---|
| **Per-tier networks** | `redis` is only on `redis`. The proxy shares `yoink-ingress` with `api` and `web`, and never joins `redis`. A compromised proxy cannot find redis through docker DNS. |
| **`depends_on` waves** | `redis` and `yoink-proxy` → `api` → `web`. yoink deploys the services of one wave concurrently. |
| **Pre-deploy hooks** | `api-migrate` runs one time for each deploy of `api` (not for each replica), immediately before the new containers start. It uses the same image and tag as the runtime container. |
| **Replicas** | `api` and `web` each have 2. The old replicas stop only after both new replicas pass the healthcheck. |
| **Surgical security opt-out** | `redis` changes only `user`. The proxy adds back only `NET_BIND_SERVICE` and runs as root. `read_only`, `cap_drop=ALL`, and `no-new-privileges` stay on. |

## See also

- [Production-shape example](/docs/reference/examples/production): the same patterns on multiple hosts, with secrets, pre-deploy hooks, and a config in fragments.
- [Reverse proxy](/docs/explanation/proxy): the schema of the bundled Caddy.
- [Networking](/docs/explanation/networking): per-tier networks, multi-host distribution, and port-forward.
- [Configuration](/docs/reference/config): each field in these fragments.
- [Add HTTPS to a service](/docs/how-to/add-https): the steps to add `domain:` to a service.
