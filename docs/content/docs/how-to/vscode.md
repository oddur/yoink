---
title: Edit container files with `yoink vscode`
description: Open VS Code in the browser rooted in a running container's live filesystem — no operator install, just SSH and a browser.
weight: 9
---

`docker exec` + `cat` + `vi` is the loop for inspecting or editing files in a running container. `yoink vscode <service>` replaces it with VS Code in your browser, rooted in the container's live filesystem. Saves land on the running container's files directly, with no copy-out, no rebuild, no redeploy.

```sh
yoink vscode api
```

First run pulls `codercom/code-server` (~700 MB) on the host; subsequent runs reuse the cached image and start in ~3 s. Ctrl-C closes the tunnel and force-removes the sidecar.

## What the editor sees

The sidecar joins the target's PID namespace and code-server is pointed at `/proc/1/root`, the kernel's magic symlink to PID 1's rootfs. Every read and write through that path operates on the target's view of the filesystem:

- The image layers as the running container sees them.
- Bind mounts the daemon set up (e.g. yoink's per-service config files).
- `tmpfs` mounts from `run.options.tmpfs:`.
- Volumes mounted into the container.

Same view as `docker exec <target> ls /`, with cmd-S instead of an in-container editor.

## Sidecar lifecycle

Each invocation spawns a fresh container on the host:

| Property | Value |
|---|---|
| Image | `codercom/code-server` (pinned tag) |
| Name | `yoink-vscode-<service>-<suffix>` |
| Labels | `yoink.managed=true`, `yoink.kind=vscode-sidecar`, `yoink.vscode.target_service=<service>` |
| User | `0:0` (PID 1's `/proc/PID/root` is owned by root in most images) |
| PID namespace | shared with the target (`pid_mode: container:<target>`) |
| Capabilities | adds `CAP_SYS_PTRACE` so `/proc/PID/root` passes the kernel's `ptrace_may_access` gate |
| Network | the target's first declared docker network (separate net-ns so docker can publish a host port) |
| Port | `127.0.0.1:<random>:8080`, tunneled via `ssh -L` |
| Cleanup | force-remove on Ctrl-C; `auto_remove: true` as fallback |

Concurrent invocations get unique names; two operators on the same service work side-by-side.

If the host enforces a custom seccomp/AppArmor profile that blocks `SYS_PTRACE`, the sidecar starts but every file read returns `EACCES`. The fix is host-side: add `SYS_PTRACE` to the daemon's allowlist or relax the per-container profile yoink applies (see `proxy:` / per-service `options.cap_add:`).

## In-browser terminal caveat

The terminal in code-server's UI runs in the **sidecar's** mount namespace, not the target's. `ls /` there shows the code-server image's filesystem; the file tree on the left shows the target's. Use `yoink shell <service>` for a shell rooted in the target.

## Read-only mode

There isn't one. code-server has no true read-only setting. Same write-foot-gun as `yoink shell`: don't truncate database files, don't `rm -rf` something the app holds an `fd` to. To inspect without risk, copy the file out with `docker cp` first.

## Flags

| Flag | Default | Effect |
|---|---|---|
| `--host <addr>` | first applicable | Pin to one host when the service runs on multiple. |
| `-r, --replica <n>` | 0 | Pick a replica when `replicas: > 1`. |
| `--no-open` | off | Don't launch a browser; print the URL and wait. |
| `--port <n>` | OS-assigned | Pin the laptop-side local port. |

## Requirements

- Target service running. `yoink vscode` finds it via the `yoink.service=<name>` label.
- Host SSH-able. `address: local` is rejected; there's no tunnel endpoint.
- Service declares a `networks:` entry. The sidecar joins it for in-terminal DNS.

## See also

- [`yoink pf`](/docs/reference/cli) — same SSH-tunnel + sidecar shape, for ports.
- [`yoink shell`](/docs/reference/cli) — exec into the target's PID namespace; the terminal `yoink vscode` doesn't give.
- [Deploy modes](/docs/guide/deploy-modes) — how the target containers got there.
