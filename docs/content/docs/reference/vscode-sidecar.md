---
title: yoink vscode sidecar
description: The flags of yoink vscode, the properties of its sidecar container, and the filesystem view of the editor.
---

## Flags

| Flag | Default | Effect |
|---|---|---|
| `--host <addr>` | none | Selects the host. Required when the service runs on more than one host. |
| `-r, --replica <n>` | 0 | Accepted. yoink validates the range only. The sidecar attaches to the first running container of the service on the host. |
| `--no-open` | off | Prints the URL and opens no browser. |
| `--port <n>` | OS-assigned | Sets the local port on the laptop. |

## Sidecar lifecycle

Each run of `yoink vscode` starts a new container on the host:

| Property | Value |
|---|---|
| Image | `codercom/code-server:4.96.4` |
| Name | `yoink-vscode-<service>-<suffix>` |
| Labels | `yoink.managed=true`, `yoink.kind=vscode-sidecar`, `yoink.vscode.target_service=<service>` |
| User | `0:0` (root owns the `/proc/PID/root` of PID 1 in most images) |
| PID namespace | shared with the target (`pid_mode: container:<target>`) |
| Capabilities | adds `CAP_SYS_PTRACE` so that `/proc/PID/root` passes the `ptrace_may_access` gate of the kernel |
| Network | the first network in `networks:` of the service, or the first network of the running container (a separate net-ns so that docker can publish a host port) |
| Port | `127.0.0.1:<random>:8080`, tunneled through `ssh -L`. code-server runs with `--auth none`. |
| Cleanup | force-remove on Ctrl-C, with `auto_remove: true` as fallback |

Concurrent runs get unique names, so two operators can work on the same service at the same time.

If a custom seccomp or AppArmor profile on the host blocks `SYS_PTRACE`, the sidecar starts but every file read returns `EACCES`.

`yoink vscode` needs an SSH host. It stops with an error for a service on `address: local`.

## What the editor sees

code-server opens `/proc/1/root`, the kernel symlink to the rootfs of PID 1 of the target. Every read and write through that path operates on the filesystem as the target sees it:

- The image layers as the running container sees them.
- Bind mounts that the daemon made, for example the per-service config files of yoink.
- `tmpfs` mounts from `run.options.tmpfs:`.
- Volumes mounted into the container.

The view is the same as the output of `docker exec <target> ls /`.

The terminal in the code-server UI runs in the mount namespace of the sidecar. It shows the filesystem of the code-server image, not of the target.

## See also

- [Edit container files with `yoink vscode`](/docs/how-to/vscode): the steps to open the editor, and the fix for `EACCES` errors.
- [CLI reference](/docs/reference/cli): `yoink pf` and `yoink shell`, which use the same tunnel and namespace design.
- [Deploy modes](/docs/explanation/deploy-modes): how the target containers got on the host.
