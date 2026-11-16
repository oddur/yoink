---
title: Auto-heal unhealthy containers
description: Restart the containers that Docker marks unhealthy with a small sidecar, without a yoink subcommand or a template.
---

The Docker `HEALTHCHECK` directive only reports: `docker ps` shows `(unhealthy)`, and nothing acts on it. The default yoink policy `restart: unless-stopped` restarts only a container that *exits*. A deadlocked process that stays alive stays unhealthy. A sidecar that monitors the docker socket restarts the unhealthy containers that have a label. The sidecar is a service fragment that you add, not code that yoink manages.

## Deploy the sidecar

### Add the service fragment [step]

Save this fragment as `services/autoheal.yaml`:

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

The fragment has no `hosts:`, so yoink deploys the sidecar on every host.

### Add the label to the services [step]

Add the label `autoheal: "true"` to each service that auto-heal must restart:

```yaml
services:
  - name: api
    # ...
    labels:
      autoheal: "true"
```

Auto-heal ignores a service without the label. The `autoheal` service is one of them.

### Add a `HEALTHCHECK` to the images [step]

Auto-heal reads `State.Health.Status`. That field is `null` unless the container has a `HEALTHCHECK`. Auto-heal ignores a container with no `HEALTHCHECK`, and shows no message.

If the image of a service declares no `HEALTHCHECK`, add one to its Dockerfile. yoink has no per-service config key that injects one. yoink sets a healthcheck only on the bundled `yoink-proxy` container.

```dockerfile
# `apk add --no-cache curl` first if your base image (e.g. nginx:alpine)
# doesn't ship a HTTP client — busybox-wget is gated on the busybox
# config and isn't reliably present.
HEALTHCHECK --interval=10s --timeout=2s --start-period=30s --retries=3 \
  CMD curl -fsS -o /dev/null http://localhost:8080/health || exit 1
```

`--start-period=30s` gives time for a cold start before the first probe counts against `--retries`. Set `AUTOHEAL_START_PERIOD` on the sidecar to the same value. Then the sidecar also waits before it acts.

### Make sure that auto-heal restarts a container [step]

Deploy. Make sure that the sidecar runs on each host:

```sh
yoink up
yoink status        # autoheal running on each host
```

Freeze PID 1 of a labelled container. The process stays alive but does not answer its `HEALTHCHECK`.

```sh
ssh $HOST -- docker exec api-XXXX kill -SIGSTOP 1
```

The container status changes to `(unhealthy)` after `interval × retries`. Auto-heal restarts the container in `AUTOHEAL_INTERVAL` seconds or less. Then `docker ps` on the host shows a new uptime for the container.

## See also

- [Security](/docs/explanation/security): the hardened yoink defaults that the fragment uses (cap-drop, read-only rootfs, tmpfs).
- [Templates](/docs/how-to/using-templates): the accessories that yoink bundles (postgres, redis, restic, and more).
- [Audit log coverage](/docs/reference/audit-log-coverage): the operations that yoink records. A restart by the sidecar is not one of them.
- [Config reference](/docs/reference/config#service): the fields of a service, which include `labels:`.
