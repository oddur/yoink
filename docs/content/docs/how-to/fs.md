---
title: Edit container files with `yoink fs`
weight: 9
---

`docker exec` + `cat` + `vi` is the usual loop for inspecting or editing files in a running container. `yoink fs <service>` replaces it with VS Code in your browser, rooted in the container's live filesystem. Saves propagate immediately — the editor is reading and writing the running container's files, not a copy.

```sh
yoink fs api
```

The first run pulls `codercom/code-server` (~700 MB) on the host; subsequent runs reuse the cached image and start in ~3 s. Ctrl-C closes the tunnel and force-removes the sidecar.

## What's mounted

The sidecar joins the target container's PID namespace and serves `/proc/1/root` — the magic symlink to the target's PID 1's rootfs. Every read and write through the editor lands on the target's view of the filesystem, including:

- The image layers as the running container sees them.
- Bind mounts the daemon set up (e.g. yoink's per-service config files).
- `tmpfs` mounts from `run.tmpfs:`.
- Volumes mounted into the container.

This is exactly what `docker exec <target> cat /path/to/file` would show, with `cmd-S` instead of an editor inside the container.

## Sidecar lifecycle

Each `yoink fs` invocation spawns a fresh container on the host:

| Property | Value |
|---|---|
| Image | `codercom/code-server` (pinned tag) |
| Name | `yoink-fs-<service>-<suffix>` |
| Labels | `yoink.managed=true`, `yoink.kind=fs-sidecar`, `yoink.fs.target_service=<service>` |
| PID namespace | shared with the target |
| Network | the target's first declared docker network (so the in-browser terminal can resolve sister-service DNS) |
| Port | `127.0.0.1:<random>:8080`, tunneled to the laptop via `ssh -L` |
| Cleanup | force-remove on Ctrl-C; `auto_remove: true` as fallback |

Concurrent invocations get unique names, so two operators on the same service work side-by-side.

## In-browser terminal caveat

code-server's built-in terminal runs in the **sidecar's** mount namespace, not the target's — `ls /` there shows the code-server image's filesystem. The file tree on the left is the target's; the terminal isn't. For a shell rooted in the target, use `yoink shell <service>`.

## Read-only mode

There isn't one. code-server doesn't expose a true read-only setting. Treat `yoink fs` the same as `yoink shell`: don't truncate database files, don't `rm -rf` something the app's holding an `fd` to. If you want a snapshot to inspect without risk, copy the file out with `docker cp` first.

## Flags

| Flag | Default | Effect |
|---|---|---|
| `--host <addr>` | first applicable | Pin to one host when the service runs on multiple. |
| `-r, --replica <n>` | 0 | Pick a specific replica when `replicas: > 1`. |
| `--no-open` | off | Don't launch a browser; print the URL and wait. |
| `--port <n>` | OS-assigned | Pin the laptop-side local port. |

## Requirements

- The target service must be running. `yoink fs` finds it via the `yoink.service=<name>` label.
- The host must be SSH-able (`address: local` is rejected — there's no tunnel endpoint).
- The service must declare a `networks:` entry. The sidecar joins it for in-terminal DNS.

## See also

- [`yoink pf`](/docs/reference/cli) — same SSH-tunnel + sidecar shape, for ports.
- [`yoink shell`](/docs/reference/cli) — exec into the target's PID namespace; the terminal `yoink fs` doesn't give you.
- [Deploy modes](/docs/guide/deploy-modes) — how the target containers get there in the first place.
