---
title: Mount a container's filesystem
weight: 12
---

Inspecting files inside a running container usually means `docker cp` round-trips or shell-execs that drop into a spartan environment without your editor or tools. `yoink fs <service>` mounts the container's root filesystem on your laptop via SSH+FUSE — `cd /tmp/yoink-fs/api/`, `tail -f var/log/app.log`, open the dir in your editor, ctrl-c to unmount.

Same shape as `yoink pf` but for files. Replica selection, host filtering, and ssh-credential reuse all match.

## Quickstart

```sh
yoink fs api                     # mounts at /tmp/yoink-fs/api/ (read-only)
ls /tmp/yoink-fs/api/etc/        # operator can read the container's /etc
cat /tmp/yoink-fs/api/var/log/app.log
^C                               # unmount
```

Multi-host or multi-replica fleet:

```sh
yoink fs api --host prod-2       # pin to one host
yoink fs api -r 1 --to ./fs      # replica 1, custom mountpoint
```

## Prerequisites

One operator-side install on macOS:

```sh
brew install --cask fuse-t-sshfs
```

This is the kext-free path — pulls [FUSE-T](https://www.fuse-t.org) as a dependency, no kernel extensions, no reboot. `yoink doctor` flags this when missing. On Linux, `apt install sshfs` (or your distro's equivalent).

Two host-side conditions, both true for any standard yoink host:

- **`overlay2` storage driver.** Yoink reads `GraphDriver.Data.MergedDir` for the host-side path that mirrors the container's rootfs. Other drivers (`btrfs`, `zfs`, `vfs`) don't expose a single host path; `yoink fs` errors out on them.
- **The deploy user in the `docker` group.** `MergedDir` lives under `/var/lib/docker/overlay2/` and the docker group has read access. Required for any yoink host already.

## Read-only by default

Writes through the merged view mutate the running container's filesystem and race with the app's own writes — file locks, log rotation, mid-write truncation. The `--rw` flag opts in:

```sh
yoink fs api --rw
⚠ mounting api-aaa read-write — writes can race with the running app's writes
  (file locks, truncation, log rotation). Ctrl-C to abort.
→ api (replica 0) on host-1 ⇆ /tmp/yoink-fs/api [rw]
```

For drop-a-config / poke-a-flag / quick patches, `--rw` is the affordance. For app-state writes (database files, in-flight uploads), prefer `docker exec` or app-aware tooling — FUSE write semantics differ from kernel writes in ways that surprise databases and lock managers.

## Schema

| Flag | Type | Default | Notes |
|---|---|---|---|
| `service` (positional) | string | required | Service name as declared in `yoink.yaml`. |
| `--host` | string | — | Pin to one host when the service runs on multiple. |
| `-r`, `--replica` | int | `0` | Replica index for `replicas: > 1`. |
| `--to` | path | `/tmp/yoink-fs/<service>/` | Mountpoint on your laptop. Created if missing; rejected if non-empty. |
| `--rw` | bool | `false` (read-only) | Mount read-write. Prints a warning before mounting. |

## What you see in the mount

The merged view layers the container's read-write top onto the image's read-only lower layers, so `/tmp/yoink-fs/api/` looks like the container's `/`:

```
/tmp/yoink-fs/api/
├── bin/             # from the image
├── etc/             # image + container overrides
├── var/log/app.log  # written by the running container
└── …
```

Two things don't show through:

- **`tmpfs` mounts.** `/run`, `/dev/shm`, anything declared with `tmpfs:` in yoink. The merged view is filesystem-only; tmpfs entries live in the container's mount namespace and aren't reachable from the host.
- **Symlinks pointing into bind-mounted paths.** `/etc/resolv.conf -> /run/...` resolves inside the container against a bind mount that exists only there. From the host's merged view, the symlink dangles.

For both, `docker exec <container> sh` is the right tool.

## Stale mount recovery

A laptop close, network drop, or `kill -9` mid-session leaves the FUSE mount in a "transport endpoint not connected" state. Running `yoink fs` again detects the stale mountpoint and reaps it before remounting. Manual cleanup if you need it:

```sh
# Linux
fusermount -u /tmp/yoink-fs/api

# macOS
umount /tmp/yoink-fs/api
```

## File ownership

Container UIDs rarely match your laptop's UID, so without remapping, files show as `nobody:nogroup`. Yoink passes `-o idmap=user` to sshfs, which remaps the SSH-side user (the host's deploy user) to your local UID for display. The actual on-disk ownership inside the container is unchanged.

If a file shows as a different non-deploy UID inside the container — say, an app running as UID 1000 — you'll see that UID locally rather than your own. `chown` it inside the container if you need to write through the mount.

## See also

- [`yoink pf`](/docs/guide/networking#port-forward) — same shape as `yoink fs` but for TCP ports.
- [Reverse proxy](/docs/guide/proxy) — the `yoink-proxy` service is itself mountable for inspecting Caddy's autosaved config.
