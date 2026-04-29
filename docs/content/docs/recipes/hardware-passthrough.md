---
title: Hardware passthrough (`devices:`)
weight: 9
---

Pass host devices into a container — GPUs, USB-attached hubs, FUSE, audio, webcams, TPMs — using the same `<host>[:<container>[:<perms>]]` syntax as docker's `--device` flag, with the rest of yoink's hardened defaults intact. Narrower than `--privileged` (which yoink deliberately doesn't expose): only the listed devices become accessible.

## Shape

```yaml
services:
  - name: media
    image: jellyfin/jellyfin
    tag: "10.10"
    run:
      port: 8096
      options:
        devices:
          - /dev/dri              # all Intel/AMD GPU nodes
          - /dev/dri/card0:/dev/dri/card0:rw   # explicit perms
```

Each entry is bind-mounted into the container *and* added to the cgroup `devices.allow` list. Both are required: bind-mount alone hits `EPERM` on `open()` from the cgroup whitelist, allow alone is invisible. Yoink does both in one declaration.

`<perms>` is some combination of `r`, `w`, `m` — defaults to `rwm` when omitted. `m` is `mknod` (creating new device nodes inside the container, almost never needed).

## Common patterns

### GPU transcode (Plex / Jellyfin / Frigate / Ollama)

```yaml
- name: jellyfin
  image: jellyfin/jellyfin
  tag: "10.10"
  run:
    port: 8096
    options:
      user: "0:0"             # most media images need root for HW access
      devices:
        - /dev/dri            # Intel/AMD VAAPI
        # - /dev/nvidia0      # NVIDIA — see below
        # - /dev/nvidiactl
        # - /dev/nvidia-uvm
```

NVIDIA needs the `nvidia-container-runtime` on the host, plus the nodes above. Ollama / vLLM / whisper-server follow the same pattern.

### USB-attached hub (Zigbee / Z-Wave / serial)

```yaml
- name: zigbee2mqtt
  image: koenkk/zigbee2mqtt
  tag: latest
  run:
    options:
      user: "0:0"
      devices:
        - /dev/ttyUSB0:/dev/ttyACM0:rw   # remap if your stick reports a different node
```

Use `udev` rules on the host to give the device a stable `/dev/serial/by-id/...` path if `/dev/ttyUSB*` reorders across reboots.

### FUSE (e.g. JuiceFS, sshfs, restic mount)

```yaml
- name: juicefs
  image: juicedata/mount
  tag: latest
  run:
    options:
      user: "0:0"
      cap_add: [SYS_ADMIN]    # mount(2) needs the cap
      read_only: false        # FUSE userspace daemons write to /var/lib
      devices:
        - /dev/fuse
```

`cap_add: [SYS_ADMIN]` is the price of FUSE — userspace `mount(2)` requires it. The rest of the hardened defaults (no-new-privileges, cap_drop=ALL except SYS_ADMIN, non-root if the daemon supports it) still apply.

### Webcam (Frigate)

```yaml
- name: frigate
  image: ghcr.io/blakeblackshear/frigate
  tag: stable
  run:
    options:
      user: "0:0"
      devices:
        - /dev/video0
        - /dev/dri              # also use VAAPI for hw decode
```

### TPM / hardware crypto

```yaml
- name: vault
  image: hashicorp/vault
  run:
    options:
      cap_add: [IPC_LOCK]
      devices:
        - /dev/tpm0:/dev/tpm0:rw
```

## What `devices:` does NOT do

- **`--device-cgroup-rule`** (raw cgroup rules like `c 81:* rmw`) isn't exposed. If you need a glob over a major number, list each minor explicitly or open an issue with the use case.
- **`--gpus`** (NVIDIA's high-level runtime selector). Use the explicit `/dev/nvidia*` device list above plus the `nvidia-container-runtime` on the host. The selector flag is a runtime-specific shortcut yoink doesn't pass through.
- **`--privileged`** is deliberately not exposed at all. If you actually need full privilege (rare — usually means the workload is wrong for a container), run it via `docker run --privileged` outside yoink.
- **Default-deny narrowing.** `devices:` *adds* explicit accesses; it doesn't tighten the runtime's default device whitelist. On cgroup v2 hosts, the default policy varies — tighten with kernel/runtime config, not yoink.

## Validation

`yoink validate` rejects malformed entries up front so they don't blow up at deploy time:

```sh
$ yoink validate
yoink: invalid config: service "media".run.options.devices:
  invalid device spec "dev/fuse": expected
  "<host-path>[:<container-path>[:<perms>]]" where paths begin with `/`
  and `<perms>` is some non-empty combination of `r`, `w`, `m`
```

Caught: relative paths, empty perms (`...:`), unknown perm chars (e.g. `rwx`), duplicate chars (`rrw`), oversize perms (longer than 3).

## See also

- [Reference: `RunOptions`](/docs/reference/config#runoptions) — every per-service runtime knob.
- [Secure by default](/docs/guide/security-defaults) — what stays locked down when you opt into a device.
- Docker's [`--device`](https://docs.docker.com/engine/reference/commandline/run/#device) — the underlying primitive.
