---
title: Auto-heal unhealthy containers
description: Restart containers Docker marks unhealthy with a small drop-in sidecar — no yoink subcommand, no template.
---

Docker's `HEALTHCHECK` directive is observability-only. `docker ps` shows `(unhealthy)`; nothing acts on it. yoink's default `restart: unless-stopped` policy reincarnates a container that *exits*, but a process that deadlocks while staying alive sits unhealthy until an operator notices. The fix is a label-driven sidecar that watches the docker socket and restarts unhealthy containers — no yoink-managed code path, just a service fragment you drop in.

## Drop-in service fragment [step]

Save as `services/autoheal.yaml`:

```yaml
services:
  - name: autoheal
    image: willfarrell/autoheal
    tag: 1.2.0
    description: Restart containers docker marks unhealthy.
    env:
      AUTOHEAL_CONTAINER_LABEL: autoheal
      AUTOHEAL_INTERVAL: "5"
      # Wait this many checks before acting after a fresh start —
      # avoids killing a container whose first probe is still in
      # the image's `--start-period`. Match this to the largest
      # `--start-period` across your healthchecked images.
      AUTOHEAL_START_PERIOD: "30"
      AUTOHEAL_DEFAULT_STOP_TIMEOUT: "10"
    run:
      binds:
        # Read-only socket bind. The `:ro` is cosmetic — anyone who
        # reaches /var/run/docker.sock can issue `restart` regardless
        # — but it documents intent.
        - /var/run/docker.sock:/var/run/docker.sock:ro
      options:
        restart: unless-stopped
        # autoheal needs root to read the docker socket; yoink's
        # default `nobody` user cannot. cap_drop=ALL stays — talking
        # to dockerd is privileged via socket access, not raw caps.
        user: "0:0"
        read_only: true
        tmpfs:
          /tmp: ""
```

`hosts:` is omitted, so the sidecar lands on every yoink-managed host.

## Opt services in [step]

Auto-heal only acts on containers labelled `autoheal: "true"`. Add the label to any service you want healed:

```yaml
services:
  - name: api
    # ...
    labels:
      autoheal: "true"
```

Services without the label are ignored, including `autoheal` itself.

## Make targets actually heal-able [step]

Auto-heal reads `State.Health.Status`. That field is `null` unless the container declares a `HEALTHCHECK`. Two sources today:

**A. Image-side (Dockerfile).** Bake one in:

```dockerfile
# `apk add --no-cache curl` first if your base image (e.g. nginx:alpine)
# doesn't ship a HTTP client — busybox-wget is gated on the busybox
# config and isn't reliably present.
HEALTHCHECK --interval=10s --timeout=2s --start-period=30s --retries=3 \
  CMD curl -fsS -o /dev/null http://localhost:8080/health || exit 1
```

The `--start-period=30s` covers cold-start latency before the first probe counts against `--retries`. Match the autoheal sidecar's `AUTOHEAL_START_PERIOD: "30"` env so the sidecar also waits before acting.

**B. The bundled `yoink-proxy`** ships a `HEALTHCHECK` already, so the proxy is healed automatically once the sidecar is deployed.

For services whose images don't declare one, add it to the Dockerfile. yoink doesn't yet expose a per-service config knob to inject a `HEALTHCHECK` from the fragment; without one of A or B autoheal sees no signal and the container is silently ignored — the correct fail-quiet behaviour for an indeterminate state.

## Verify [step]

Deploy and confirm the sidecar lands:

```sh
yoink up
yoink status        # autoheal running on each host
```

Manufacture an unhealthy state on a labelled container — freezing PID 1 is the cleanest because the process stays up but stops responding to its `HEALTHCHECK`:

```sh
ssh $HOST -- docker exec api-XXXX kill -SIGSTOP 1
```

The container's status flips to `(unhealthy)` after `interval × retries`; autoheal restarts it within `AUTOHEAL_INTERVAL` seconds. `yoink history api` shows the restart.

## See also

- [Defense in depth](./defense-in-depth.md) — yoink's hardened defaults the fragment leans on (cap-drop, read-only rootfs, tmpfs).
- [Templates](./using-templates.md) — for accessories yoink *does* bundle (postgres, redis, restic, …).
- [Audit log](./audit-log.mdx) — restart events surface here once the heal fires.
