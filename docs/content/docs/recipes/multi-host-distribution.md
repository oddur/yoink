---
title: Multi-host distribution
weight: 9
---

How services spread across hosts when you scale beyond one box. Two knobs: `replicas` (per host) and `services[].hosts` (which hosts run a service).

## Default: every service on every host

By default a service runs on **every** host in `hosts:`, with `replicas` copies per host. So:

```yaml
hosts:
  - { address: prod-eu-1, user: deploy }
  - { address: prod-eu-2, user: deploy }

services:
  - name: api
    image: ghcr.io/you/api
    run:
      port: 8080
      replicas: 2
```

…gives you **4 api containers total**: 2 on `prod-eu-1`, 2 on `prod-eu-2`. Rolling swap is per host — yoink keeps `replicas - 1` alive on each host during the swap.

## Pinning a service to specific hosts

`services[].hosts:` whitelists which hosts run that service. Strings match `hosts[].address`.

```yaml
hosts:
  - { address: prod-eu-1, user: deploy }
  - { address: prod-eu-2, user: deploy }
  - { address: prod-db-1, user: deploy }      # beefier box, dedicated to stateful

services:
  - name: api
    image: ghcr.io/you/api
    # no `hosts:` → runs on all three; usually not what you want for stateful peers
    run: { port: 8080, replicas: 2 }

  - name: redis
    image: redis
    tag: 7-alpine
    hosts: [prod-db-1]                         # pin to the db host only
    run: { port: 6379 }

  - name: caddy
    image: lucaslorentz/caddy-docker-proxy
    tag: 2.10-alpine
    hosts: [prod-eu-1, prod-eu-2]              # public-facing tier only
    run:
      publish: ["80:80", "443:443", "443:443/udp"]
```

## Common shapes

### Stateless app, scale horizontally

```yaml
hosts: [prod-eu-1, prod-eu-2, prod-eu-3]
services:
  - name: api
    run: { replicas: 2 }     # 6 total — 2 per host
```

### Singleton (cron, queue worker)

```yaml
services:
  - name: scheduler
    hosts: [prod-eu-1]       # one host
    run: { replicas: 1 }     # one container — singleton
```

### Stateful pinned + stateless replicated

```yaml
services:
  - name: redis
    hosts: [prod-db-1]
    run: { replicas: 1 }

  - name: api
    hosts: [prod-eu-1, prod-eu-2]
    networks: [api, redis]   # api dials redis cross-host via the redis network
    run: { replicas: 2 }
```

For cross-host network reach, the `redis` network must be an **overlay** network (or you use a tailnet sidecar). Yoink creates bridge networks by default; switch by declaring it ahead of time on the host or extending `deploy.networks` once overlay support lands.

### Region-pinned

```yaml
hosts:
  - { address: prod-eu-1, user: deploy }
  - { address: prod-us-1, user: deploy }

services:
  - name: api-eu
    image: ghcr.io/you/api
    hosts: [prod-eu-1]
    env: { REGION: eu }
    run: { port: 8080, replicas: 2 }

  - name: api-us
    image: ghcr.io/you/api
    hosts: [prod-us-1]
    env: { REGION: us }
    run: { port: 8080, replicas: 2 }
```

Two services, same image, different env — region-aware deploys without conditionals in the config.

## How `yoink up` schedules across hosts

For each service, yoink computes its host set (`services[].hosts` ∩ `hosts[]`, defaulting to all hosts when unset) and runs the reconcile in parallel across those hosts. Within a host, the rolling swap is sequential per replica (start new → healthcheck → swap → drain old).

Wave ordering (`depends_on`) is global — `redis` finishes its host fan-out before `api` starts, regardless of which hosts each lands on.

## Pre-deploy hooks

`pre_deploy` hooks run **once per `up`**, on the first host that has the service. Don't multiply by replica count or host count — migrations run once, full stop.

## Pruning

`yoink prune` walks every host independently, removing containers and images that don't match any current service definition. A service that used to run on `prod-eu-2` but is now pinned to `prod-eu-1` gets cleaned up on `prod-eu-2` automatically.

## See also

- [Run staging alongside prod](/docs/recipes/staging-alongside-prod) — same primitives, separate `yoink.yaml` per environment.
- [Multi-host Let's Encrypt with Redis](/docs/recipes/multi-host-redis-storage) — proxy-side coordination when more than one host fronts the same domain.
- [Configuration reference](/docs/reference/config) — full `hosts:` / `replicas:` / `pin:` schema.
