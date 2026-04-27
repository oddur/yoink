---
title: TUI
weight: 3
---

```sh
yoink tui
```

Heavily inspired by [k9s](https://k9scli.io/) and [lazydocker](https://github.com/jesseduffield/lazydocker), but pointed at remote hosts you deploy to instead of just the local docker daemon. Keyboard-driven, real-time, and aware of yoink-specific concepts (services, drift, sealed secrets, deploy history) on top of the everyday docker introspection an operator wants when something is on fire.

![yoink TUI dashboard](https://github.com/user-attachments/assets/bcf956b1-937a-43d7-92f6-0f24a1d0cd98)

![yoink TUI container detail](https://github.com/user-attachments/assets/36d3cc00-355c-482b-addc-454a0070b58e)

## What it shows

| pane | what it's for |
|---|---|
| **Dashboard** | one row per yoink-managed container across every host, with drift, CPU/mem, and exit-code colour. The "is anything broken right now" view. |
| **Hosts** | per-host preflight (ssh, daemon version, kernel/OS) + aggregate CPU/mem across the host. Drill in for a per-host table. |
| **HostDetail** | every running container on a host (yoink-managed *or not*) with live stats, plus a rolling per-host docker-events panel at the bottom. |
| **Services** | one row per configured service with replica count + image. Drill in for `ServiceDetail` (every replica across every host) → `ServiceHistory` (every past deploy → roll back from here). |
| **ContainerDetail** | k9s-style "describe": image, command, env (secrets redacted), ports, mounts, networks, security profile (cap_drop / read_only / pids_limit), restart count, **5-minute history charts** for CPU% + Mem%, and another for net rx/tx rate. `p` opens a `docker top` modal listing in-container processes. |
| **Logs** | a multiplexed live tail of every yoink-managed container (auto-piped through [`hl`](https://github.com/pamburus/hl) when present). `/` filters substring, `g`/`G` jump to top/bottom, `y` yanks the visible buffer to the system clipboard via OSC-52. |
| **ContainerLogs** | same shape, scoped to one container. |
| **Resources** | three sub-tabs (`Images`, `Volumes`, `Networks`) covering everything lazydocker exposes. Per-row remove with `d`; per-tab `P` prune (`A` for the aggressive image prune that goes beyond `<none>:<none>`). |
| **Secrets** | view / add / edit / remove individual sealed secrets without leaving the TUI; reuses the same on-disk format as `yoink secrets edit`. |

## Top-level navigation

| key | mode |
|---|---|
| `d` | Dashboard |
| `h` | Hosts |
| `s` | Services |
| `l` | Logs |
| `R` | Resources (Images / Volumes / Networks) |
| `e` | Encrypted-secrets |
| `Tab` / `Shift-Tab` | cycle modes forward / backward |
| `?` | toggle help overlay (per-view keybinds) |
| `q` / `Ctrl-C` | quit |

## Operator gestures from the dashboard

| key | action |
|---|---|
| `↑` `↓` / `j` `k` | navigate |
| `enter` | drill into selected row (container detail) |
| `i` | container inspect (security & limits, env, mounts, networks) — also from HostDetail / ServiceDetail |
| `K` | SIGKILL container (with confirmation) |
| `S` / `X` / `R` | start / stop / restart container — from HostDetail or ContainerDetail |
| `U` | reconcile this service (with confirmation) — drift-only services no-op |
| `A` | reconcile **all** services (with confirmation) |
| `P` | prune stale + orphan containers (with confirmation) |
| `!` | shell into container (`bash` then fallback to `sh`) |
| `D` | debug sidecar (alpine in target's pid+net ns — for distroless / shell-less images) |
| `H` | service deploy history; on a stopped row press `r` to roll back |
| `x` | toggle eXited containers visible in the table |
| `r` | refresh |
| `/` | filter substring (Esc clears) |

## Container detail (`i` from any list view)

The detail pane renders four kinds of information for a single container:

1. **Header card** — image, state (colored), command, restart count, started/finished/exit-code timestamps.
2. **5-minute history charts** — left panel plots CPU% (cyan) and either Mem% (magenta) when a memory cap is set or absolute MB when uncapped, sharing a 0–100 y-axis when both metrics are percentages. Right panel plots network rx (green) / tx (yellow) rates derived from successive cumulative-bytes samples.
3. **Runtime + security blocks** — published ports, mounts, attached networks, then `cap_drop` / `cap_add` / `security_opt` / `read_only` / `pids_limit` / effective `user` so you can see at a glance whether this container is hardened.
4. **Env + labels** — sorted KEY=value with secret-ish keys (`*TOKEN*`, `*SECRET*`, `*PASSWORD*`, `*API_KEY*`, `*PRIVATE_KEY*`, `*DSN*`) auto-redacted; full label table including the `yoink.*` set.

Plus, at the bottom of the pane, a rolling tail of the container's logs.

| key | action |
|---|---|
| `enter` / `l` | open dedicated logs view |
| `!` | exec a shell inside (`bash` → `sh`) |
| `D` | debug sidecar (alpine sharing pid+net ns) |
| `S` / `X` / `R` | start / stop / restart |
| `K` | SIGKILL (with confirmation) |
| `U` | reconcile this service |
| `p` | docker top — shows in-container processes in a modal (Esc to close) |
| `r` | refresh |
| `esc` | back to host detail |

If your image is distroless or otherwise has no shell, `!` will fail. **Fall back to `D`** — the debug sidecar attaches an alpine container sharing the target's PID and network namespaces, so you can run `ps`, `ss`, `cat /proc/<pid>/...` against the target without modifying the production image. The sidecar auto-removes when you `exit` / Ctrl-D.

## Hosts pane (`h`)

Per-host summary table:

| col | meaning |
|---|---|
| `host` | `user@address` from `yoink.yaml` |
| `status` | ssh probe + bollard handshake — `ok` / `unreachable` (with classified hint for Tailscale auth, `ssh-add` reminders, etc.) |
| `daemon` | docker server version, OS / kernel |
| `cpu`, `mem` | aggregate across all running containers (sum of `docker stats`) — gauge-coloured |

`Enter` drills into **HostDetail**, which shows every running container on the host (whether yoink manages it or not), with the same live CPU/mem cells, drift indicator, and per-container actions:

| key | action |
|---|---|
| `↑↓` / `j` `k` | select container |
| `enter` | live logs |
| `i` | container detail (env, mounts, security, history charts, …) |
| `!` | shell · `D` debug sidecar |
| `S` / `X` / `R` | start · stop · restart |
| `K` | SIGKILL (with confirmation) |
| `U` | reconcile this service |
| `/` | filter substring · `esc` clears |
| `r` | refresh |
| `esc` | back to Hosts (when no active filter) |

The **events panel** at the bottom of HostDetail collects live `docker events` for that host (start / stop / die / health-status / kill / oom / restart). Each row is a one-line summary timestamped with relative time. The ring keeps the last 200 events per host — long enough that an operator returning to the pane after a reconcile sees the full sequence of swaps, not just the final state.

## Services pane (`s`)

`Services` lists every service in `yoink.yaml` with its current replica count and configured image. `Enter` opens **ServiceDetail** — one row per running replica across every host — with the same per-row container actions. `H` opens **ServiceHistory**: every yoink-managed container with `yoink.service=<name>` (running and exited), sorted newest-first by `yoink.deployed-at`. Pressing `r` on a row triggers a rollback confirmation pinned to that row's tag (same flow as `yoink rollback --tag <value>`).

## Resources pane (`R`)

Three sub-tabs covering the introspection lazydocker users expect, fanned out across every configured host:

| tab | columns | actions |
|---|---|---|
| **Images** (`i` inside Resources) | host · 12-char id · size · age · dangling · tags | `d` remove · `P` prune dangling · `A` prune all unused |
| **Volumes** (`v`) | host · name · driver · mountpoint | `d` remove · `P` prune unused |
| **Networks** (`n`) | host · name · driver · scope · internal | `d` remove · `P` prune unused |

`Tab` / `Shift-Tab` cycles between the three sub-tabs (instead of cycling the top-level modes — only inside Resources). `/` filters across host/name/tag substrings; partial fetch errors per host appear as a red footer ribbon rather than blanking the whole table. Dangling images sort to the top so they're trivial to prune.

There's intentionally **no volume file browsing** — drop into the container with `!` (or, for distroless containers, `D` for the debug sidecar) and use the shell. That's strictly more capable than the half-baked file UI lazydocker has, and it's what most operators reach for anyway.

## Secrets pane (`e`)

View / add / edit / remove individual sealed secrets without leaving the TUI. Reuses the same on-disk format as `yoink secrets edit` and respects per-environment `secrets.file:` paths — the title bar shows which file is active.

| key | action |
|---|---|
| `↑↓` / `j` `k` | select key |
| `r` | reveal/mask values |
| `/` | filter substring |
| `a` | add a new secret (age provider only) |
| `e` / `enter` | edit selected value |
| `d` | delete selected (with confirmation) |
| `Esc` / `q` | back |

When the provider is Infisical the pane is read-only — edits go via the Infisical web UI. When no age identity is available, the pane shows the failed-load reason + a remediation pointer. For bulk multi-line edits, drop to the CLI: `yoink secrets edit`.

## Pretty logs

Structured log lines (JSON, logfmt, etc.) are hard to scan as raw text. The TUI's logs pane auto-detects [`hl`](https://github.com/pamburus/hl) (`brew install pamburus/tap/hl`) on the operator's `PATH` and transparently pipes every container's log stream through it before rendering — so JSON keys are colored, timestamps are dim, levels are highlighted, and stack traces stay readable. Falls back to raw output when `hl` isn't installed; no config knob to toggle.

The `yoink logs <svc> -f` CLI doesn't auto-pipe (the operator decides their own shell pipeline), but `yoink logs api -f | hl` works the same way.

## Keybinding cheat sheet

A consistent set of letters has the same meaning everywhere they appear:

| key | meaning |
|---|---|
| `j` `k` / `↑` `↓` | navigate up / down |
| `enter` | drill in (logs from a list, modal-confirm from a dialog) |
| `esc` | back / dismiss / clear filter |
| `/` | begin filter input |
| `r` | refresh the current pane |
| `!` | shell into selection |
| `D` | debug sidecar |
| `i` | inspect (container detail) |
| `K` | kill (SIGKILL with confirmation) |
| `S` / `X` / `R` | start · stop · restart container |
| `U` | reconcile current service |
| `A` | reconcile all services |
| `P` | prune (containers from Dashboard; resources from Resources) |
| `H` | history (only on ServiceDetail) |
| `p` | docker top (only on ContainerDetail) |
| `y` | yank visible buffer to clipboard (logs / progress modals) |
| `?` | help overlay scoped to the current view |
| `q` / `Ctrl-C` | quit |

Letters are case-sensitive — capital letters generally mean "destructive or expensive" (kill, restart, reconcile-all, prune-all-images, …) and require a `y`/Enter confirmation when state-changing, while lowercase letters are read-only navigation / refreshes.
