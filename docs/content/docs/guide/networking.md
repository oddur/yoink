---
title: Networking
weight: 5
---

How yoink reaches your hosts, how services reach each other across them, and how you debug a service without weakening the security posture. Three layers:

1. **Operator → host**: SSH, paired with Tailscale for hostname + auth + ACLs.
2. **Service → service**: docker networks per `yoink up` host, plus `services[].hosts` to pin which boxes run what.
3. **Operator → service (debugging)**: `yoink pf <service>` tunnels to any container without publishing host ports.

## Connectivity layer (Tailscale + SSH)

Yoink's transport is `ssh://user@host` — it opens an SSH tunnel and speaks the Docker Engine API over the remote daemon's Unix socket. Yoink doesn't try to solve "how do I reach my hosts" — it expects you to bring an off-the-shelf SSH connectivity layer.

Pairing this with **Tailscale SSH** is what we recommend, and what every example in these docs assumes:

- **Hostnames work everywhere.** MagicDNS gives every host a stable name (`my-server`) reachable from your laptop, CI runner, anywhere on the tailnet. No `ssh_config` to maintain, no jump hosts, no bastion.
- **Auth without keys.** Tailscale SSH issues short-lived certs based on tailnet membership and ACLs. Onboard a new operator: invite to the tailnet, grant ACL access to the `tag:server` group. Done. Off-board: revoke from tailnet. Their key is gone everywhere, immediately.
- **CI authentication is the same flow.** A GH Actions runner with `tailscale/github-action` joins the tailnet under `tag:ci`; ACL grants `tag:ci → tag:server` SSH; yoink connects without ever touching `~/.ssh/`.
- **No port forwarding.** The Docker daemon never listens on a TCP port. The SSH transport handles auth + transport in one hop. Surface area: `:22` accessible only from the tailnet.

The deploy user on each host is in the `docker` group (functionally root, scope your tailnet ACLs accordingly).

## What yoink owns vs leaves to the connectivity layer

| concern | tool |
|---|---|
| **container lifecycle** (pull, start, healthcheck, drain, replace) | yoink |
| **dep-ordered deploys** (redis before api before caddy) | yoink |
| **per-tier network isolation** | yoink |
| **HTTPS / hostname routing / cert issuance** | yoink (bundled Caddy — see [proxy guide](/docs/guide/proxy)) |
| **operator → host connectivity** | Tailscale (or your SSH config) |
| **CI → host connectivity** | Tailscale (or your SSH config) |
| **stateful services** (postgres, etc.) | yoink (with a named volume) or docker compose on the host |
| **secrets** | yoink (age-sealed) or any external CLI via `provider: command` — see [secrets guide](/docs/guide/secrets) |

## Multi-host distribution

How services spread across hosts when you scale beyond one box. Two knobs: `replicas` (per host) and `services[].hosts` (which hosts run a service).

### Default: every service on every host

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

### Pinning a service to specific hosts

`services[].hosts:` whitelists which hosts run that service. Strings match `hosts[].address`.

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

### Common shapes

**Stateless app, scale horizontally:**

```yaml
hosts: [prod-eu-1, prod-eu-2, prod-eu-3]
services:
  - name: api
    run: { replicas: 2 }     # 6 total — 2 per host
```

**Singleton (cron, queue worker):**

```yaml
services:
  - name: scheduler
    hosts: [prod-eu-1]       # one host
    run: { replicas: 1 }     # one container — singleton
```

**Stateful pinned + stateless replicated** (single-host shape — see caveat below for multi-host):

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

> **Cross-host service-to-service traffic is not yet first-class.** Docker bridge networks (yoink's default) are per-host, so an api on `prod-eu-1` cannot dial `redis:6379` on `prod-db-1` over the docker network alone. Today's working patterns: pin the dialing service to the same host as its dependency (above), or expose the stateful service on a tailnet IP via `publish:` and have the consumer dial that tailnet hostname. Docker overlay networks would solve this generically; first-class support is on the roadmap.

**Region-pinned:**

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

### How `yoink up` schedules across hosts

For each service, yoink computes its host set (`services[].hosts` ∩ `hosts[]`, defaulting to all hosts when unset) and runs the reconcile in parallel across those hosts. Within a host, the rolling swap is sequential per replica (start new → healthcheck → swap → drain old).

Wave ordering (`depends_on`) is global — `redis` finishes its host fan-out before `api` starts, regardless of which hosts each lands on.

### Pre-deploy hooks

`pre_deploy` hooks run **once per `up`**, on the first host that has the service. Don't multiply by replica count or host count — migrations run once, full stop.

### Pruning

`yoink prune` walks every host independently, removing containers and images that don't match any current service definition. A service that used to run on `prod-eu-2` but is now pinned to `prod-eu-1` gets cleaned up on `prod-eu-2` automatically.

## Port-forward

`yoink pf <service>` opens a tunnel from your laptop to a container port — the same shape `kubectl port-forward` gives you, reusing the SSH connection yoink already has to the host. Works whether or not the service publishes a host port; **you don't need to publish anything to debug a service**. Open URL, `Ctrl-C` to close.

### Why this matters

Yoink doesn't add `publish:` entries by default — backend services sit on the docker network and are reachable through Caddy on `:443`, never bound to a host port. Operator UIs (pgadmin, monitoring) that need direct reach bind `127.0.0.1` over SSH/tailnet, not `0.0.0.0`. The proxy is the only thing on `:443`; with origin-pull mTLS, the origin is unreachable except through Cloudflare's edge.

The standard pressure to weaken that default is debugging. Without `pf`, operators reach for one of three workarounds, each of which makes it worse:

1. Add `publish:` "temporarily" — and the line stays in the yaml forever because removing it is a chore.
2. `docker exec` + curl from inside — only works on images that ship `curl`, no browser path.
3. One-off `docker run --network=container:<target>` shell with socat / nc — real-but-fiddly, manual cleanup per host.

`yoink pf` is option 3 made automatic: same SSH path as the deploy, auto-cleanup on Ctrl-C, one-key TUI invocation, footer band so you don't forget tunnels are open. **Debugging never requires changing what's exposed in production** — the locked-down default stays the only default.

### What it works on

Anything yoink runs:

- Services that **publish** a host port (pgadmin / admin UIs).
- Services with **no `publish:`** — the secure-by-default api/web shape only reachable via Caddy on `:443`.
- Services on a single network or multi-network.
- Replicated services. See [Replicas](#replicas) below for routing caveats.

### CLI

```sh
# Auto-mode: published if available, sidecar otherwise.
yoink pf pgadmin              # published path (pgadmin has `publish:`)
yoink pf api                  # sidecar path (api has no publish; uses run.port)
yoink pf api 8080             # explicit container port
yoink pf api 5050:8080        # LOCAL:CONTAINER (stable laptop port across sessions)
yoink pf api 8080 -o          # also open browser when ready
yoink pf web 3000 --scheme=https -o     # force https://

# Mode override:
yoink pf pgadmin --mode=sidecar   # bypass docker-proxy, hit the container directly
yoink pf api --mode=published     # error instead of falling back to sidecar

# JSON output (script-friendly): first stdout line is `{local_port, mode, url, …}`,
# then keep tunneling.
yoink pf api --json &
LOCAL=$(read -r line; echo "$line" | jq -r .local_port)
curl -s http://localhost:$LOCAL/health
```

The process holds the tunnel until you Ctrl-C; on exit the SSH child dies, the sidecar (if any) is force-removed, and the local port is freed.

#### Errors you'll see

- **`service "x" has no \`publish:\` block and \`--mode published\` was forced`** — drop the flag (auto mode falls back to a sidecar) or pass `--mode sidecar` explicitly.
- **`service "x" declares no \`networks:\``** — sidecar mode needs a docker network to join. Add a `networks:` entry to the service or to `deploy.networks:`.
- **`service "x" has no \`publish:\` and no \`run.port:\``** — when no port arg is given, yoink defaults to `run.port`. Set one or pass the container port explicitly.
- **`ssh probe to <host> failed: …`** — the same probe `yoink up` uses. Tailnet, key, host-key acceptance — fix once and `yoink pf` works for everything.

### TUI

Three keys plus a visual indicator on every row whose service has a tunnel open:

| key | does |
|---|---|
| `f` | Open a port-forward to the focused service. Auto-mode: published path when there's a matching `publish:` entry, else spawns a sidecar. |
| `o` / `O` | Open the active port-forward URL in the system browser. Works in **any** view; falls back to the most-recently-opened tunnel when the focused row has no forward of its own. |
| `F` (Shift-F) | Close every active port-forward. Sidecars are force-removed; ssh children killed. The footer band disappears. |

Forwarded rows show a cyan `↦` prefix on the service cell — at-a-glance "is this thing tunneled?" without needing to read the footer. The marker scope follows how the tunnel was opened:

- **From a Dashboard / HostDetail / ContainerDetail row** — the operator pressed `f` on a specific replica. Only that replica gets the marker. The other replicas of the same service render unmarked, so a `web-1`/`web-2` pair shows which one you're tunneled through.
- **From the Services pane** or **the CLI** (no row context) — every replica of the service gets the marker. Useful when the operator just cares "is `api` reachable?" rather than "which `api` am I hitting?"

```
host            service      container             state    ...
backtrack-eu-1  ↦ web        web-13584766-0       running  ...   ← f pressed here
backtrack-eu-1    web        web-13584766-1       running  ...
backtrack-eu-1  ↦ api        api-186bd0cd-0       running  ...   ← yoink pf api on the CLI
backtrack-eu-1  ↦ api        api-186bd0cd-1       running  ...   ← marked too (CLI = all replicas)
```

While any tunnel is open, a one-line footer band stays visible across every pane:

```
↦ api :8080 → http://localhost:54321  pgadmin :80 → http://localhost:54322   [o] open  [F] close all
```

The band is hard to miss on purpose — open tunnels are the kind of thing operators forget about and accidentally leave running between sessions. Quitting the TUI closes every tunnel cleanly.

### Replicas

For services with `replicas: > 1`, the tunnel may land on any healthy replica per connection. Pin the routing with one of:

- **`--host <ADDRESS>`** — restrict the tunnel to replicas on a specific host. With one replica per host, that's enough to pin.
- **Use a `publish:` entry per replica** with distinct host ports for fully deterministic single-replica access.
- **`--replica <N>` (0-based)** is accepted but currently validates range only; sticky per-replica routing is on the roadmap.

The TUI's `↦` marker shows which replica row was selected when you pressed `f`. Treat it as "this is the tunnel I opened" — not a routing guarantee.

### What's guaranteed

- **Same auth path as everything else.** If `yoink up` works against the host, `yoink pf` works.
- **No image dependencies on the target.** Whether the target is `FROM scratch`, distroless, or full Debian, `pf` reaches it.
- **Nothing crosses the public internet.** Tunnel is loopback-on-host bridged over SSH.
- **Auto-cleanup on exit.** Ctrl-C / `Shift-F` / TUI exit / crash all force-remove any sidecars and free the local port.

## See also

- [Security](/docs/guide/security) — why `publish:` should be the exception, not the rule.
- [Reverse proxy](/docs/guide/proxy) — HTTPS / hostname routing for services with `domain:`.
- [Multi-host Let's Encrypt with Redis](/docs/recipes/multi-host-redis-storage) — proxy-side coordination when more than one host fronts the same domain.
- [Run staging alongside prod](/docs/recipes/staging-alongside-prod) — same primitives, separate `yoink.yaml` per environment.
- [TanStack Start + postgres](/docs/recipes/tanstack-stack) — end-to-end recipe that uses `yoink pf` to verify the deploy.
- [CLI reference: pf](/docs/reference/cli) — full flag surface.
- [Configuration reference](/docs/reference/config) — full `hosts:` / `replicas:` / `pin:` schema.
