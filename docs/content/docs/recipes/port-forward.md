---
title: Port-forward to any service
weight: 13
---

`yoink pf <service>` opens a tunnel from your laptop to a container port — the same shape `kubectl port-forward` gives you, reusing the SSH connection yoink already has to the host. Two paths under the hood, picked automatically:

- **Published path.** When the service declares a `publish:` entry that matches the requested container port, yoink does a single `ssh -L` to the host's existing `docker-proxy` listener. No extra container, ~50 ms to bind.
- **Sidecar path.** When the service has no `publish:` (the secure-by-default api/web shape — only reachable via Caddy on :443), yoink spawns an ephemeral `alpine/socat` sidecar that joins the same docker network, listens on its own internal port, and forwards to `<service-alias>:<container-port>`. Yoink then `ssh -L`s to the sidecar's published-on-loopback port. The sidecar lives only as long as the `pf` invocation; Drop force-removes it.

Either way the operator-visible UX is identical: an open URL, `Ctrl-C` / `Shift-F` closes everything cleanly. **You don't need to publish a port to debug a service.** The sidecar path is the secure-by-default story: production stays "no `publish:` for api/web" and `yoink pf` Just Works.

## Why this matters — make the secure default the easy one

The biggest production-security win on a single-host docker deploy is *not publishing host ports*. Specifically:

- The **reverse proxy** (yoink-proxy / Caddy) terminates TLS and is the only thing that should bind `:443` (and `:80` for the redirect). Cloudflare's edge is the only thing that should reach it.
- Every backend service — api, web, the SSR worker, internal admin endpoints — should sit on a **docker network with no host-side port binding**. They're reachable through Caddy on the public side, and through docker DNS aliases (`api`, `web`, …) for in-network calls.
- Operator UIs (pgadmin, monitoring dashboards) that genuinely need to be operator-reachable should bind `127.0.0.1:<port>` only — never `0.0.0.0` — so they're inaccessible from the public internet but reachable through SSH from a laptop on the tailnet.

This shape buys you a lot:

- **Smaller attack surface.** A misconfigured firewall or a kernel that suddenly forwards `0.0.0.0` ports doesn't matter — the ports aren't bound there in the first place.
- **No accidental exposure.** Adding a service is "declare it in yoink.yaml, attach to a network." There's no checklist of "and remember to NOT publish unless you really need to" — the default doesn't publish.
- **Origin-pull mTLS actually works.** If the only public-facing port is `:443` and that listener is locked to Cloudflare's CA via `client_auth: require_and_verify`, the origin is genuinely unreachable from anywhere except Cloudflare's edge. Direct hits to the IP fail at TLS handshake.

The standard objection: **"but how do I debug api / poke a database / hit an internal admin endpoint when something's wrong at 2 AM?"** Most ops teams answer this with one of three workarounds, each of which weakens the default:

1. **Add `publish:` "temporarily"** so you can curl from your laptop. The temporary publish stays in the yaml because removing it after the incident is a chore. Now api is on the public internet for the rest of forever.
2. **`docker exec` into the target** and curl localhost from inside. Works, but only if the target image happens to ship `curl` / `wget` (most distroless / `FROM scratch` images don't), and only if the operator wants to see one ad-hoc response — there's no way to point a real browser at a debug UI this way.
3. **Run a one-off `docker run --network=container:<target>` shell** with socat / nc / curl pre-installed. Real-but-fiddly. Different invocation per host, manual cleanup, no shared muscle memory across the team.

`yoink pf` is the ergonomic version of #3 — but with auto-cleanup, one-key TUI invocation, and a footer band that makes the open tunnel impossible to forget. The "should I temporarily publish this port?" question disappears: **debugging never requires changing what's exposed in production**. The locked-down default stays the only default; the on-call operator gets a browser-pointable URL in <2 seconds without editing yoink.yaml or restarting anything.

In other words: the security posture and the debugging posture stop fighting. The right default for production *is* the right default for everything; `pf` papers over the awkwardness that used to make operators reach for `publish:` as a workaround.

## What it works on

Anything yoink runs:

- **Published** (pgadmin on `127.0.0.1:5050:80`, the operator UI): published path. Fastest.
- **Caddy-fronted, sealed-network** (api on the `api` network, no `publish:`): sidecar path. ~500 ms cold-start; ~5 MB image pulled once per host.
- **Multi-network** (web on `web` + `otel`): sidecar joins the first declared network and the operator dials the service's docker DNS alias.

## CLI

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

### Errors you'll see

- **`service "x" has no \`publish:\` block and \`--mode published\` was forced`** — drop the flag (auto mode falls back to a sidecar) or pass `--mode sidecar` explicitly.
- **`service "x" declares no \`networks:\``** — sidecar mode needs a docker network to join. Add a `networks:` entry to the service or to `deploy.networks:`.
- **`service "x" has no \`publish:\` and no \`run.port:\``** — when no port arg is given, yoink defaults to `run.port`. Set one or pass the container port explicitly.
- **`ssh probe to <host> failed: …`** — the same probe `yoink up` uses. Tailnet, key, host-key acceptance — fix once and `yoink pf` works for everything.

## TUI

Three keys plus a visual indicator on every row whose service has a tunnel open:

| key | does |
|---|---|
| `f` | Open a port-forward to the focused service. Auto-mode: published path when there's a matching `publish:` entry, else spawns a sidecar. |
| `o` / `O` | Open the active port-forward URL in the system browser. Works in **any** view; falls back to the most-recently-opened tunnel when the focused row has no forward of its own. |
| `F` (Shift-F) | Close every active port-forward. Sidecars are force-removed; ssh children killed. The footer band disappears. |

Forwarded service rows show a cyan `↦` prefix on the service cell across the Dashboard, HostDetail, and Services panes — at-a-glance "is this thing tunneled?" without needing to read the footer.

```
host          service      container             state    ...
backtrack-eu-1 ↦ api       api-186bd0cd-0       running  ...
backtrack-eu-1   web        web-13584766-0       running  ...
```

Sidecars themselves are filtered out of the container lists (their names start with `yoink-pf-`); they exist for the duration of the tunnel and aren't user-facing.

While any tunnel is open, a one-line footer band stays visible across every pane:

```
↦ api :8080 → http://localhost:54321  pgadmin :80 → http://localhost:54322   [o] open  [F] close all
```

The band is hard to miss on purpose — open tunnels are the kind of thing operators forget about and accidentally leave running between sessions. Yoink's TUI exit (`q` / Ctrl-C) closes every tunnel cleanly: ssh children die synchronously, sidecar containers force-remove via the `auto_remove: true` belt-and-braces.

## How it works

### Published path

When the service publishes the requested port, yoink runs `ssh -N -L laptop_port:remote_dial_host:remote_port user@host` against the same SSH config bollard already uses for the docker daemon connection. The remote-dial-host comes from the publish block (`127.0.0.1` for two-token publishes, the explicit IP for three-token); the remote port is the host port. `docker-proxy` was already listening before `pf` ran. ~50 ms cold-start.

### Sidecar path

When the service doesn't publish the requested port, yoink:

1. Pulls `alpine/socat:latest` on the host (no-op after the first run; ~5 MB).
2. Spawns a one-shot container named `yoink-pf-<service>-<port>-<id>` that joins the same docker network as the target and publishes its own internal port `1080` to a random `127.0.0.1:<host_port>` on the host. Inside, it runs `socat tcp-listen:1080,fork,reuseaddr tcp:<service-alias>:<container-port>`.
3. Inspects the container for the docker-assigned host port.
4. SSH-tunnels the laptop to that loopback host port.
5. On `pf` exit (Ctrl-C, `Shift-F`, TUI exit, panic), Drop fires a `force-remove` against the sidecar. Belt-and-braces `auto_remove: true` catches the case where Drop runs after the runtime has torn down.

The sidecar is labelled `yoink.kind=pf-sidecar` so a future `yoink prune` pass can sweep stragglers if `pf` ever crashes mid-flight.

What both paths share:

- **Same auth path as everything else.** If `yoink up` works against the host, `yoink pf` works. No separate SSH config, no extra keys.
- **No image dependencies on the target.** Whether the target is `FROM scratch` or full Debian, `pf` reaches it the same way (the published path doesn't enter the target; the sidecar speaks docker DNS to it).
- **Loopback-only on the host.** Both the published path and the sidecar's published port bind `127.0.0.1` — the laptop reaches them through SSH; nothing fronts the public internet.
