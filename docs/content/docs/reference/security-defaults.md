---
title: Security defaults
description: The default value, the effect, and the opt-out of each hardened run option, with the devices syntax and its validation rules.
---

All fields on this page are keys of `run.options` on a service, except `binds`, which is a key of `run`.

## Defaults

| field | yoink default | effect | opt-out |
|---|---|---|---|
| `cap_drop` | `["ALL"]` | Drops every Linux capability (no `CAP_NET_ADMIN`, `CAP_SYS_ADMIN`, …). | `cap_drop: []` (the full default set of capabilities), or add single capabilities again with `cap_add`. |
| `security_opt` | `["no-new-privileges:true"]` | setuid binaries in the container cannot escalate through `execve`. | `security_opt: []` |
| `read_only` | `true` | Mounts the rootfs read-only. Exploits cannot write binaries to disk. | `read_only: false` |
| `user` | `"65534:65534"` (nobody) | The container runs as a non-root uid without privileges. A process breakout does not give the attacker write access through tmpfs or bind mounts. | `user: "0:0"` for images that need root (otel reads `/proc`), or a different `"uid:gid"`. |
| `tmpfs` mount opts | automatic `noexec,nosuid,nodev` | A process cannot use a writable tmpfs to run a binary, set setuid bits, or create device nodes. | Put an explicit `exec`, `suid`, or `dev` in the tmpfs option string. |
| `binds` mode | `:ro` when no mode is set | A container cannot write to a host path through a bind mount without an explicit `:rw`. | `"/host:/container:rw"` (explicit) |
| `init` | `true` | tini as PID 1 reaps zombies and forwards SIGTERM, so drains and rolling swaps complete. | `init: false` for images that contain their own init (systemd-in-containers, s6, supervisord). |
| `pids_limit` | `1024` | Puts a bound on the fork-bomb class of exploits. | `pids_limit: null` (no limit), or a positive int. |
| `cpus` | `null` (no limit) | None. | Set to `"2"`, `"500m"`, or `"1.5"` (k8s style). |
| `memory` | `null` (no limit) | None. | Set to `"512Mi"`, `"1Gi"`, `"512m"`, or `"1g"` (k8s style and docker style are accepted). |
| `devices` | `[]` | Only the set that the kernel whitelists is visible. Host hardware (GPUs, USB hubs, `/dev/fuse`, …) is not reachable. | List explicit `--device`-style entries. See [`devices:` entry syntax](#devices-entry-syntax). Narrower than `--privileged`, which yoink does not expose. |

## `user` overrides

The default works for alpine, debian-slim, ubuntu, and the gcr.io distroless variants. These cases need an override.

| image / use case | override | reason |
|---|---|---|
| `otel/opentelemetry-collector-contrib` that reads `/proc` for host metrics | `user: "0:0"` | Needs CAP_DAC_READ_SEARCH (and root) to traverse `/proc/<pid>`. |
| `redis` (the official image) | `user: redis` | The image contains its own non-root account. |
| Images with fixed file ownership for a specific uid | `user: "<uid>:<gid>"` | Without the override, the volume or file permissions collide. |
| Migration container that must chown a new volume | `user: "0:0"` (one-shot pre-deploy hook) | The hook prepares the volume one time. After that, the service runs without the privilege. |

## `devices:` entry syntax

```
<host>[:<container>[:<perms>]]
```

| Part | Meaning |
|---|---|
| `<host>` | The device path on the host. Must start with `/`. |
| `<container>` | The device path in the container. Must start with `/`. |
| `<perms>` | A combination of `r`, `w`, and `m`. The default is `rwm`. `m` is `mknod`, which lets the container create device nodes. Workloads almost never need it. |

The syntax is the same as the `--device` flag of docker. yoink bind-mounts each entry into the container and adds it to the cgroup `devices.allow` list.

## What `devices:` does NOT do

| docker feature | Status |
|---|---|
| `--device-cgroup-rule` (raw cgroup rules such as `c 81:* rmw`) | Not exposed. For a glob over a major number, list each minor explicitly, or open an issue with the use case. |
| `--gpus` (the high-level runtime selector of NVIDIA) | Not passed through. The explicit `/dev/nvidia*` device list and the `nvidia-container-runtime` on the host replace it. |
| `--privileged` | Not exposed, by design. A workload that needs full privilege runs through `docker run --privileged`, outside yoink. |
| Default-deny narrowing | `devices:` *adds* explicit accesses. It does not make the default device whitelist of the runtime tighter. On cgroup v2 hosts, the kernel or runtime config controls the default policy, not yoink. |

## Validation

`yoink validate` rejects malformed `devices:` entries before a deploy starts:

```sh
$ yoink validate
yoink: invalid config: service "media".run.options.devices: invalid device spec "dev/fuse": expected "<host-path>[:<container-path>[:<perms>]]" with absolute paths and `<perms>` drawn from `r`, `w`, `m`
```

| Rejected entry | Example |
|---|---|
| Relative path | `dev/fuse` |
| Empty perms | `...:` |
| Unknown perm character | `rwx` |
| Duplicate perm character | `rrw` |
| Perms longer than 3 characters | |

## Not configured by yoink

| Feature | Where it is set |
|---|---|
| Seccomp and AppArmor profiles beyond the defaults of docker | docker daemon |
| User-namespace remapping (`--userns-remap`) | docker daemon |
| gVisor or Kata runtime selection | docker daemon |

yoink containers inherit these values from the daemon.

## See also

- [Security](/docs/explanation/security): the reason for each default.
- [Relax a hardened default for one service](/docs/how-to/relax-security-defaults): config for the common opt-outs.
- [Pass host devices into a container](/docs/how-to/hardware-passthrough): `devices:` config for each device type.
- [Reference: `RunOptions`](/docs/reference/config#runoptions): the type of each field.
