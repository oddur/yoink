---
title: Networking
description: How yoink reaches hosts through SSH and Tailscale, how services spread across hosts, and how yoink pf debugs a service without a published port.
weight: 5
---

Networking in yoink has three layers:

1. **Operator → host**: SSH, together with Tailscale for hostnames, auth, and ACLs.
2. **Service → service**: docker networks on each host, and `services[].hosts` to pin a service to specific hosts.
3. **Operator → service (debugging)**: `yoink pf <service>` opens a tunnel to a container, and no host port is published.

## Connectivity layer (Tailscale + SSH)

The transport of yoink is `ssh://user@host`. yoink opens an SSH tunnel and speaks the Docker Engine API through the Unix socket of the remote daemon. You supply the SSH connectivity layer. yoink does not decide how you reach your hosts.

We recommend **Tailscale SSH** for this layer, and every example in these docs assumes it. There are four reasons:

- **Hostnames work everywhere.** MagicDNS gives every host a stable name (`my-server`) that each device on the tailnet can reach. There is no `ssh_config` to maintain, no jump host, and no bastion.
- **Auth without keys.** Tailscale SSH issues short-lived certs from tailnet membership and ACLs. A new operator needs a tailnet invitation and an ACL grant for the `tag:server` group. When you revoke the tailnet membership of an operator, the access stops on every host immediately.
- **CI authentication is the same flow.** A GH Actions runner with `tailscale/github-action` joins the tailnet as `tag:ci`. An ACL grants SSH from `tag:ci` to `tag:server`. yoink connects and never reads `~/.ssh/`.
- **No port forwarding.** The Docker daemon never listens on a TCP port. The only exposed surface is `:22`, and only the tailnet can reach it.

The deploy user on each host is in the `docker` group. That group is functionally root, so the tailnet ACLs must have a scope that fits.

## What yoink owns vs leaves to the connectivity layer

| concern | tool |
|---|---|
| **container lifecycle** (pull, start, healthcheck, drain, replace) | yoink |
| **deploys in dependency order** (redis before api before caddy) | yoink |
| **network isolation for each tier** | yoink |
| **HTTPS, hostname routing, cert issuance** | yoink (bundled Caddy, see the [proxy guide](/docs/explanation/proxy)) |
| **operator → host connectivity** | Tailscale (or your SSH config) |
| **CI → host connectivity** | Tailscale (or your SSH config) |
| **stateful services** (postgres and others) | yoink (with a named volume) or docker compose on the host |
| **secrets** | yoink (age-sealed) or an external CLI through `provider: command`, see the [secrets guide](/docs/explanation/secrets) |

## Multi-host distribution

Two config keys control how services spread across hosts. `replicas` sets the number of containers on each host. `services[].hosts` sets which hosts run a service.

### Default: every service on every host

By default a service runs on **every** host in `hosts:`, with `replicas` containers on each host.

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

This config gives **4 api containers in total**: 2 on `prod-eu-1` and 2 on `prod-eu-2`. The rolling swap runs on each host independently. On each host, the old replicas run until every new replica passes its healthcheck.

### Pinning a service to specific hosts

`services[].hosts:` is a whitelist of the hosts that run that service. The strings match `hosts[].address`.

```yaml
hosts:
  - { address: prod-eu-1, user: deploy }
  - { address: prod-eu-2, user: deploy }
  - { address: prod-db-1, user: deploy }      # beefier box, dedicated to stateful

services:
  - name: api
    image: ghcr.io/you/api
    domain: api.example.com                    # bundled Caddy auto-injects on every host that runs api
    hosts: [prod-eu-1, prod-eu-2]              # public-facing tier; redis stays internal
    run: { port: 8080, replicas: 2 }

  - name: redis
    image: redis
    tag: 7-alpine
    hosts: [prod-db-1]                         # pin to the db host only
    run: { port: 6379 }
```

[Distribute services across hosts](/docs/how-to/multi-host-distribution) has the config for the common shapes: horizontal scale, a singleton, a pinned stateful service, and one service for each region.

### Cross-host traffic between services

**yoink does not yet route traffic between services on different hosts.** The default network of yoink is a docker bridge network, and a bridge network exists on one host only. Thus an api on `prod-eu-1` cannot dial `redis:6379` on `prod-db-1` through the docker network alone.

Two patterns work today:

- The service that dials runs on the same host as its dependency.
- The stateful service has a `publish:` entry on a tailnet IP, and the consumer dials that tailnet hostname.

Docker overlay networks are a generic solution, and support for them is on the roadmap.

### How `yoink up` schedules across hosts

For each service, yoink computes the host set: `services[].hosts` ∩ `hosts[]`, or all hosts when `services[].hosts` is unset. yoink prepares the new containers on each host in the set, in sequence. Then it swaps one host at a time. On one host, yoink starts and probes the new replicas in sequence, then stops the old containers.

The wave order (`depends_on`) is global. `redis` completes on all its hosts before `api` starts, and the hosts of each service do not change this order.

### Pre-deploy hooks

A `pre_deploy` hook runs **one time**. The number of replicas and the number of hosts do not multiply the hooks, so a migration runs one time. A container hook runs on the first host in `hosts:`.

### Pruning

`yoink prune` examines every host independently. It removes the containers of services that the config no longer declares, and the stopped containers of the other services. It keeps a running container of a declared service, also on a host that `services[].hosts` no longer lists.

## Port-forward

`yoink pf <service>` opens a tunnel from your laptop to a container port, as `kubectl port-forward` does. It uses the SSH connection that yoink already has to the host. The tunnel works for a service with a published host port and for a service without one. **You do not publish a port to debug a service.**

### Why this matters

yoink does not add `publish:` entries by default:

- Backend services stay on the docker network, reachable only through Caddy on `:443`. They are never bound to a host port.
- Operator UIs that need direct access (pgadmin, monitoring) bind `127.0.0.1` through SSH or the tailnet, not `0.0.0.0`.
- The proxy is the only listener on `:443`. With origin-pull mTLS, the origin is reachable only through the edge of Cloudflare.

Debugging is the usual pressure against that default. Without `pf`, there are three workarounds, and each one makes the exposure or the effort worse:

1. A "temporary" `publish:` entry. The line stays in the yaml permanently because its removal is a chore.
2. `docker exec` and curl in the container. This works only for images that contain `curl`, and it gives no browser access.
3. A one-off `docker run --network=container:<target>` shell with socat or nc. This is fiddly and needs a manual cleanup on each host.

`yoink pf` is workaround 3 in automatic form. It uses the same SSH path as the deploy, and it cleans up on Ctrl-C. The TUI opens a tunnel with one key and shows a footer band while tunnels are open. Debugging never changes what production exposes.

### What it works on

`yoink pf` works on every service that yoink runs:

- Services that **publish** a host port (pgadmin and other admin UIs).
- Services with **no `publish:`**: the api and web shape that is secure by default.
- Services on one network or on more than one network.
- Services with replicas. See [Replicas](#replicas) for the routing limits.

`pf` has two paths. If the service has a matching `publish:` entry, `pf` uses the published port. If not, `pf` starts a sidecar container that joins the docker network of the service.

### Replicas

For a service with more than one replica, each connection through the tunnel can go to any healthy replica. The `--host` flag limits the tunnel to the replicas on one host. Sticky routing to one replica index is on the roadmap. Until then, the `↦` marker in the TUI means "this is the tunnel that I opened". It is not a routing guarantee.

### What is guaranteed

- **The same auth path as all other commands.** If `yoink up` works against the host, `yoink pf` works.
- **No image dependencies on the target.** `pf` reaches a `FROM scratch` image, a distroless image, and a full Debian image.
- **Nothing crosses the public internet.** The tunnel is a loopback port on the host, bridged through SSH.
- **Automatic cleanup on exit.** Ctrl-C, `Shift-F`, a TUI exit, and a crash all force-remove the sidecars and free the local port.

## See also

- [Distribute services across hosts](/docs/how-to/multi-host-distribution): config for each common distribution shape.
- [Debug a service with a port-forward](/docs/how-to/port-forward): open, use, and close a tunnel.
- [Port-forward reference](/docs/reference/port-forward): arguments, modes, errors, and TUI keys of `yoink pf`.
- [Security](/docs/explanation/security): why `publish:` is the exception.
- [Reverse proxy](/docs/explanation/proxy): HTTPS and hostname routing for services with `domain:`.
- [Multi-host Let's Encrypt with Redis](/docs/how-to/multi-host-redis-storage): proxy coordination when more than one host serves the same domain.
