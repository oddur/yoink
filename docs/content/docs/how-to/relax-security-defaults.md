---
title: Relax a hardened default for one service
description: Find the hardened default that stops an image, and override only that field for only that service.
---

## Prerequisites

- A service in `yoink.yaml` that fails with the hardened defaults.
- The [Security defaults](/docs/reference/security-defaults) table, for the opt-out of each field.

## Find the default to override

### Deploy with the defaults [step]

Deploy the service with no overrides in `run.options`.

### Read the logs [step]

```sh
yoink logs <svc>
```

A container that cannot run with a default stops quickly.

### Override one field [step]

If the logs show a "permission denied" failure, override only the field that causes it. If the app works, do not add an override.

### Deploy again [step]

Run `yoink up`. Make sure that the container starts and that its healthcheck passes.

## Add one capability again

Caddy must bind `:80` and `:443`. Add only `NET_BIND_SERVICE`.

```yaml
# caddy needs to bind :80/:443 — re-add NET_BIND_SERVICE only.
- name: caddy
  run:
    options:
      cap_drop: [ALL]
      cap_add:  [NET_BIND_SERVICE]
      cpus: "0.5"
      memory: "64Mi"
```

## Make the rootfs writable

The image of pgadmin writes to its rootfs at startup. Set only `read_only: false`.

```yaml
# pgadmin's image scribbles all over its rootfs at startup — back out
# of read_only, keep the rest of the hardened defaults.
- name: pgadmin
  run:
    options:
      read_only: false
      cpus: "1"
      memory: "256Mi"
```

## Give a read-only service scratch space

If the app only needs `/tmp`, `/run`, or a cache directory, keep `read_only: true`. Add a `tmpfs:` entry for each directory. The data in a `tmpfs:` does not persist.

```yaml
run:
  options:
    read_only: true
    tmpfs:
      /tmp: "size=64m,mode=1777"
      /var/cache/app: "size=128m,mode=0755"
```

## Run an image as root with a writable `/etc`

Use this config only for an image with legacy code that needs root and a writable `/etc`.

```yaml
# Image with stubborn legacy code that needs root + writable /etc.
- name: legacy-thing
  run:
    options:
      user: "0:0"
      read_only: false
      cap_drop: []
      security_opt: []
```

## Add a host device

List only the device nodes that the service needs. This is narrower than privileged mode.

```yaml
# Hardware passthrough — opt into specific device nodes only,
# not the whole privileged blast radius. See below.
- name: jellyfin
  run:
    options:
      user: "0:0"        # most media images need root for HW access
      devices:
        - /dev/dri       # Intel/AMD VAAPI — read+write+mknod default
```

[Pass host devices into a container](/docs/how-to/hardware-passthrough) has the config for the other device types.

## Make sure that the override is in effect

In the TUI, press `i` on the container row. The "security & limits" section shows the effective security profile of the container.

## See also

- [Security defaults](/docs/reference/security-defaults): the default and the opt-out of each field.
- [Security](/docs/explanation/security): why each default exists.
- [Pass host devices into a container](/docs/how-to/hardware-passthrough): GPUs, USB, FUSE, webcams, and TPMs.
- [TUI reference](/docs/reference/tui): the container detail view.
