---
title: Security
description: Hardened container defaults — cap_drop=ALL, read-only rootfs, no-new-privileges — and the escape hatches for GPUs, FUSE, and USB.
weight: 3
---

Compose's defaults are dev-friendly, not prod-friendly: every container runs with the full Linux capability set, a writable rootfs, no `pids` cgroup limit, no protection against setuid escalation. Yoink flips those; every service yoink creates is hardened by default, and the only knobs that opt _into_ less safety are explicit. This page is the full surface: the defaults, the surgical opt-outs, and the device-passthrough escape hatch.

## Defaults

| field | yoink default | what it gives you | how to opt out |
|---|---|---|---|
| `cap_drop` | `["ALL"]` | every Linux capability dropped (no `CAP_NET_ADMIN`, `CAP_SYS_ADMIN`, …) | `cap_drop: []` (full default cap set), or surgically re-add via `cap_add` |
| `security_opt` | `["no-new-privileges:true"]` | setuid binaries inside the container can't escalate via `execve` | `security_opt: []` |
| `read_only` | `true` | rootfs mounted read-only; exploits can't drop binaries on disk | `read_only: false` |
| `user` | `"65534:65534"` (nobody) | container runs as a non-root unprivileged uid; a process breakout doesn't immediately give the attacker write access through tmpfs / bind mounts | `user: "0:0"` for images that need root (otel reading `/proc`), or any other `"uid:gid"` |
| `tmpfs` mount opts | auto-`noexec,nosuid,nodev` | a writable scratch tmpfs can't be used to drop + run a binary, set setuid bits, or create device nodes | include explicit `exec`/`suid`/`dev` in your tmpfs option string |
| `binds` mode | `:ro` when no mode set | accidental bind-mount-and-write to host paths is impossible without explicit `:rw` | `"/host:/container:rw"` (explicit) |
| `init` | `true` | tini as PID 1 reaps zombies + forwards SIGTERM, so drains and rolling swaps actually finish | `init: false` for images that ship their own init (systemd-in-containers, s6, supervisord) |
| `pids_limit` | `1024` | bounds the fork-bomb class of exploits | `pids_limit: null` (unlimited), or any positive int |
| `cpus` | `null` (uncapped) | — | set to `"2"`, `"500m"`, `"1.5"`, etc. (k8s style) |
| `memory` | `null` (uncapped) | — | set to `"512Mi"`, `"1Gi"`, `"512m"`, `"1g"` (k8s and docker styles both accepted) |
| `devices` | `[]` | only the kernel-whitelisted set is visible; host hardware (GPUs, USB hubs, `/dev/fuse`, …) is unreachable | list explicit `--device`-style entries; see [Hardware passthrough](#hardware-passthrough-devices) below. Narrower than `--privileged` (which yoink doesn't expose at all). |

## Surgical opt-outs in practice

```yaml
# caddy needs to bind :80/:443 — re-add NET_BIND_SERVICE only.
- name: caddy
  run:
    options:
      cap_drop: [ALL]
      cap_add:  [NET_BIND_SERVICE]
      cpus: "0.5"
      memory: "64Mi"

# pgadmin's image scribbles all over its rootfs at startup — back out
# of read_only, keep the rest of the hardened defaults.
- name: pgadmin
  run:
    options:
      read_only: false
      cpus: "1"
      memory: "256Mi"

# Image with stubborn legacy code that needs root + writable /etc.
- name: legacy-thing
  run:
    options:
      user: "0:0"
      read_only: false
      cap_drop: []
      security_opt: []

# Hardware passthrough — opt into specific device nodes only,
# not the whole privileged blast radius. See below.
- name: jellyfin
  run:
    options:
      user: "0:0"        # most media images need root for HW access
      devices:
        - /dev/dri       # Intel/AMD VAAPI — read+write+mknod default
```

## Non-root in practice

`user: "65534:65534"` works out of the box for the vast majority of images; alpine, debian-slim, ubuntu, gcr.io distroless variants all ship `nobody:nogroup` at that uid pair. The cases where the operator has to override are predictable and few:

| image / use case | override | why |
|---|---|---|
| `otel/opentelemetry-collector-contrib` reading `/proc` for host metrics | `user: "0:0"` | needs CAP_DAC_READ_SEARCH (and root) to traverse `/proc/<pid>` |
| `redis` (the official image) | `user: redis` | image ships its own non-root account; we just point at it |
| Images with pre-baked file ownership for a specific uid | `user: "<uid>:<gid>"` | volume / file-perm collision otherwise |
| Migration container that needs to chown a fresh volume | `user: "0:0"` (one-shot pre-deploy hook) | initial setup, fine to drop privilege after |

When in doubt: deploy with the default first, watch the container fail-fast (`yoink logs <svc>`), and override only when the failure mode is "permission denied" rather than "function works fine".

## Read-only rootfs in practice

The app still needs *somewhere* to write: `/tmp`, sometimes `/run`, occasionally a cache directory. Pair `read_only: true` with `tmpfs:` to give it scratch space without letting it persist:

```yaml
run:
  options:
    read_only: true
    tmpfs:
      /tmp: "size=64m,mode=1777"
      /var/cache/app: "size=128m,mode=0755"
```

## No host port binds by default

Yoink doesn't add `publish:` entries for you. A service without a `publish:` block is reachable from the docker network it's on (other yoink-managed services dial it by alias) and through the bundled Caddy proxy when it has a `domain:`, but **never directly bound to a host port**. That's the right default for everything except operator UIs that genuinely need to be reachable on the host's loopback (pgadmin, monitoring dashboards behind tailnet) and the proxy itself.

The standard pressure to weaken that default is debugging: "how do I curl this from my laptop?" The answer is [`yoink pf <service>`](/docs/guide/networking#port-forward), which tunnels to any container (published or not) over the same SSH the deploy uses. Debugging never requires changing what's exposed in production; the locked-down default stays the only default.

## Hardware passthrough (`devices:`)

Pass host devices into a container (GPUs, USB-attached hubs, FUSE, audio, webcams, TPMs) using the same `<host>[:<container>[:<perms>]]` syntax as docker's `--device` flag, with the rest of yoink's hardened defaults intact. Narrower than `--privileged` (which yoink deliberately doesn't expose): only the listed devices become accessible.

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

`<perms>` is some combination of `r`, `w`, `m`, defaulting to `rwm` when omitted. `m` is `mknod` (creating new device nodes inside the container, almost never needed).

### Common patterns

#### GPU transcode (Plex / Jellyfin / Frigate / Ollama)

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

#### USB-attached hub (Zigbee / Z-Wave / serial)

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

#### FUSE (e.g. JuiceFS, sshfs, restic mount)

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

`cap_add: [SYS_ADMIN]` is the price of FUSE; userspace `mount(2)` requires it. The rest of the hardened defaults (no-new-privileges, cap_drop=ALL except SYS_ADMIN, non-root if the daemon supports it) still apply.

#### Webcam (Frigate)

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

#### TPM / hardware crypto

```yaml
- name: vault
  image: hashicorp/vault
  run:
    options:
      cap_add: [IPC_LOCK]
      devices:
        - /dev/tpm0:/dev/tpm0:rw
```

### What `devices:` does NOT do

- **`--device-cgroup-rule`** (raw cgroup rules like `c 81:* rmw`) isn't exposed. If you need a glob over a major number, list each minor explicitly or open an issue with the use case.
- **`--gpus`** (NVIDIA's high-level runtime selector). Use the explicit `/dev/nvidia*` device list above plus the `nvidia-container-runtime` on the host. The selector flag is a runtime-specific shortcut yoink doesn't pass through.
- **`--privileged`** is deliberately not exposed at all. If you actually need full privilege (rare, and usually means the workload is wrong for a container), run it via `docker run --privileged` outside yoink.
- **Default-deny narrowing.** `devices:` *adds* explicit accesses; it doesn't tighten the runtime's default device whitelist. On cgroup v2 hosts, the default policy varies; tighten with kernel/runtime config, not yoink.

### Validation

`yoink validate` rejects malformed entries up front so they don't blow up at deploy time:

```sh
$ yoink validate
yoink: invalid config: service "media".run.options.devices:
  invalid device spec "dev/fuse": expected
  "<host-path>[:<container-path>[:<perms>]]" where paths begin with `/`
  and `<perms>` is some non-empty combination of `r`, `w`, `m`
```

Caught: relative paths, empty perms (`...:`), unknown perm chars (e.g. `rwx`), duplicate chars (`rrw`), oversize perms (longer than 3).

## What yoink doesn't do automatically (for now)

Seccomp/AppArmor profiles beyond docker's defaults, user-namespace remapping (`--userns-remap`), gVisor / Kata runtime selection. Those are host-wide concerns, not per-service config. Set them on the docker daemon and yoink containers inherit.

The TUI's container detail (`i` from any container row) shows the effective security profile for each running container under "security & limits," easy to spot when something's accidentally lax.

## See also

- [Reference: `RunOptions`](/docs/reference/config#runoptions) — every per-service runtime knob.
- [Networking](/docs/guide/networking) — the no-publish default and how `yoink pf` makes it practical.
- Docker's [`--device`](https://docs.docker.com/engine/reference/commandline/run/#device) — the underlying primitive.
