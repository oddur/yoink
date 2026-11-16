---
title: Distribute services across hosts
description: Set replicas and services[].hosts to scale a stateless app, run a singleton, pin a stateful service, or run one service for each region.
---

## Prerequisites

- A `yoink.yaml` with more than one entry in `hosts:`.
- You know the two keys: `replicas` (containers on each host) and `services[].hosts` (the hosts that run a service). See [Networking](/docs/explanation/networking#multi-host-distribution).

## Scale a stateless app horizontally

Leave `services[].hosts` unset. Set `replicas` to the number of containers for each host.

```yaml
hosts:
  - { address: prod-eu-1, user: deploy }
  - { address: prod-eu-2, user: deploy }
  - { address: prod-eu-3, user: deploy }
services:
  - name: api
    run: { replicas: 2 }     # 6 total — 2 per host
```

## Run a singleton (cron, queue worker)

Pin the service to one host. Set `replicas` to `1`.

```yaml
services:
  - name: scheduler
    hosts: [prod-eu-1]       # one host
    run: { replicas: 1 }     # one container — singleton
```

## Pin a stateful service and replicate a stateless service

Pin the stateful service to one host. Pin the service that dials it to the same host, and put the two on the same network.

```yaml
services:
  - name: redis
    hosts: [prod-db-1]
    run: { replicas: 1 }

  - name: api
    hosts: [prod-db-1]
    networks: [redis]
    run: { replicas: 2 }
```

This shape works on one host only. A Docker bridge network exists on one host, so an `api` on `prod-eu-1` cannot dial `redis:6379` on `prod-db-1`. For the patterns that work across hosts, see [Cross-host traffic between services](/docs/explanation/networking#cross-host-traffic-between-services).

## Reach a stateful service from a different host

Add a `publish:` entry to the stateful service that binds a tailnet IP. Then configure the consumer to dial that tailnet hostname.

## Run one service for each region

Declare one service for each region with the same image. Pin each service to the hosts of its region. Set the region in `env`.

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

## Make sure that the distribution is correct

1. Run `yoink up --dry-run`. Make sure that each service shows on the hosts that you expect.
2. If you moved a service to a different host, make sure that the output lists the containers on the old host as orphans. `yoink up` does not stop them, and `yoink prune` removes them only after they stop.

## See also

- [Networking](/docs/explanation/networking#multi-host-distribution): how yoink schedules across hosts, and why cross-host traffic has limits.
- [Configuration reference](/docs/reference/config): the `hosts:` and `replicas:` fields.
- [Multi-host Let's Encrypt with Redis](/docs/how-to/multi-host-redis-storage): certificates when more than one host serves the same domain.
- [Production example](/docs/reference/examples/production): a full multi-host config.
