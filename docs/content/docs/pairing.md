---
title: Pairing with Tailscale + caddy-docker-proxy
weight: 4
---

`yoink` doesn't try to solve "how do I reach my hosts" or "how do I route HTTPS to the right container". It expects you to bring two off-the-shelf tools that solve those completely.

## Tailscale for SSH (the connectivity layer)

Yoink's transport is `ssh://user@host` via [bollard's SSH transport](https://docs.rs/bollard/), which opens an SSH tunnel and speaks the Docker Engine API over the remote daemon's Unix socket. Pairing this with **Tailscale SSH**:

- **Hostnames work everywhere.** MagicDNS gives every host a stable name (`my-server`) reachable from your laptop, CI runner, anywhere on the tailnet. No `ssh_config` to maintain, no jump hosts, no bastion.
- **Auth without keys.** Tailscale SSH issues short-lived certs based on tailnet membership and ACLs. Onboard a new operator: invite to the tailnet, grant ACL access to the `tag:server` group. Done. Off-board: revoke from tailnet. Their key is gone everywhere, immediately.
- **CI authentication is the same flow.** A GH Actions runner with `tailscale/github-action` joins the tailnet under `tag:ci`; ACL grants `tag:ci → tag:server` SSH; yoink connects without ever touching `~/.ssh/`.
- **No port forwarding.** The Docker daemon never listens on a TCP port. The SSH transport handles auth + transport in one hop. Surface area: `:22` accessible only from the tailnet.

The deploy user on each host is in the `docker` group (functionally root, scope your tailnet ACLs accordingly).

## caddy-docker-proxy for the public surface

Yoink owns the container lifecycle. **It does not own routing.** Public traffic landing on your hosts wants:

- TLS termination (with auto-renewing certs)
- Hostname → container routing
- Per-route headers, redirects, rate limits

[caddy-docker-proxy](https://github.com/lucaslorentz/caddy-docker-proxy) is a Caddy plugin that watches the Docker socket for container labels and reconfigures itself live. The pairing:

```yaml
services:
  - name: api
    image: ghcr.io/you/api
    labels:
      caddy: api.example.com
      caddy.reverse_proxy: "{{upstreams 8080}}"
    run:
      port: 8080
      replicas: 2
      networks: [public, api]
```

`yoink up` deploys the api container with those labels; the caddy container (also yoink-managed, but separately) reads them via the docker socket and routes `https://api.example.com` to the api containers, automatically picking up new replicas and dropping retired ones. Cert issuance is Caddy's job (Let's Encrypt or Cloudflare origin); yoink doesn't know it's happening.

Replicas plug into this naturally: `caddy.reverse_proxy: "{{upstreams 8080}}"` resolves all containers with the same network alias and round-robins between them. yoink's healthcheck-gated rolling swap means the caddy upstream pool is always traffic-ready.

## The split

| concern | tool |
|---|---|
| **container lifecycle** (pull, start, healthcheck, drain, replace) | yoink |
| **dep-ordered deploys** (redis before api before caddy) | yoink |
| **per-tier network isolation** | yoink |
| **HTTPS / hostname routing / cert renewal** | caddy-docker-proxy |
| **operator → host connectivity** | Tailscale |
| **CI → host connectivity** | Tailscale |
| **stateful services** (postgres, etc.) | docker compose on the host |
| **secrets** | Infisical (REST API, no CLI install needed) |
