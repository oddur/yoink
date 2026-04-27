---
title: Configuration
weight: 5
---

The full `yoink.yaml` schema lives in [`yoink/src/config.rs`](https://github.com/oddur/yoink/blob/main/yoink/src/config.rs) — every field has a doc comment that explains what it's for. Until this page is fleshed out as a full reference, that file is the canonical source.

A skeleton config covering the common fields:

```yaml
deploy:
  networks: [public, api, redis, otel]   # named tiers — services join the ones they need

hosts:
  - { address: my-server, user: deploy } # reachable via tailnet hostname

secrets:
  provider: infisical                    # optional; yoink talks to Infisical's REST API directly
  project_id: <your-infisical-project>
  environment: prod
  # domain: https://infisical.example.com  # only for self-hosted Infisical instances

include:
  - services/*.yaml

# services/api.yaml:
services:
  - name: api
    image: ghcr.io/you/api
    # tag: <required at deploy time via --tag api=<sha>, or set here>
    depends_on: [redis]
    networks: [api, redis]               # can dial only services on these networks
    secrets: [DATABASE_URL]
    pre_deploy:
      - name: api-migrate
        image: ghcr.io/you/api
        tag: { service: api }            # mirror the runtime tag
        cmd: ["migrate"]
        secrets: [DATABASE_MIGRATE_URL]
    run:
      port: 8080
      replicas: 2
      healthcheck_path: /health
      healthcheck_timeout: 60s
      drain_timeout: 30s
      cmd:
        - /usr/local/bin/api
      options:
        memory: "512Mi"
        cpus: "1.5"
        # cap_drop / security_opt / read_only / pids_limit all default
        # to the hardened profile (see Security defaults).
        tmpfs: { /tmp: "size=64m,mode=1777" }
        network_aliases: [api]           # caddy-docker-proxy upstream key
```

Pair with [Security defaults](/docs/security-defaults) for the `options:` block defaults, and [Three deploy modes](/docs/deploy-modes) for the `image:` / `build:` / `tag:` shape.
