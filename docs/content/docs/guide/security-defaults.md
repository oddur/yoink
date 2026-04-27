---
title: Secure by default
weight: 3
---

Compose's defaults are dev-friendly, not prod-friendly: every container runs with the full Linux capability set, a writable rootfs, no `pids` cgroup limit, no protection against setuid escalation. Yoink flips those — every service yoink creates is hardened by default. **The only knobs that opt _into_ less safety are explicit:**

| field | yoink default | what it gives you | how to opt out |
|---|---|---|---|
| `cap_drop` | `["ALL"]` | every Linux capability dropped (no `CAP_NET_ADMIN`, `CAP_SYS_ADMIN`, …) | `cap_drop: []` (full default cap set), or surgically re-add via `cap_add` |
| `security_opt` | `["no-new-privileges:true"]` | setuid binaries inside the container can't escalate via `execve` | `security_opt: []` |
| `read_only` | `true` | rootfs mounted read-only — exploits can't drop binaries on disk | `read_only: false` |
| `user` | `"65534:65534"` (nobody) | container runs as a non-root unprivileged uid; a process breakout doesn't immediately give the attacker write access through tmpfs / bind mounts | `user: "0:0"` for images that need root (otel reading `/proc`), or any other `"uid:gid"` |
| `tmpfs` mount opts | auto-`noexec,nosuid,nodev` | a writable scratch tmpfs can't be used to drop + run a binary, set setuid bits, or create device nodes | include explicit `exec`/`suid`/`dev` in your tmpfs option string |
| `binds` mode | `:ro` when no mode set | accidental bind-mount-and-write to host paths is impossible without explicit `:rw` | `"/host:/container:rw"` (explicit) |
| `init` | `true` | tini as PID 1 reaps zombies + forwards SIGTERM, so drains and rolling swaps actually finish | `init: false` for images that ship their own init (systemd-in-containers, s6, supervisord) |
| `pids_limit` | `1024` | bounds the fork-bomb class of exploits | `pids_limit: null` (unlimited), or any positive int |
| `cpus` | `null` (uncapped) | — | set to `"2"`, `"500m"`, `"1.5"`, etc. — k8s style |
| `memory` | `null` (uncapped) | — | set to `"512Mi"`, `"1Gi"`, `"512m"`, `"1g"` — k8s + docker styles both accepted |

## Surgical opt-outs in practice

```yaml
# caddy needs to bind :80/:443 — re-add NET_BIND_SERVICE only.
- name: caddy
  run:
    cap_drop: [ALL]
    cap_add:  [NET_BIND_SERVICE]
    cpus: "0.5"
    memory: "64Mi"

# pgadmin's image scribbles all over its rootfs at startup — back out
# of read_only, keep the rest of the hardened defaults.
- name: pgadmin
  run:
    read_only: false
    cpus: "1"
    memory: "256Mi"

# Image with stubborn legacy code that needs root + writable /etc.
- name: legacy-thing
  run:
    user: "0:0"
    read_only: false
    cap_drop: []
    security_opt: []
```

## Non-root in practice

`user: "65534:65534"` works out of the box for the vast majority of images — alpine, debian-slim, ubuntu, gcr.io distroless variants all ship `nobody:nogroup` at that uid pair. The cases where the operator has to override are predictable and few:

| image / use case | override | why |
|---|---|---|
| `otel/opentelemetry-collector-contrib` reading `/proc` for host metrics | `user: "0:0"` | needs CAP_DAC_READ_SEARCH (and root) to traverse `/proc/<pid>` |
| `redis` (the official image) | `user: redis` | image ships its own non-root account; we just point at it |
| Images with pre-baked file ownership for a specific uid | `user: "<uid>:<gid>"` | volume / file-perm collision otherwise |
| Migration container that needs to chown a fresh volume | `user: "0:0"` (one-shot pre-deploy hook) | initial setup, fine to drop privilege after |

When in doubt: deploy with the default first, watch the container fail-fast (`yoink logs <svc>`), and override only when the failure mode is "permission denied" rather than "function works fine".

## Read-only rootfs in practice

The app still needs *somewhere* to write — `/tmp`, sometimes `/run`, occasionally a cache directory. Pair `read_only: true` with `tmpfs:` to give it scratch space without letting it persist:

```yaml
run:
  read_only: true
  tmpfs:
    /tmp: "size=64m,mode=1777"
    /var/cache/app: "size=128m,mode=0755"
```

## What yoink doesn't do automatically (for now)

Seccomp/AppArmor profiles beyond docker's defaults, user-namespace remapping (`--userns-remap`), gVisor / Kata runtime selection. Those are host-wide concerns, not per-service config — set them on the docker daemon and yoink containers inherit.

The TUI's container detail (`i` from any container row) shows the effective security profile for each running container under "security & limits" — easy to spot when something's accidentally lax.
