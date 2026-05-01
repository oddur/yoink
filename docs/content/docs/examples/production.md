---
title: Production-shape
description: "A full production-shape config: multi-host, replicas, sealed secrets, pre-deploy hooks, and drift detection."
weight: 3
---

Once a deploy is what the team ships against, you need more than `compose up`: secrets out of the repo, config split across files, pre-deploy migrations, a staging environment on the same hardware, and a PR-time diff for reviewers.

**A prod-shape `yoink.yaml`**: multiple hosts (start with one, scale later), age-sealed secrets in git, CI-built registry images, config fragmented across files, and a staging entry running alongside prod on the same hosts.

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
  # `yoink-ingress` and `yoink-proxy-admin` are auto-injected by the
  # bundled-proxy code path — no need to list them here.
  networks: [api, web, redis, otel]

hosts:
  - { address: prod-eu-1, user: deploy }
  # add more as needed; replicas + services[].hosts handle distribution
  # — see /docs/guide/networking#multi-host-distribution

# Age-sealed secrets committed to the repo. Decrypted once per `up`
# from the identity in `YOINK_AGE_KEY` (CI) or auto-discovered from
# `~/.config/yoink/keys/<recipient>.key` (laptop) and folded into
# spec_hash. See /docs/guide/secrets for the full flow;
# /docs/guide/secrets covers the `provider: command`
# alternative if you'd rather pull from Doppler / 1Password / Vault /
# AWS Secrets Manager / the Infisical CLI.
secrets:
  provider: age
  recipients:
    - age1examplepublickeyreplacewithyourown000000000000000000000000

# Registry credentials live in the same secrets bundle. Yoink looks up
# these named keys and passes them to docker as X-Registry-Auth on
# every pull. Skip this block when every service is locally-built
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

For ACME (Let's Encrypt) instead of sealed Cloudflare origin certs, drop `proxy.tls` and set `proxy.email: ops@example.com`; every service with `domain:` then auto-issues. See the [proxy guide](/docs/guide/proxy) and the [Cloudflare Origin Certs recipe](/docs/how-to/cloudflare-origin-certs).

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

No `caddy.yaml` fragment. The bundled proxy is synthesized from the top-level `proxy:` block plus each service's `domain:`. See [the proxy guide](/docs/guide/proxy) for the full surface (mTLS, h2c, multi-domain canonical redirects, snippets).

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

Two repo workflows do the work. Skeleton:

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

See [PR-comment dry-run](/docs/how-to/pr-comment-dry-run) for the complete workflow.

## What this exercises

- **Multi-tier networks**: `redis` only on `redis`; otel isolated on `otel`; api joined to `api/redis/otel` per its dial-out needs.
- **Bundled reverse proxy**: `domain:` on api/web, one `proxy.tls` block at the top, no `caddy.yaml`. Auto-joins `yoink-ingress` with each routed service.
- **Pre-deploy hooks**: `api-migrate` runs once per up before the api swap, same image as the runtime.
- **`env_from_secrets`**: store under one name, expose under a different env-var name. For legacy env-var conventions.
- **`files:` mounts**: content-hashed bind that feeds into `spec_hash`. Edit the otel config, redeploy, the otel container rerolls.
- **Surgical security opt-outs**: `otel` opts out of the hardened defaults (with a comment); everything else inherits.
- **Config fragmentation**: one file per service, glob-included. Renaming a service is a one-file operation.
- **Staging alongside prod**: see [the recipe](/docs/how-to/staging-alongside-prod) for `yoink.staging.yaml` with `name: api-staging` on the same hosts.

## See also

- [Polyglot-stack example](/docs/examples/polyglot-stack) — single-host version of the same patterns.
- [Architecture](/docs/guide/architecture) — drift detection, deploy lock, healthcheck-gated swap.
- [Sealed secrets workflow](/docs/how-to/sealed-secrets-workflow) — operator-side commands for the `secrets:` block this example uses.
- [Pre-merge dry-run on every PR](/docs/how-to/pr-comment-dry-run) — wire the `dry_run.yaml` snippet into a real CI workflow.
