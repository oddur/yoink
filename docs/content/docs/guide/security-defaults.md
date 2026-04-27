---
title: Sane security defaults
weight: 3
---

Compose's defaults are dev-friendly, not prod-friendly: every container runs with the full Linux capability set, a writable rootfs, no `pids` cgroup limit, no protection against setuid escalation. Yoink flips those — every service yoink creates is hardened by default. **The only knobs that opt _into_ less safety are explicit:**

| field | yoink default | what it gives you | how to opt out |
|---|---|---|---|
| `cap_drop` | `["ALL"]` | every Linux capability dropped (no `CAP_NET_ADMIN`, `CAP_SYS_ADMIN`, …) | `cap_drop: []` (full default cap set), or surgically re-add via `cap_add` |
| `security_opt` | `["no-new-privileges:true"]` | setuid binaries inside the container can't escalate via `execve` | `security_opt: []` |
| `read_only` | `true` | rootfs mounted read-only — exploits can't drop binaries on disk | `read_only: false` |
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
