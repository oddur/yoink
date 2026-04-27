---
title: TUI
weight: 3
---

```sh
yoink tui
```

Heavily inspired by [k9s](https://k9scli.io/). Keyboard-driven panes for dashboard (drift across all services), hosts (per-host detail with live CPU/mem), services (per-service detail with replica info), container logs (live tail with `/` filter), per-container detail (env, mounts, healthcheck, networks, security profile), service deploy history.

![yoink TUI dashboard](https://github.com/user-attachments/assets/bcf956b1-937a-43d7-92f6-0f24a1d0cd98)

![yoink TUI container detail](https://github.com/user-attachments/assets/36d3cc00-355c-482b-addc-454a0070b58e)

## Top-level modes

| key | mode |
|---|---|
| `d` | Dashboard |
| `h` | Hosts |
| `s` | Services |
| `l` | Logs |
| `e` | Encrypted-secrets |
| `Tab` / `Shift-Tab` | cycle modes forward / backward |

## Operator gestures from the dashboard

| key | action |
|---|---|
| `↑` / `↓` / `j` / `k` | navigate |
| `enter` | drill into selected row |
| `i` | container inspect (security & limits, env, mounts, networks) |
| `K` | kill (with confirmation) |
| `U` | reconcile this service |
| `A` | reconcile all services |
| `P` | prune |
| `!` | shell into container |
| `D` | debug sidecar (alpine in target's pid+net ns) |
| `H` | service deploy history; on a stopped row press `r` to roll back |
| `x` | toggle eXited containers visible in the table |
| `r` | refresh |
| `/` | filter substring (Esc clears) |
| `?` | help overlay |
| `q` | quit |

## Secrets pane (`e`)

View / add / edit / remove individual sealed secrets without leaving the TUI. Reuses the same on-disk format as `yoink secrets edit` and respects per-environment `secrets.file:` paths — the title bar shows which file is active.

| key | action |
|---|---|
| `↑` / `↓` / `j` / `k` | select key |
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
