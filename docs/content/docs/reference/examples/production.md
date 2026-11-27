---
title: Production-shape
description: "A full production config with multiple hosts, replicas, sealed secrets, pre-deploy hooks, and drift detection."
weight: 3
---

This config is for a deploy that a team ships against. It has these parts:

- age-sealed secrets in git, with no plaintext in the repo
- a config in fragments across files
- pre-deploy migrations
- a staging entry that runs next to prod on the same hosts
- a diff for reviewers at PR time
- registry images that CI builds

The config starts with one host. You can add more hosts later.

## Layout

```
yoink.prod.yaml             # prod entry; includes services/prod/*.yaml
yoink.staging.yaml          # staging entry; includes services/staging/*.yaml
services/prod/api.yaml
services/prod/web.yaml
services/prod/redis.yaml
services/prod/otel.yaml
services/staging/api.yaml   # name: api-staging, networks: [api-staging, …]
services/staging/web.yaml   # name: web-staging
```

## `yoink.prod.yaml`

```yaml
deploy:
  # `yoink-ingress` and `yoink-admin` are auto-injected by the
  # bundled-proxy code path — no need to list them here.
  networks: [api, web, redis, otel]

hosts:
  - { address: prod-eu-1, user: deploy }
  # add more as needed; replicas + services[].hosts handle distribution
  # — see /docs/explanation/networking#multi-host-distribution

# Age-sealed secrets committed to the repo. Decrypted once per `up`
# from the identity in `YOINK_AGE_KEY` (CI) or auto-discovered from
# `~/.config/yoink/keys/<recipient>.key` (laptop) and folded into
# spec_hash. See /docs/explanation/secrets for the full flow;
# /docs/explanation/secrets covers the `provider: command`
# alternative if you'd rather pull from Doppler / 1Password / Vault /
# AWS Secrets Manager / the Infisical CLI.
secrets:
  provider: age
  recipients:
    - age1examplepublickeyreplacewithyourown000000000000000000000000

# Registry credentials live in the same secrets bundle. Yoink looks up
# these named keys and passes them to docker as X-Registry-Auth on
# each pull from `registry.server`. Skip this block when every service is locally-built
# (yoink ships build artifacts straight from your docker daemon, no
# auth needed) or when pulling exclusively from public registries.
registry:
  server: ghcr.io
  username_secret: GHCR_USERNAME
  password_secret: GHCR_TOKEN

include:
  - services/prod/*.yaml

# Bundled reverse proxy (yoink-proxy). Renders Caddy from each
# service's `domain:` field; ACME is implicitly off because
# `proxy.tls.cert_secret` is set, and a :80 → :443 redirect is
# auto-emitted. Origin-pull mTLS locks the origin to Cloudflare's
# edge — direct hits to the host IP fail at TLS handshake.
proxy:
  tls:
    cert_secret: CF_ORIGIN_CERT
    key_secret:  CF_ORIGIN_KEY
    client_auth:
      mode: require_and_verify
      trust_pool_secret: CF_ORIGIN_PULL_CA
```

The alternative to sealed Cloudflare origin certs is ACME (Let's Encrypt). That config has no `proxy.tls` and sets `proxy.email: ops@example.com`. Then each service with `domain:` gets a cert. See the [proxy guide](/docs/explanation/proxy) and the [Cloudflare Origin Certs recipe](/docs/how-to/cloudflare-origin-certs).

## `services/prod/api.yaml`

```yaml
services:
  - name: api
    image: ghcr.io/you/api
    # tag: provided at deploy via `--tag api=$(git rev-parse HEAD)`
    depends_on: [redis, otel]
    networks: [api, redis, otel]            # api dials redis + otel
    domain: api.example.com                 # bundled proxy fronts this
    upstream_h2c: true                      # gRPC + HTTP/1.1 over one h2c upstream
    secrets: [DATABASE_URL, JWT_SIGNING_KEY]
    env_from_secrets:
      # Same secret store, different env-var name on the container
      OTEL_EXPORTER_OTLP_HEADERS: GRAFANA_CLOUD_OTEL_AUTH_HEADER
    env:
      OTEL_EXPORTER_OTLP_ENDPOINT: http://otel:4318
      RUST_LOG: info
    pre_deploy:
      - name: api-migrate
        image: ghcr.io/you/api
        tag: { service: api }               # mirror the runtime tag exactly
        cmd: ["migrate"]
        secrets: [DATABASE_MIGRATE_URL]     # different role, same store
    run:
      port: 8080
      replicas: 2
      healthcheck_path: /health
      healthcheck_timeout: 60s
      drain_timeout: 30s
      cmd: [/usr/local/bin/api]
      options:
        memory: "1Gi"
        cpus: "1.5"
        tmpfs:
          /tmp: "size=64m,mode=1777"
        network_aliases: [api]
```

## `services/prod/redis.yaml`

```yaml
services:
  - name: redis
    image: redis
    tag: 7-alpine
    networks: [redis]
    run:
      port: 6379
      cmd:
        - redis-server
        - --maxmemory
        - 512mb
        - --maxmemory-policy
        - allkeys-lru
        - --appendonly
        - "no"
      options:
        memory: "600Mi"
        cpus: "0.5"
        user: redis                          # avoid the root → drop dance
        tmpfs:
          /data: "size=64m,mode=1777"
        network_aliases: [redis]
```

## Reverse proxy

There is no `caddy.yaml` fragment. yoink synthesizes the bundled proxy from the top-level `proxy:` block and the `domain:` of each service. [The proxy guide](/docs/explanation/proxy) has the full config (mTLS, h2c, multi-domain canonical redirects, snippets).

## `services/prod/otel.yaml`

```yaml
services:
  - name: otel
    image: otel/opentelemetry-collector-contrib
    tag: "0.150.1"                            # pin exact — minor versions ship breaking renames
    networks: [otel]
    secrets: [GRAFANA_CLOUD_OTEL_AUTH_HEADER]
    env:
      HOST_NAME: prod-eu-1
    run:
      port: 13133                             # health_check extension
      healthcheck_path: /
      healthcheck_timeout: 30s
      files:
        # File content hashes into spec_hash, so a config edit reroles
        - "services/prod/config/otel-collector.yaml:/etc/otelcol-contrib/config.yaml:ro"
      binds:
        - "/proc:/hostfs/proc:ro"
        - "/sys:/hostfs/sys:ro"
        - "/:/hostfs:ro"
        - "/var/run/docker.sock:/var/run/docker.sock:ro"
      options:
        memory: "256Mi"
        cpus: "0.5"
        # Host metrics receiver needs root + reads under /proc/<pid>;
        # opt out of the hardened defaults for this one.
        user: "0:0"
        cap_drop: []
        security_opt: []
        read_only: false
        network_aliases: [otel]
```

## CI deploys

Two workflows run yoink. The skeleton of each:

```yaml
# .github/workflows/deploy.yml — runs on push to live
- run: |
    yoink up --tag api=${{ github.sha }} --tag web=${{ github.sha }}
  env:
    YOINK_AGE_KEY: ${{ secrets.YOINK_AGE_KEY }}
```

```yaml
# .github/workflows/pr-diff.yml — runs on PRs touching services/**
- run: |
    yoink up --dry-run --format=markdown \
      --tag api=${{ github.event.pull_request.head.sha }} \
      --tag web=${{ github.event.pull_request.head.sha }} > diff.md
- uses: marocchino/sticky-pull-request-comment@v2
  with:
    header: yoink-pr-diff
    path: diff.md
```

[PR-comment dry-run](/docs/how-to/pr-comment-dry-run) has the complete workflow.

## What this exercises

| Feature | In this config |
|---|---|
| **Multi-tier networks** | `redis` is only on `redis`. otel is isolated on `otel`. api joins `api/redis/otel`, the networks that it dials. |
| **Bundled reverse proxy** | `domain:` on api and web, one `proxy.tls` block at the top, and no `caddy.yaml`. yoink puts the proxy and each routed service on `yoink-ingress`. |
| **Pre-deploy hooks** | `api-migrate` runs one time for each deploy of `api`, immediately before the new containers start. It uses the same image and tag as the runtime container. |
| **`env_from_secrets`** | The store has the secret under one name, and the container gets it under a different env-var name. This is for apps that expect a fixed env-var name. |
| **`files:` mounts** | A read-only bind whose content hash goes into `spec_hash`. After an edit of the otel config, the next deploy replaces the otel container. |
| **Surgical security opt-outs** | `otel` opts out of four hardened defaults (with a comment). All other services inherit them. |
| **Config fragmentation** | One file for each service, included by glob. A service rename changes one file. |
| **Staging alongside prod** | `yoink.staging.yaml` with `name: api-staging` on the same hosts. See [Staging alongside prod](/docs/how-to/staging-alongside-prod). |

## See also

- [Polyglot-stack example](/docs/reference/examples/polyglot-stack): the single-host version of the same patterns.
- [Architecture](/docs/explanation/architecture): drift detection, the deploy lock, and the healthcheck-gated swap.
- [Sealed secrets workflow](/docs/how-to/sealed-secrets-workflow): the commands for the `secrets:` block of this example.
- [Pre-merge dry-run on every PR](/docs/how-to/pr-comment-dry-run): how to put the `dry_run.yaml` snippet into a real CI workflow.
- [Configuration](/docs/reference/config): each field in these fragments.
