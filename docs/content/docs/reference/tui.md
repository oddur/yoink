---
title: TUI
weight: 7
---

```sh
yoink tui
```

Heavily inspired by [k9s](https://k9scli.io/). Keyboard-driven panes for dashboard (drift across all services), hosts (per-host detail with live CPU/mem), services (per-service detail with replica info), container logs (live tail with `/` filter), per-container detail (env, mounts, healthcheck, networks, security profile), service deploy history.

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
| `?` | help overlay |
| `q` | quit |

## Pretty logs

Structured log lines (JSON, logfmt, etc.) are hard to scan as raw text. The TUI's logs pane auto-detects [`hl`](https://github.com/pamburus/hl) (`brew install pamburus/tap/hl`) on the operator's `PATH` and transparently pipes every container's log stream through it before rendering — so JSON keys are colored, timestamps are dim, levels are highlighted, and stack traces stay readable. Falls back to raw output when `hl` isn't installed; no config knob to toggle.

The `yoink logs <svc> -f` CLI doesn't auto-pipe (the operator decides their own shell pipeline), but `yoink logs api -f | hl` works the same way.
