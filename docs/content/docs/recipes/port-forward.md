---
title: Port-forward to a published service
weight: 13
---

`yoink pf <service>` opens an `ssh -L` tunnel from your laptop to one of the service's published container ports — the same shape `kubectl port-forward` gives you, but reusing the SSH connection yoink already has to the host. No proxy, no sidecar, no extra TLS termination; the host's `docker-proxy` is already listening on `127.0.0.1:<host_port>`, and yoink just bridges the laptop into it.

## What it works on

`yoink pf` is **only** for services that declare a `publish:` block in `yoink.yaml`. Common cases:

- `pgadmin` published on `127.0.0.1:5050:80` (operator UI behind tailnet).
- An internal admin port published on `127.0.0.1:9090:9090` for ad-hoc curling.
- A databases / consoles you've intentionally bound to loopback for SSH-only access.

For the typical web service that's only reachable through the bundled Caddy proxy on `:443` (api / web in the standard Backtrack-shape config), there's no published port to forward — use [`yoink shell <service>`](/docs/reference/cli) to drop into a docker-exec inside the container instead.

## CLI

```sh
# Service has exactly one publish: that's the one we use, OS-assigned local port.
yoink pf pgadmin

# Equivalent: pin local port = container port (the implicit shape).
yoink pf pgadmin 80

# `LOCAL:CONTAINER` to keep a known local port stable across sessions.
yoink pf pgadmin 5050:80

# Open the URL in the browser as soon as the tunnel is up.
yoink pf pgadmin -o

# Force the URL scheme (HTTP/HTTPS heuristic only knows the obvious ports).
yoink pf my-service 9000 --scheme=https -o

# Print the chosen local port as JSON, then keep tunneling — useful from scripts.
yoink pf pgadmin --json &
LOCAL=$(read -r line; echo "$line" | jq -r .local_port)
curl -s http://localhost:$LOCAL/healthz
```

The process holds the tunnel until you Ctrl-C; on exit the SSH child dies and the local port is freed.

### Errors you'll see

- **`service "x" has no \`publish:\` block`** — yoink doesn't have a host port to bridge to. Use `yoink shell x` to docker-exec inside the container.
- **`service "x" has no published container port 9999 (available: 80, 443)`** — typo. The error lists the container ports the service actually publishes; pick one.
- **`service "x" publishes 80 on more than one host port (got 5050, 5051); pin one`** — rare (services usually publish each container port to exactly one host port), but handle it by passing `LOCAL:CONTAINER` to disambiguate.
- **`ssh probe to <host> failed: …`** — the same probe `yoink up` uses. Tailnet, key, host-key acceptance — fix once and `yoink pf` works for everything.

## TUI

Three keys, all on the focused row in any pane that surfaces a service (Dashboard, Hosts, Services, container detail):

| key | does |
|---|---|
| `f` | Open a port-forward to the focused service. Single-publish services get an OS-assigned local port and a toast with the URL. Multi-publish needs the CLI for now. |
| `o` | Open the active port-forward URL in the system browser. |
| `F` (Shift-F) | Close every active port-forward. The footer band disappears. |

While any tunnel is open, a one-line footer band stays visible across every pane:

```
↦ pgadmin :80 → http://localhost:54321  api :8080 → http://localhost:54322   [o] open  [F] close all
```

The band is hard to miss on purpose — open tunnels are the kind of thing operators forget about and accidentally leave running between sessions. yoink's TUI exit (`q` / Ctrl-C) closes every tunnel cleanly.

## How it works

`yoink pf` is a thin wrapper around `ssh -N -L laptop_port:remote_dial_host:remote_port user@host` against the same SSH config bollard already uses for the docker daemon connection. The remote-dial-host comes from the publish block (`127.0.0.1` for two-token publishes, the explicit IP for three-token), and the remote port is the host port. Container-side: nothing changes; `docker-proxy` was already listening before `pf` ran.

What that buys you:

- **No extra container.** Some other tools spin up a sidecar in the target's network namespace running socat or nc; yoink doesn't need to. The host already has the listener.
- **No image dependencies.** `pf` works whether the target image has socat, curl, anything. Even a `FROM scratch` distroless image port-forwards fine, because we never enter the target container.
- **Same auth path as everything else.** If `yoink up` works against the host, `yoink pf` works. No separate SSH config, no extra keys.

The trade-off is that `pf` only forwards what's already published. That's the whole point — encoding "do you actually want this exposed on the host?" into the yaml is part of yoink's contract.
