---
title: Security
description: Why yoink hardens every container by default, why each opt-out is explicit, and why device passthrough replaces privileged mode.
weight: 3
---

The defaults of Compose are for development, not for production. Every container runs with the full set of Linux capabilities and a writable rootfs. There is no `pids` cgroup limit and no protection against setuid escalation. yoink reverses those defaults: it hardens every service, and each config key that decreases safety is explicit.

## Defaults

The defaults fall into four groups. [Security defaults](/docs/reference/security-defaults) has the value of each field and its opt-out.

**Privileges.** yoink drops every Linux capability (`cap_drop: ["ALL"]`), so a container has no `CAP_NET_ADMIN` and no `CAP_SYS_ADMIN`. The option `no-new-privileges:true` stops setuid binaries in the container from escalating through `execve`. The container runs as `65534:65534` (nobody), a non-root uid without privileges. Thus a process breakout does not immediately give an attacker write access through tmpfs or bind mounts.

**Filesystem.** yoink mounts the rootfs read-only, so an exploit cannot write binaries to disk. A writable scratch `tmpfs` gets `noexec,nosuid,nodev` automatically. Thus an attacker cannot use it to write and run a binary, set setuid bits, or create device nodes. A bind mount with no mode becomes `:ro`, so a write to a host path needs an explicit `:rw`.

**Process control.** tini runs as PID 1 (`init: true`). It reaps zombies and forwards SIGTERM, so drains and rolling swaps complete. A `pids_limit` of `1024` puts a bound on the fork-bomb class of exploits. `cpus` and `memory` have no limit by default.

**Hardware.** `devices` is empty by default. A container sees only the set of devices that the kernel whitelists. Host hardware such as GPUs, USB hubs, and `/dev/fuse` is not reachable.

## Surgical opt-outs in practice

Each opt-out applies to one field of one service. A service that needs one privilege gets that privilege and keeps the other hardened defaults. For example, Caddy must bind `:80` and `:443`, so its config adds `NET_BIND_SERVICE` again and nothing else. The image of pgadmin writes to its rootfs at startup, so its config sets `read_only: false` and keeps the other defaults.

[Relax a hardened default for one service](/docs/how-to/relax-security-defaults) has the config for these cases.

## Non-root in practice

`user: "65534:65534"` works without changes for most images. alpine, debian-slim, ubuntu, and the gcr.io distroless variants all contain `nobody:nogroup` at that uid pair. The cases that need an override are few and predictable: an image that reads `/proc`, an image with its own non-root account, files with fixed ownership, and a one-shot `chown` of a new volume. [Security defaults](/docs/reference/security-defaults#user-overrides) lists them.

The default gives a fast signal. A container that cannot run as nobody stops quickly, and `yoink logs <svc>` shows "permission denied". Only that failure mode is a reason for an override.

## Read-only rootfs in practice

An app with a read-only rootfs still needs a location to write: `/tmp`, sometimes `/run`, occasionally a cache directory. `read_only: true` together with `tmpfs:` gives the app scratch space that does not persist. [Relax a hardened default for one service](/docs/how-to/relax-security-defaults#give-a-read-only-service-scratch-space) shows the config.

## No host port binds by default

yoink does not add `publish:` entries for you, so it **never binds a service directly to a host port**. Other services reach a service without a `publish:` block on its docker network, by alias. If the service has a `domain:`, the bundled Caddy proxy reaches it too.

This default is correct for all services except two kinds:

- An operator UI that must be reachable on the loopback of the host, for example pgadmin or a monitoring dashboard behind a tailnet.
- The proxy itself.

Debugging is the usual pressure against this default: "how do I curl this from my laptop?" The answer is [`yoink pf <service>`](/docs/explanation/networking#port-forward). It opens a tunnel to a container, published or not, through the same SSH connection that the deploy uses. Thus debugging never changes what production exposes.

## Hardware passthrough (`devices:`)

`devices:` passes host devices into a container: GPUs, USB hubs, FUSE, audio, webcams, and TPMs. It uses the same `<host>[:<container>[:<perms>]]` syntax as the `--device` flag of docker. The other hardened defaults of yoink stay in effect.

`devices:` is narrower than `--privileged`, and yoink intentionally does not expose `--privileged`. Only the devices in the list become accessible. A workload that needs full privilege is rare, and it is usually wrong for a container. Such a workload runs through `docker run --privileged`, apart from yoink.

yoink bind-mounts each entry into the container *and* adds it to the cgroup `devices.allow` list. The two parts are necessary. With only the bind mount, `open()` fails with `EPERM` because of the cgroup whitelist. With only the allow entry, the device is not visible.

`devices:` *adds* explicit accesses. It does not make the default device whitelist of the runtime tighter. On cgroup v2 hosts the default policy varies, and the kernel or runtime config controls it, not yoink.

[Pass host devices into a container](/docs/how-to/hardware-passthrough) has the config for each device type. [Security defaults](/docs/reference/security-defaults#devices-entry-syntax) has the syntax, the limits, and the validation rules.

## What yoink does not do automatically (for now)

yoink does not configure these items:

- seccomp or AppArmor profiles beyond the defaults of docker
- user-namespace remapping (`--userns-remap`)
- the selection of a gVisor or Kata runtime

Those are concerns of the full host, not config for one service. When you set them on the docker daemon, the yoink containers inherit them.

The container detail of the TUI (`i` on a container row) shows the effective security profile of each running container under "security & limits". A container with a relaxed default is visible there.

## See also

- [Security defaults](/docs/reference/security-defaults): each default, its effect, and its opt-out, in table form.
- [Relax a hardened default for one service](/docs/how-to/relax-security-defaults): the config for each common opt-out.
- [Pass host devices into a container](/docs/how-to/hardware-passthrough): `devices:` config for GPUs, USB, FUSE, webcams, and TPMs.
- [Reference: `RunOptions`](/docs/reference/config#runoptions): every runtime field of a service.
- [Networking](/docs/explanation/networking): the no-publish default and how `yoink pf` makes it practical.
- Docker's [`--device`](https://docs.docker.com/engine/reference/commandline/run/#device): the primitive that `devices:` uses.
