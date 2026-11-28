---
title: Port-forward reference
description: The argument forms, modes, errors, TUI keys, row markers, and guarantees of yoink pf.
---

`yoink pf <service>` opens a tunnel from your laptop to a container port through the SSH connection that yoink has to the host. The process holds the tunnel until Ctrl-C.

## Argument forms

| Command | Container port | Local port |
|---|---|---|
| `yoink pf pgadmin` (one `publish:` entry) | The container port of the entry. | The host port of the entry. |
| `yoink pf api` (no `publish:`) | `run.port`. | OS-assigned. |
| `yoink pf api 8080` | 8080. | 8080. |
| `yoink pf api 5050:8080` | 8080. | 5050 (`LOCAL:CONTAINER`). |
| `yoink pf api 0:8080` | 8080. | OS-assigned. |

| Flag | Result |
|---|---|
| `-o`, `--open` | Opens the browser when the tunnel is ready. |
| `--scheme=https` | Forces `https://` in the URL. The values are `auto`, `http`, `https`, `tcp`, and `none`. `tcp` and `none` print `localhost:N` and open no browser. |
| `--json` | The first stdout line is `{local_port, mode, url, …}`. Then the tunnel stays open. |

With `--scheme=auto` (default), container ports 80, 3000, 5000, 5050, 8000, 8080, 8081, 8443, and 9000 give `http://`. Port 443 gives `https://`. All other ports give `localhost:N`.

## Modes

| Mode | Behavior |
|---|---|
| auto (default) | Published path if a `publish:` entry matches the container port. Sidecar if not. |
| `--mode=sidecar` | Always starts a sidecar. Bypasses docker-proxy and reaches the container directly. |
| `--mode=published` | Published path only. Stops with an error when no `publish:` entry matches. |

The published path bridges to the host port that `docker-proxy` already listens on. The sidecar path starts an `alpine/socat` container on the first docker network of the service that is not `yoink-proxy-admin`. Only the proxy and its admin relay can join `yoink-proxy-admin`. The sidecar publishes a random port on `127.0.0.1` of the host.

## Replica flags

| Flag | Behavior |
|---|---|
| none | For `replicas: > 1`, each connection through a sidecar can go to any replica. |
| `--host <ADDRESS>` | Selects the host. Required when the service runs on more than one host. |
| `--replica <N>` (0-based) | Accepted. At this time yoink validates the range only. Sticky routing to one replica is on the roadmap. |

A `publish:` entry for each replica, with different host ports, gives deterministic access to one replica.

## Exit behavior

| Event | Result |
|---|---|
| Ctrl-C | yoink force-removes the sidecar (if there is one). The SSH child stops. The local port is free. |
| `Shift-F` in the TUI | Same as Ctrl-C, for every tunnel. |
| TUI exit | Every tunnel closes. |
| Crash | Cleanup is best-effort. A `yoink-pf-*` sidecar can stay on the host. |

## Errors

| Error | Cause and correction |
|---|---|
| ``service "x" has no `publish:` block and `--mode published` was forced`` | Remove the flag (auto mode falls back to a sidecar), or pass `--mode sidecar` explicitly. |
| `service "x" has no published container port N (available: …)` | `--mode published` with a port that no `publish:` entry has. Use a port from the list, or remove the flag. |
| `service "x" publishes N on more than one host port` | Two `publish:` entries have the same container port. Remove one entry, or pass `--mode sidecar`. |
| ``service "x" declares no `networks:` `` | Sidecar mode needs a docker network to join, other than `yoink-proxy-admin`. Add a `networks:` entry to the service or to `deploy.networks:`. |
| ``service "x" has no `publish:` and no `run.port:` `` | With no port argument, yoink uses `run.port`. Set `run.port`, or pass the container port explicitly. |
| `service "x" runs on N hosts; pin one with --host:` | Pass `--host <ADDRESS>`. |
| `ssh probe to <host>: ssh probe failed: …` | This is the same probe that `yoink up` uses. Correct the tailnet membership, the SSH key, or the host-key acceptance. |

## TUI keys

| key | does |
|---|---|
| `f` | Opens a port-forward to the focused service in auto mode. The container port is the one `publish:` entry of the service, or `run.port`. |
| `o` / `O` | Opens the URL of the port-forward of the focused service in the system browser. Works in **every** view. If the focused row has no forward, the key uses the first active tunnel. |
| `F` (Shift-F) | Closes every active port-forward. yoink force-removes the sidecars and stops the ssh children. The footer band goes away. |

## Row marker

A row whose service has an open tunnel shows a cyan `↦` prefix on the service cell.

| `f` pressed in | Rows with the marker |
|---|---|
| Dashboard, HostDetail, or ContainerDetail | Only the selected replica. |
| ServiceDetail | Every replica of the service. |

```
host            service      container             state    ...
backtrack-eu-1  ↦ web        web-13584766-0       running  ...   ← f pressed here
backtrack-eu-1    web        web-13584766-1       running  ...
backtrack-eu-1  ↦ api        api-186bd0cd-0       running  ...   ← f pressed in ServiceDetail
backtrack-eu-1  ↦ api        api-186bd0cd-1       running  ...   ← marked too (all replicas)
```

The marker shows the row that was selected when you pressed `f`. It is not a routing guarantee. The TUI does not show tunnels that `yoink pf` on the CLI opened.

## Footer band

While a tunnel is open, a one-line footer band stays visible in every pane:

```
↦ api :8080 → http://localhost:54321  pgadmin :80 → http://localhost:54322   [o] open  [F] close all
```

## Guarantees

| Guarantee | Detail |
|---|---|
| Same auth path | If `yoink up` works against the host, `yoink pf` works. |
| No image dependencies on the target | `pf` reaches `FROM scratch`, distroless, and full Debian images. |
| Nothing crosses the public internet | The sidecar port is on the loopback interface of the host, and the tunnel goes through SSH. |

## See also

- [Debug a service with a port-forward](/docs/how-to/port-forward): the steps to open, use, and close a tunnel.
- [Networking](/docs/explanation/networking#port-forward): why `yoink pf` exists.
- [CLI reference](/docs/reference/cli): the full flag list of `yoink pf`.
- [TUI reference](/docs/reference/tui): all other keys of the TUI.
