---
title: Pairing with Tailscale
weight: 5
---

`yoink` doesn't try to solve "how do I reach my hosts." It expects you to bring an off-the-shelf SSH connectivity layer.

{{< callout type="info" >}}
**For routing**, yoink now ships a [bundled Caddy reverse proxy](/docs/guide/proxy) — set `domain:` on a service and you're done. This page covers the connectivity layer (Tailscale) and the rest of the stack split.
{{< /callout >}}

## Tailscale for SSH (the connectivity layer)

Yoink's transport is `ssh://user@host` via [bollard's SSH transport](https://docs.rs/bollard/), which opens an SSH tunnel and speaks the Docker Engine API over the remote daemon's Unix socket. Pairing this with **Tailscale SSH**:

- **Hostnames work everywhere.** MagicDNS gives every host a stable name (`my-server`) reachable from your laptop, CI runner, anywhere on the tailnet. No `ssh_config` to maintain, no jump hosts, no bastion.
- **Auth without keys.** Tailscale SSH issues short-lived certs based on tailnet membership and ACLs. Onboard a new operator: invite to the tailnet, grant ACL access to the `tag:server` group. Done. Off-board: revoke from tailnet. Their key is gone everywhere, immediately.
- **CI authentication is the same flow.** A GH Actions runner with `tailscale/github-action` joins the tailnet under `tag:ci`; ACL grants `tag:ci → tag:server` SSH; yoink connects without ever touching `~/.ssh/`.
- **No port forwarding.** The Docker daemon never listens on a TCP port. The SSH transport handles auth + transport in one hop. Surface area: `:22` accessible only from the tailnet.

The deploy user on each host is in the `docker` group (functionally root, scope your tailnet ACLs accordingly).

## Routing

For HTTPS termination + hostname routing, yoink ships a [bundled Caddy reverse proxy](/docs/guide/proxy). One field on a service:

```yaml
services:
  - name: api
    image: ghcr.io/you/api
    domain: api.example.com
    run: { port: 8080 }
proxy:
  email: ops@example.com
```

ACME issuance, rolling-deploy-synchronized routing flips, and per-service `caddy_extra_json:` for advanced features (auth, rate limit, headers) are all built in. See the [proxy guide](/docs/guide/proxy) and the [Cloudflare Origin Certs recipe](/docs/recipes/cloudflare-origin-certs).

## The split

| concern | tool |
|---|---|
| **container lifecycle** (pull, start, healthcheck, drain, replace) | yoink |
| **dep-ordered deploys** (redis before api before caddy) | yoink |
| **per-tier network isolation** | yoink |
| **HTTPS / hostname routing / cert issuance** | yoink (bundled Caddy) |
| **operator → host connectivity** | Tailscale (or your SSH config) |
| **CI → host connectivity** | Tailscale (or your SSH config) |
| **stateful services** (postgres, etc.) | docker compose on the host |
| **secrets** | yoink (age-sealed) or Infisical |
