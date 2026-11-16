---
title: Debug a service with a port-forward
description: Open a tunnel from your laptop to a container port with yoink pf, from the CLI or the TUI, and close it again.
---

## Prerequisites

- `yoink up` works against the host. `yoink pf` uses the same SSH path.
- The service has a `run.port:`, or you know the container port.
- For a service without a `publish:` block, the service or `deploy.networks:` declares a network.

## Open a tunnel from the CLI

### Start the port-forward [step]

Use the form that fits the service:

```sh
# Auto-mode: published if available, sidecar otherwise.
yoink pf pgadmin              # published path (pgadmin has `publish:`)
yoink pf api                  # sidecar path (api has no publish; uses run.port)
yoink pf api 8080             # explicit container port
yoink pf api 5050:8080        # LOCAL:CONTAINER (stable laptop port across sessions)
yoink pf api 8080 -o          # also open browser when ready
yoink pf web 3000 --scheme=https -o     # force https://
```

### Open the URL [step]

Open the URL that `yoink pf` prints. Make sure that the service answers.

### Close the tunnel [step]

Press `Ctrl-C`. yoink stops the SSH child and frees the local port. If there is a sidecar, yoink force-removes it.

## Force a mode

To bypass docker-proxy and reach the container directly, force the sidecar mode. To get an error instead of a fallback to a sidecar, force the published mode.

```sh
# Mode override:
yoink pf pgadmin --mode=sidecar   # bypass docker-proxy, hit the container directly
yoink pf api --mode=published     # error instead of falling back to sidecar
```

## Use the tunnel from a script

Pass `--json`. The first stdout line is a JSON object with the local port. The tunnel stays open until yoink gets SIGINT.

```sh
# JSON output (script-friendly): first stdout line is `{local_port, mode, url, …}`,
# then keep tunneling.
yoink pf api --json &
LOCAL=$(read -r line; echo "$line" | jq -r .local_port)
curl -s http://localhost:$LOCAL/health
```

## Open a tunnel from the TUI

### Open the port-forward [step]

Select the service row and press `f`. A cyan `↦` shows on the row, and a footer band shows the local URL.

### Open the URL in the browser [step]

Press `o` or `O`. This key works in every view.

### Close all tunnels [step]

Press `F` (Shift-F). The footer band goes away. If you quit the TUI, yoink also closes every tunnel.

## Reach a specific replica

For a service with `replicas: > 1`, each connection can go to any healthy replica. Use one of these methods:

- Pass `--host <ADDRESS>` to limit the tunnel to the replicas on one host. With one replica on each host, this flag pins the tunnel.
- For deterministic access to one replica, add a `publish:` entry for each replica with a different host port.

yoink accepts `--replica <N>` (0-based) but only validates the range. The flag does not pin the tunnel to one replica.

## If the command stops with an error

The [Port-forward reference](/docs/reference/port-forward#errors) lists each error of `yoink pf` and its correction.

## See also

- [Port-forward reference](/docs/reference/port-forward): argument forms, modes, errors, TUI keys, and marker rules.
- [Networking](/docs/explanation/networking#port-forward): why `yoink pf` exists and what it guarantees.
- [TanStack Start + postgres](/docs/tutorials/tanstack-stack): a tutorial that uses `yoink pf` on a finished deploy.
