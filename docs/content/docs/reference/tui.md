---
title: TUI
description: The keybind tables for every TUI pane, the log piping, and the drift detail view.
weight: 3
---

```sh
yoink tui
```

The TUI is a keyboard-driven dashboard for the remote hosts that you deploy to. It shows yoink concepts (services, drift, sealed secrets, deploy history) together with docker introspection, and it updates in real time. It takes ideas from [k9s](https://k9scli.io/) and [lazydocker](https://github.com/jesseduffield/lazydocker).

![yoink TUI dashboard](https://github.com/user-attachments/assets/bcf956b1-937a-43d7-92f6-0f24a1d0cd98)

![yoink TUI container detail](https://github.com/user-attachments/assets/36d3cc00-355c-482b-addc-454a0070b58e)

## What it shows

| pane | purpose |
|---|---|
| **Dashboard** | One row for each yoink-managed container on every host, with state, health, drift, CPU, and memory. |
| **Hosts** | The connection status, docker version, and aggregate CPU and memory of each host. |
| **HostDetail** | Every running container on one host (yoink-managed or not) with live stats, and a docker-events panel. |
| **Services** | One row for each configured service, with image and running count. |
| **ServiceDetail** | Every replica of one service, on every host. |
| **ServiceHistory** | Every past deploy of one service, with rollback. |
| **ContainerDetail** | Image, command, env, ports, mounts, networks, security profile, restart count, and three 5-minute history charts. |
| **Logs** | A live tail of every yoink-managed container in one buffer. |
| **ContainerLogs** | The same view for one container. |
| **Resources** | The images, volumes, and networks of every host. |
| **Secrets** | The individual sealed secrets. |
| **Audit** | The merged operator and host audit log. |

## Top-level navigation

These keys work in every view except the embedded shell and the text inputs.

| key | action |
|---|---|
| `1` / `d` | Dashboard |
| `2` / `h` | Hosts |
| `3` / `s` | Services |
| `4` / `l` | Logs |
| `5` / `R` | Resources (Images / Volumes / Networks) |
| `6` / `e` | Secrets |
| `7` / `a` | Audit log |
| `Tab` / `Shift-Tab` | cycle panes forward / backward |
| `D` | doctor modal: runs the checks of `yoink doctor`. `r` runs them again, `Esc` closes. |
| `E` | open the config in `$EDITOR`, at the line of the focused service or host |
| `~` | show the drift detail for the focused service |
| `f` | port-forward the focused service |
| `o` / `O` | open the URL of the active port-forward in the system browser. When the focused row has no forward, the key uses the first active tunnel. |
| `F` | close every active port-forward (the TUI force-removes sidecars and stops ssh children) |
| `v` | open VS Code in the browser, on the container filesystem of the focused service (not in Resources) |
| `V` | close every active vscode session |
| `?` | show or hide the help overlay |
| `q` / `Ctrl-C` | quit |

`~`, `f`, and `v` need a focused service: a row in Dashboard, HostDetail, or ServiceDetail, or an open ContainerDetail.

`f` uses the published path when the service has a matching `publish:` entry. If there is no match, it starts an `alpine/socat` sidecar on the docker network of the service. A footer band stays visible in all panes until you close the forward. See [Port-forward reference](/docs/reference/port-forward).

## Container and service keys

| key | action | views |
|---|---|---|
| `↑` `↓` / `j` `k` | select a row | all list views |
| `enter` | open the container detail | Dashboard |
| `enter` | open the live logs of the container | HostDetail, ServiceDetail, ContainerDetail |
| `i` | open the container detail | HostDetail, ServiceDetail |
| `!` | open a shell in the container (`bash`, then `sh`) | HostDetail, ServiceDetail, ContainerDetail, ContainerLogs |
| `B` | open a debug sidecar | HostDetail, ServiceDetail, ContainerDetail, ContainerLogs |
| `S` / `X` / `T` | start / stop / restart the container | HostDetail, ContainerDetail |
| `K` | SIGKILL the container | Dashboard, HostDetail, ServiceDetail, ContainerDetail |
| `U` | reconcile the service of the row | Dashboard, HostDetail, ServiceDetail, ContainerDetail |
| `A` | reconcile all services | Dashboard |
| `P` | prune stale and orphan containers | Dashboard |
| `x` | show or hide exited containers | Dashboard |
| `H` | open the deploy history of the service | ServiceDetail |
| `p` | show the processes of the container (`docker top`) in a modal | ContainerDetail |
| `r` | refresh | Dashboard, Hosts, HostDetail, ContainerDetail, Services, ServiceDetail, Resources, Audit |
| `esc` | clear the filter, close a modal, or go back | all views |

Letters are case-sensitive. `K`, `U`, `A`, `P`, and the remove key of Resources and Secrets ask for a confirmation: `y` or `Enter` confirms, and any other key cancels. `S`, `X`, and `T` run immediately.

For a service without `tag:` in the config, `U` uses the tag of the running replica.

## Container detail

The detail pane shows four kinds of information for one container:

1. **Header card**: image, state, command, restart count, and the started, finished, and exit-code values. The right column has live `CPU` / `MEM` gauges and a `NET ↓rx ↑tx` line with the bytes transferred since the container started.
2. **5-minute history charts**: three panels. The title of each panel contains the latest value.
   - **CPU %** (cyan): scaled to peak CPU.
   - **Mem** (magenta): in % when memory has a cap, in MB when it has no cap.
   - **net tx / rx** (yellow / green): `tx` is above the zero line and `rx` is below it. The title shows the current rates, for example `↑1.2MB/s ↓340KB/s`.
3. **Runtime and security blocks**: published ports, mounts, networks, then `cap_drop` / `cap_add` / `security_opt` / `read_only` / `pids_limit` / effective `user`.
4. **Env and labels**: sorted `KEY=value`. The TUI redacts the values of keys that contain `TOKEN`, `SECRET`, `PASSWORD`, `PASS`, `API_KEY`, `PRIVATE_KEY`, or `DSN`.

A rolling tail of the container logs is at the bottom of the pane.

A background poller samples `docker stats` every 2 seconds for every running container on every host, in all views. Thus a chart already contains up to 5 minutes of history when you open the pane.

`esc` returns to HostDetail.

## Drift detail (`~`)

When a row shows ⚠, `~` opens a modal with the per-field diff. The content is the same as `yoink up --plan --service <name>` against the same host:

```
 drift: api on host-a (esc to close)
   image  ghcr.io/me/api  (unchanged)
   spec   a1b2c3d → e5f6a7b
    tag   v1.2.4
env:
  + DATABASE_POOL_SIZE
  ~ LOG_LEVEL
labels:
  - yoink.caddy.tls
```

| marker | color | meaning |
|---|---|---|
| `+` | green | added in the desired spec |
| `-` | red | removed from the running container |
| `~` | yellow | changed |

A hash or image line shows the running → desired transition in cyan when it differs, and a dim `(unchanged)` when it matches.

For a service without `tag:` in the config, the modal uses the tag of the running replica. The diff then shows only the env and label changes.

`Esc` closes the modal.

## Hosts pane (`h`)

The table has one row for each host:

| col | meaning |
|---|---|
| `ssh target` | `user@address` from `yoink.yaml` |
| `status` | `connected` or `error` |
| `server` | docker server version |
| `cpu (cores)`, `memory` | usage of the host, with a gauge |
| `ctr run/all` | running containers / all containers |
| `images` | number of images |
| `platform / error` | OS and architecture, or the error with a hint (for example for Tailscale auth or `ssh-add`) |

`Enter` opens **HostDetail**: every running container on the host, with live CPU and memory, a drift indicator, and the [container keys](#container-and-service-keys). `esc` returns to Hosts.

The **events panel** at the bottom of HostDetail collects live `docker events` for the host (start / stop / die / health-status / kill / oom / restart). Each row is a one-line summary with a relative timestamp. The panel keeps the last 200 events for each host.

## Services pane (`s`)

`Services` lists every service in `yoink.yaml` with its image, running count, health, and hosts. `Enter` opens **ServiceDetail**: one row for each replica on every host, with the [container keys](#container-and-service-keys).

In ServiceDetail, `H` opens **ServiceHistory**: every yoink-managed container with `yoink.service=<name>` (running and exited), newest first. On a row that is not running, `r` opens a reconcile confirmation that is pinned to the tag of that row.

## Resources pane (`R`)

Three sub-tabs show the images, volumes, and networks of every configured host.

| tab | key | columns | actions |
|---|---|---|---|
| **Images** | `i` | host · id · size · age · dangling · tags | `x` remove · `P` prune dangling · `A` prune all unused |
| **Volumes** | `v` | host · name · driver · mountpoint | `x` remove · `P` prune unused |
| **Networks** | `n` | host · name · driver · scope · internal | `x` remove · `P` prune unused |

Inside Resources, `Tab` / `Shift-Tab` cycles between the three sub-tabs. Dangling images sort to the top. If the fetch fails for some hosts, a red footer shows the errors and the table stays visible.

The TUI has no file browser for volumes. A shell in the container (`!` or `B`) gives access to the files.

## Secrets pane (`e`)

The pane shows the individual sealed secrets. It uses the same on-disk format as `yoink secrets edit` and the `secrets.file:` path of the config. The header shows the active file.

| key | action |
|---|---|
| `↑↓` / `j` `k` | select a key |
| `r` | reveal or mask the values |
| `/` | filter on a substring |
| `enter` | edit the selected value (age provider only) |
| `x` | remove the selected secret, with confirmation (age provider only) |

With `provider: command`, the pane is read-only. When no age identity is available, the pane shows why the load failed. For bulk edits and multi-line values, use `yoink secrets edit`.

## Logs pane (`l`)

The Logs view collects a live tail from every yoink-managed container on every host into one buffer. Each line has the container name as a prefix.

| key | action |
|---|---|
| `↑↓` / `PgUp` `PgDn` | scroll 1 line / 10 lines (auto-follow stops while the view is away from the bottom) |
| `g` / `G` / `End` | go to the top / bottom (the bottom starts auto-follow again) |
| `/` | start filter input: a case-insensitive substring. `Enter` applies, `Esc` cancels. |
| `w` | wrap or truncate long lines |
| `L` | cycle the minimum level that `hl` shows: all → trace → debug → info → warning → error |
| `c` | clear the buffer |
| `r` | restart the streams |
| `y` | copy the visible buffer to the system clipboard through OSC-52 |

The buffer has a limit of 5,000 lines. Older lines leave the buffer as new lines arrive. A filter changes the view, not the buffer.

### `ContainerLogs` (single container)

`Enter` on a row in HostDetail or ServiceDetail, or in ContainerDetail, opens this view. It has the keys of the Logs pane except `r` and `L`. `!` and `B` open a shell from the logs. `Esc` returns to HostDetail.

## Audit pane (`a`)

The pane shows the merged operator and host audit log: every state-changing run of yoink (deploy, rollback, prune, secrets rotate). It uses the same data path as `yoink audit log`. The selection is stable on `event_id`, so a refresh keeps the selected row.

| key | action |
|---|---|
| `↑↓` / `j` `k` | select row |
| `Enter` | show or hide the detail panel (full `event_id`, `deploy_id`, actor, git SHA, log tail for `DeployFailed`) |
| `/` | start a substring filter: matches host, event name, summary, `deploy_id`, actor |
| `r` | refresh |
| `Esc` | the first press clears the active filter, the second press returns to Dashboard |

The `origin` column labels each row `operator` (magenta) or `host` (cyan).

## Shell / debug sidecar (`!`, `B`)

Both keys open a PTY through the Docker exec API of the host, with no SSH session on top.

`!` runs `bash` in the container, with `sh` as the fallback. It is equivalent to `yoink shell <service>`. If the image is distroless or has no shell, `!` fails.

`B` starts an alpine debug sidecar that shares the PID and network namespaces of the target. You can run `ps`, `ss`, `cat /proc/<pid>/...`, and `apk add` for other tools, and the production image does not change. yoink removes the sidecar when the shell ends.

| key inside the shell view | action |
|---|---|
| any other key | forwarded to the process in the shell (also `Ctrl-C` and `Ctrl-D`) |
| `Ctrl-Q` | leave the shell and go to HostDetail |
| `?` | show or hide the help overlay |
| `exit` / `Ctrl-D` | end the shell normally |

The TUI sends window resizes to the daemon, so `top`, `vim`, and similar programs reflow.

## Progress modals

Reconcile-one, reconcile-all, and prune show a centered modal that streams the deploy event log. The modal takes every key while the operation runs. `y` copies the text of the modal to the clipboard at any time. After the operation finishes, `Esc` or `Enter` closes the modal.

`reconcile-all` (`A` from Dashboard) adds a status table at the top of the modal, with one row for each service. The states are waiting, pulling, pulled, healthcheck, swapping, then synced (no-op), done, or failed.

## Filter conventions

Dashboard, Hosts, HostDetail, Services, ServiceDetail, and Secrets have the same filter:

- `/` starts input mode. `Backspace` removes a character, `Enter` applies, and `Esc` cancels.
- Outside input mode, `Esc` clears an active filter.
- The filter is a case-insensitive substring match. An empty filter shows everything.

| pane | fields |
|---|---|
| Dashboard | host, service, container name, state, version, networks |
| Hosts | `user@address` |
| HostDetail | container name, service, state, status text |
| Services | service name |
| ServiceDetail | host, container name, state |
| Secrets | key |

## Help overlay (`?`)

`?` shows or hides a modal that lists the global keys and the keys of the current view. `?` or `Esc` closes it. The overlay is available in every view, also in the embedded shell.

## CLI launch flags

```sh
yoink tui                       # Dashboard view by default
yoink tui --mode hosts          # start on a specific top-level pane
yoink tui --mouse               # enable mouse capture (scroll wheel + selection)
```

| Flag or variable | Effect |
|---|---|
| `--mode` | `dashboard` / `hosts` / `services` / `logs` / `resources` / `secrets` / `audit`. |
| `--mouse` | The scroll wheel moves the selection in Hosts, HostDetail, Services, and ServiceDetail, and scrolls the logs views. Off by default, because mouse capture disables the native text selection of your terminal. |
| `YOINK_NO_HL` | When set, the TUI does not look for `hl` and the logs views use raw output. |

The TUI writes its own log to `$XDG_STATE_HOME/yoink/tui.log` (default `~/.local/state/yoink/tui.log`).

## Pretty logs

When [`hl`](https://github.com/pamburus/hl) (`brew install pamburus/tap/hl`) is on your `PATH`, the logs views pipe the stream of every container through it. `hl` colors JSON keys, dims timestamps, highlights levels, and keeps stack traces readable. When `hl` is not installed, the views use raw output. No config key controls this behavior.

`yoink logs <svc> -f` on the CLI does not pipe automatically. `yoink logs api -f | hl` gives the same result.

## See also

- [CLI reference](/docs/reference/cli): the same commands that the TUI binds to keys, in a form for scripts.
- [Driving yoink from an AI agent](/docs/explanation/ai-agents): the CLI surface that the TUI mirrors, for non-interactive use.
- [Troubleshooting](/docs/how-to/troubleshooting): help for when the TUI shows something that you do not expect.
