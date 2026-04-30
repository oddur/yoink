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
| **ContainerDetail** | k9s-style "describe": image, command, env (secrets redacted), ports, mounts, networks, security profile (cap_drop / read_only / pids_limit), restart count, three **5-minute history charts** (CPU%, Mem, mirrored net tx/rx), and a `NET ↓rx ↑tx` cumulative line in the card. `p` opens a `docker top` modal listing in-container processes. |
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
| `B` | debug sidecar (alpine in target's pid+net ns — for distroless / shell-less images) |
| `H` | service deploy history; on a stopped row press `r` to roll back |
| `~` | show drift detail for the focused service (image / tag / spec_hash / env keys / label keys) |
| `f` | port-forward the focused service. Auto-mode: published path when the service has a matching `publish:` entry, else spawns an ephemeral `alpine/socat` sidecar that joins the service's docker network. Footer band stays visible across panes until closed. |
| `o` / `O` | open the active port-forward URL in the system browser. Works in any view; falls back to the most-recently-opened tunnel when the focused row has no forward of its own. |
| `F` | close every active port-forward (sidecars are force-removed; ssh children killed) |
| `x` | toggle eXited containers visible in the table |
| `r` | refresh |
| `/` | filter substring (Esc clears) |

## Container detail (`i` from any list view)

The detail pane renders four kinds of information for a single container:

1. **Header card** — image, state (colored), command, restart count, started/finished/exit-code timestamps. The right column has live `CPU` / `MEM` gauges and a `NET ↓rx ↑tx` cumulative-bytes line ("how much has this thing transferred since start").
2. **5-minute history charts** — three side-by-side panels, each with the latest sampled value baked into its title so it's readable without squinting at the rightmost edge:
   - **CPU %** (cyan) — own y-axis scaled to peak CPU
   - **Mem** (magenta) — own y-axis. % when memory is capped, MB when uncapped
   - **net tx / rx** (yellow / green) — btop-style mirrored: `tx` (outgoing) plots above the zero line, `rx` (incoming) mirrored below. The two series can never overlap. Title shows current rates: `↑1.2MB/s ↓340KB/s`. Y-axis labels carry the direction arrow on each side.
3. **Runtime + security blocks** — published ports, mounts, attached networks, then `cap_drop` / `cap_add` / `security_opt` / `read_only` / `pids_limit` / effective `user` so you can see at a glance whether this container is hardened.
4. **Env + labels** — sorted KEY=value with secret-ish keys (`*TOKEN*`, `*SECRET*`, `*PASSWORD*`, `*API_KEY*`, `*PRIVATE_KEY*`, `*DSN*`) auto-redacted; full label table including the `yoink.*` set.

Plus, at the bottom of the pane, a rolling tail of the container's logs.

**History is collected for every container, all the time** — a background poller samples `docker stats` for every running container across every configured host every 2 seconds, regardless of which view you're currently on. So when you drill into a container's detail pane, the chart is already populated with up to 5 minutes of context instead of starting from zero. Stale entries (containers that stopped and aged out) are GC'd automatically.

| key | action |
|---|---|
| `enter` / `l` | open dedicated logs view |
| `!` | exec a shell inside (`bash` → `sh`) |
| `B` | debug sidecar (alpine sharing pid+net ns) |
| `S` / `X` / `R` | start / stop / restart |
| `K` | SIGKILL (with confirmation) |
| `U` | reconcile this service |
| `p` | docker top — shows in-container processes in a modal (Esc to close) |
| `r` | refresh |
| `esc` | back to host detail |

If your image is distroless or otherwise has no shell, `!` will fail. **Fall back to `B`** — the debug sidecar attaches an alpine container sharing the target's PID and network namespaces, so you can run `ps`, `ss`, `cat /proc/<pid>/...` against the target without modifying the production image. The sidecar auto-removes when you `exit` / Ctrl-D.

## Drift detail (`~`)

When the dashboard / host detail / service detail / container detail view shows ⚠ on a row, press **`~`** to open a modal that explains *what* drifted — the same per-field diff `yoink up --plan` produces on the CLI side:

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

Color-coded so the markers read at a glance: `+` green (added in desired), `-` red (removed from running), `~` yellow (changed). The hash and image columns highlight the running → desired transition in cyan when they differ; the line dims to `(unchanged)` when they match.

The fetch reuses `diff::compute` under the hood, so the modal's content is identical to `yoink up --plan --service <name>` against the same host. For services without a `tag:` pinned in config (typical for `image: ghcr.io/you/api` where CI provides the tag), the modal falls back to the running replica's tag so the diff isolates the env/label change instead of erroring on a missing tag — useful for "what changed since deploy?" without leaving the TUI.

`Esc` closes the modal. The underlying view stays put.

## Hosts pane (`h`)

Per-host summary table:

| col | meaning |
|---|---|
| `host` | `user@address` from `yoink.yaml` |
| `status` | ssh probe + Docker API handshake — `ok` / `unreachable` (with classified hint for Tailscale auth, `ssh-add` reminders, etc.) |
| `daemon` | docker server version, OS / kernel |
| `cpu`, `mem` | aggregate across all running containers (sum of `docker stats`) — gauge-coloured |

`Enter` drills into **HostDetail**, which shows every running container on the host (whether yoink manages it or not), with the same live CPU/mem cells, drift indicator, and per-container actions:

| key | action |
|---|---|
| `↑↓` / `j` `k` | select container |
| `enter` | live logs |
| `i` | container detail (env, mounts, security, history charts, …) |
| `!` | shell · `B` debug sidecar |
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

There's intentionally **no volume file browsing** — drop into the container with `!` (or, for distroless containers, `B` for the debug sidecar) and use the shell. That's strictly more capable than the half-baked file UI lazydocker has, and it's what most operators reach for anyway.

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

When `provider: command` is configured the pane is read-only — rotation happens in whichever external tool the configured CLI talks to. When no age identity is available, the pane shows the failed-load reason + a remediation pointer. For bulk multi-line edits, drop to the CLI: `yoink secrets edit`.

## Logs pane (`l`)

The multiplexed Logs view aggregates a live tail from **every yoink-managed container** across every host into one scrollable buffer. Each line is prefixed with the container name; lines from different hosts and services interleave in real time. Useful for "something just happened on prod, what was it?" when you don't yet know which service.

| key | action |
|---|---|
| `↑↓` / `PgUp` `PgDn` | scroll line / page (auto-follow disengages while scrolling away from bottom) |
| `g` / `G` / `End` | jump to top / bottom (resumes auto-follow) |
| `/` | begin filter input — Enter applies, Esc cancels (live-typed, case-insensitive substring) |
| `c` | clear the buffer (next ticks repopulate) |
| `r` | restart streams (re-opens log pipes if any died) |
| `y` | yank the visible buffer to the system clipboard via OSC-52 |

The buffer is bounded at 5,000 lines; older lines fall off as new ones arrive. Filter doesn't shrink the buffer, only the rendered view.

### `ContainerLogs` (single container)

Reached via `Enter` from any list view. Same shape as the multiplexed Logs pane, scoped to one container. Same keybindings; `Esc` returns to the parent. `!` and `B` are also bound here for quick "tail logs → drop into shell" pivots.

## Shell / debug sidecar (`!`, `B`)

Both gestures put you on a PTY inside the host's docker daemon, no SSH on top — yoink uses the Docker exec API and the TUI streams bytes both ways through a terminal-emulator parser.

`!` runs `bash` (falls back to `sh`) inside the existing container — equivalent to `yoink shell <service>` but staying in the TUI. Useful when the image has a shell and you want quick access to the running process's filesystem, env, etc.

`B` spins up an ephemeral **alpine debug sidecar** sharing the target container's PID and network namespaces. The fallback for distroless / scratch / shell-less images: you get `ps`, `ss`, `cat /proc/<pid>/...`, `tcpdump`, `apk add` whatever you need — without modifying the production image. The sidecar is `--rm` and force-removed when you `exit` / Ctrl-D, even if the TUI crashes.

| key inside the shell view | action |
|---|---|
| anything | forwarded into the in-shell process (Ctrl-C, Ctrl-D, arrow keys, …) |
| `Ctrl-Q` | exit shell, back to the parent pane (yoink-side gesture) |
| `?` | toggle help overlay (one yoink-side gesture even inside the shell) |
| `exit` / `Ctrl-D` | end the in-container shell normally |

Window resizing flows through automatically — the panel size is sent to the daemon on every render so `top` / `vim` / etc. re-flow.

## Progress modals

Long-running operations (reconcile-one, reconcile-all, prune) render a centred modal that streams the deploy event log live. The border colour reflects state — cyan while running, green on success, red on failure. The modal eats every key while the operation is in flight (so a stray `j` can't drive the underlying view); `y` yanks the modal's text to the clipboard at any time. Once finished, `Esc` / `Enter` dismisses.

`reconcile-all` (`A` from Dashboard) gets a richer status table at the top of the modal — one row per service, colour-coded by current state (waiting → pulling → healthcheck → swapping → done / failed) — so when the wave-parallel deploy is mid-flight you can see all six services' progress at a glance instead of hunting through interleaved log lines.

## Filter conventions

Every list/table pane has a consistent filter:

- `/` enters input mode — type freeform; `Backspace` deletes; `Enter` applies; `Esc` cancels (drops back to whatever filter was already active)
- Active filter is shown in cyan in the footer (`filter: foo`); editable filter buffer is yellow
- `Esc` with no input mode and an active filter clears it

Filter is case-insensitive substring across multiple fields per pane (host + service + container name + state + version + networks for the dashboard; analogous sets elsewhere). Empty filter shows everything.

## Help overlay (`?`)

Toggles a centred per-view modal listing every key binding active in the current view. Press `?` again or `Esc` to close. The contents are scoped — Dashboard's overlay shows only Dashboard keys, Resources' shows only Resources keys, etc. — so you don't have to scan irrelevant bindings.

The overlay is available in every view including inside the embedded shell (which otherwise forwards every key to the PTY).

## CLI launch flags

```sh
yoink tui                       # Dashboard view by default
yoink tui --mode hosts          # start on a specific top-level pane
yoink tui --mouse               # enable mouse capture (scroll wheel + selection)
```

`--mode` accepts `dashboard` / `hosts` / `services` / `logs` / `resources` / `secrets`. `--mouse` is opt-in because mouse capture disables your terminal's native text-selection — if you don't actively use mouse scroll inside the TUI, leave it off.

`YOINK_NO_HL=1` in the environment skips the `hl` auto-detection (the logs pane will use raw output even if `hl` is on PATH). Useful when troubleshooting `hl` itself.

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
| `B` | debug sidecar |
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

## See also

- [CLI reference](/docs/reference/cli) — the same commands the TUI binds to keys, in scriptable form.
- [Driving yoink from an AI agent](/docs/guide/ai-agents) — the CLI surface the TUI mirrors, suitable for non-interactive driving.
- [Troubleshooting](/docs/troubleshooting) — when the TUI shows something surprising.
