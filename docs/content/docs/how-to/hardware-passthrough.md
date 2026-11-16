---
title: Pass host devices into a container
description: Give one service access to a GPU, a USB device, FUSE, a webcam, or a TPM, and keep the other hardened defaults.
---

## Prerequisites

- The device node exists on the host, for example `/dev/dri` or `/dev/ttyUSB0`.
- You know the [`devices:` entry syntax](/docs/reference/security-defaults#devices-entry-syntax): `<host>[:<container>[:<perms>]]`.

## Add a device to a service

### Add the `devices:` entries [step]

List each device under `run.options.devices`. If the service needs specific permissions, add them to the entry.

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

### Make sure that the config is valid [step]

```sh
yoink validate
```

`yoink validate` rejects malformed entries. [Security defaults](/docs/reference/security-defaults#validation) shows the error and the rejected entries.

### Deploy [step]

Run `yoink up`. Make sure that the app in the container can open the device.

## GPU transcode (Plex / Jellyfin / Frigate / Ollama)

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

For NVIDIA, install the `nvidia-container-runtime` on the host and list the `/dev/nvidia*` nodes. yoink does not pass the `--gpus` flag through. Use the same pattern for Ollama, vLLM, and whisper-server.

## USB-attached hub (Zigbee / Z-Wave / serial)

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

If the order of `/dev/ttyUSB*` changes after a reboot, add `udev` rules on the host. The rules give the device a stable `/dev/serial/by-id/...` path.

## FUSE (JuiceFS, sshfs, restic mount)

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

The other hardened defaults stay in effect: no-new-privileges, cap_drop=ALL except SYS_ADMIN, and non-root if the daemon supports it.

## Webcam (Frigate)

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

## TPM / hardware crypto

```yaml
- name: vault
  image: hashicorp/vault
  run:
    options:
      cap_add: [IPC_LOCK]
      devices:
        - /dev/tpm0:/dev/tpm0:rw
```

## See also

- [Security defaults](/docs/reference/security-defaults#devices-entry-syntax): the entry syntax, the docker flags that `devices:` does not replace, and the validation rules.
- [Security](/docs/explanation/security#hardware-passthrough-devices): why yoink has `devices:` and no privileged mode.
- [Relax a hardened default for one service](/docs/how-to/relax-security-defaults): the other opt-outs that device workloads often need.
- [Volume backups](/docs/how-to/volume-backups): a task that uses a FUSE mount.
