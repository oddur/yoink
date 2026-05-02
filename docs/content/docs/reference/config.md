---
title: Configuration
description: Complete yoink.yaml schema reference — every field, its type, and its default.
weight: 5
---

Full schema for `yoink.yaml`. Canonical source: [`yoink/src/config.rs`](https://github.com/oddur/yoink/blob/main/yoink/src/config.rs).

## Top level

| Field | Type | Default | Notes |
|---|---|---|---|
| `slug` | string (optional) | unset | Banner shown in the TUI's top chrome; use to mark a config (e.g. `"PRODUCTION — TREAD CAREFULLY"`) so the operator can't miss which environment they're pointed at. Keep it short (single line). |
| `hosts` | list of [Host](#host) | required | One or more deploy targets. |
| `deploy` | [Deploy](#deploy) | `{}` | Cross-service defaults (networks, etc.). |
| `secrets` | [Secrets](#secrets) | unset | Secret provider config. Required if any service uses `secrets:` / `env_from_secrets:`. |
| `registry` | [Registry](#registry) | unset | Image-pull credentials. Skip when every service is locally-built (yoink ships build artifacts from your docker daemon, no auth needed) or when pulling exclusively from public registries. |
| `proxy` | [Proxy](#proxy) | unset | Bundled Caddy reverse proxy. Implicitly enabled when any service has a `domain:` set. |
| `services` | list of [Service](#service) | `[]` | Services to deploy. Can be defined inline or split via `include:`. |
| `hooks` | [Hooks](#hooks) | `{}` | Top-level hooks not scoped to a single service. Currently: `pre_deploy:`. |
| `webhooks` | list of [Webhook](#webhooks) | `[]` | Outbound HTTP notifications fired on run-level events. Operator-side, best-effort. |
| `include` | list of glob | `[]` | Glob paths (relative to the config file) merged into `services`. |

## Host

Each host needs an address; that can be either a literal `address:` or, for cases where the address itself is sensitive (a public IP that you front through Cloudflare, a tailnet hostname you'd rather not commit), a sealed-secret reference via `address_secret:`. Set exactly one; neither or both is rejected at config load.

| Field | Type | Notes |
|---|---|---|
| `address` | string | Hostname or IP. Anything your local SSH client accepts: a raw IP, a DNS name, a `~/.ssh/config` alias, or a tailnet hostname. Mutually exclusive with `address_secret`. |
| `address_secret` | string (optional) | Name of an entry in your sealed-secrets bundle holding the host's address (IP or hostname). Resolved at config-load time after the bundle decrypts. Use this when the deploy config sits in a public repo and the host's address is sensitive. Mutually exclusive with `address`. |
| `user` | string | SSH user. Must be in the `docker` group on the host (or be `root`). |
| `ssh_key_secret` | string (optional) | Name of an entry in your sealed-secrets bundle holding a PEM-formatted SSH private key. When set, yoink decrypts the key into a per-process tempfile (mode `0o600`) and uses it for this host's SSH connections, both the docker daemon connection and the pre-flight ssh probe. Lets you ship the deploy key with the repo (encrypted at rest in `secrets.age`) instead of relying on every operator's personal `ssh-agent`. |

```yaml
hosts:
  - { address: prod-eu-1, user: deploy }
  - { address: prod-us-1, user: deploy }

  # Deploy-key-in-repo flavour (for fresh VPS hosts without
  # ssh-agent already configured for them):
  - address: 1.2.3.4
    user: root
    ssh_key_secret: PROD_HOST_SSH_KEY

  # Address-in-repo-too flavour: the IP lives in `secrets.age`,
  # never in the committed yaml. Resolved at deploy time.
  - address_secret: DOCS_HOST_IP
    user: deploy
    ssh_key_secret: DOCS_DEPLOY_SSH_KEY
```

`yoink hosts add --address-secret <KEY> --user <USER> --name <NAME>` writes the sealed-address fragment shape. `--name` is required (no literal address to derive a fragment filename from).

## Deploy

| Field | Type | Default | Notes |
|---|---|---|---|
| `networks` | list of string | `[]` | Declared docker networks. Services attach via `services[].networks:`. Yoink creates each one if missing. |

## Secrets

Two providers, selected by the `provider:` tag.

### `provider: age` (default, batteries-included)

A single sealed dotenv file committed alongside `yoink.yaml`, decrypted at deploy time with the operator's age **identity** (private half) resolved in priority order:

1. `YOINK_AGE_KEY` env (raw key): CI / managed-env contexts
2. `YOINK_AGE_KEY_FILE` env (path): explicit override
3. `~/.config/yoink/keys/*.key`: laptop default; yoink scans the dir and picks whichever key's public half matches one of `recipients:` below
4. `~/.config/yoink/age.key`: legacy single-key fallback

The keys-dir scan (#3) is what makes `yoink secrets key generate` work without per-project env-var setup; multiple projects with distinct identities coexist there, picked by recipient match.

| Field | Type | Default | Notes |
|---|---|---|---|
| `provider` | string | required | `age` |
| `recipients` | list of string | `[]` | Public age **recipients** (`age1...`), one per principal that needs to decrypt. New writes are sealed against every entry; decryption only needs *one* matching identity. Validates non-empty at config-load. |
| `file` | string | `secrets.age` | Sealed file path, relative to the config file's directory. `..` and absolute paths are rejected. |
| `profiles` | map of [SecretsProfile](#secretsprofile) | `{}` | Named env-shape recipes for downstream consumers (Terraform state-backend creds, provider tokens, CI shell exports). Consumed by `yoink secrets env --profile <NAME>` and by `hooks.pre_deploy[].secrets_profile`. See [Secrets profiles](#secrets-profiles). |

The recipient/identity split is the asymmetric-keypair mental model; see [Sealed secrets (age)](/docs/guide/secrets#sealed-secrets-age--the-default) in the secrets guide for the full explanation.

#### Secrets profiles

A profile names "what env shape does one downstream tool expect" and lets the same recipe drive both an operator shell (Taskfile, CI workflow) and a yoink pre-deploy hook. Without profiles, the same `bundle-key → env-var-name` mapping is hand-encoded in N Taskfiles plus N CI workflows, with a separate `unset` block per consumer for env-leak cleanup; profiles consolidate it next to the bundle that supplies the values.

##### `SecretsProfile`

| Field | Type | Default | Notes |
|---|---|---|---|
| `include` | list of string | `[]` | Allowlist of bundle keys this consumer cares about. Empty = "every key in the bundle." A listed key missing from the bundle is a hard error at resolve time — loud-fail beats silently feeding empty creds. |
| `rename` | map of string→string | `{}` | `bundle_key → env_var_name`. Renames apply after `include`; keys not in the map keep their original name. Targets must be POSIX env-var names (`[A-Za-z_][A-Za-z0-9_]*`); mixed case is allowed because Terraform's `TF_VAR_<varname>` puts the (lowercase) variable name in the suffix. |
| `unset` | list of string | `[]` | Env vars to clear in the importing shell *before* the exports land. Solves the "devbox `init_hook` leaks `B2_ENDPOINT` and the b2 SDK underneath the terraform provider mis-routes auth" class of issue. **Operator-shell-only** — a hook runs in a fresh container with no parent env to clear; referencing an `unset` profile from a hook is a config-validation error. |

Profile names must match `[a-z0-9-]+` (lowercase letters, digits, hyphens).

```yaml
secrets:
  provider: age
  recipients: [age1…]
  profiles:
    terraform-cloudflare:
      include: [CLOUDFLARE_API_TOKEN,
                TFSTATE_B2_KEY_ID, TFSTATE_B2_APPLICATION_KEY]
      rename:
        TFSTATE_B2_KEY_ID:          AWS_ACCESS_KEY_ID
        TFSTATE_B2_APPLICATION_KEY: AWS_SECRET_ACCESS_KEY
      unset: [B2_ENDPOINT, B2_BUCKET_NAME]
```

Use it from a Taskfile:

```yaml
env:
  YOINK_TF:
    sh: cd ../../deploy-prod && yoink secrets env --profile terraform-cloudflare
tasks:
  tf:plan:
    cmds:
      - eval "$YOINK_TF" && terraform plan
```

Or from a yoink pre-deploy hook (subprocess flavor — runs on the operator's machine):

```yaml
hooks:
  pre_deploy:
    - name: cloudflare-tf-apply
      working_dir: ../terraform/cloudflare
      cmd: ["sh", "-c",
            "terraform init -input=false && terraform apply -auto-approve"]
      secrets_profile: terraform-cloudflare
```

The profile's `unset:` applies on subprocess hooks (yoink calls `Command::env_remove` for each entry); container hooks reject it because there's no parent env to clear. See [Run Terraform from a hook](/docs/how-to/terraform-from-hooks) for the full walkthrough.

In CI, when `GITHUB_ENV` is set in the environment, `yoink secrets env` auto-prepends `echo '::add-mask::<value>'` lines for every revealed value — no per-key copy-paste needed; new keys added to the profile are masked automatically.

Bootstrap: `yoink secrets key generate` writes a fresh **identity** to `~/.config/yoink/keys/<recipient>.key` (mode 0600) by default and prints the matching **recipient** (`age1…`, public; paste into `recipients:` above). `--out PATH` writes to a specific file at mode 0600 instead. `--print` sends the secret to stdout for piping into a CI secret store (`… --print | gh secret set YOINK_AGE_KEY`). `yoink secrets key public` re-derives the recipient from whichever identity yoink would use right now (handy "is the key in my shell the same one yoink.yaml expects?" check). See the [secrets guide](/docs/guide/secrets).

### `provider: command` (bring-your-own-tool)

Yoink invokes the configured command and reads a secrets bundle from stdout. Format auto-detects between dotenv and JSON. Lets operators wire any external manager (Doppler, 1Password, HashiCorp Vault, AWS Secrets Manager, the Infisical CLI, Bitwarden, …) without yoink growing first-party integrations for each.

| Field | Type | Default | Notes |
|---|---|---|---|
| `provider` | string | required | `command` |
| `command` | list of string | required | Argv to spawn. First element is the binary, the rest are arguments. No shell interpretation; wrap in `["sh", "-c", "..."]` if you need pipes. |
| `format` | string | `auto` | `auto` inspects the first non-whitespace byte (`{` → JSON, else dotenv). `dotenv` / `json` force the parser. |

```yaml
secrets:
  provider: command
  command: ["doppler", "secrets", "download", "--no-file", "--format", "env"]
```

See the [secrets guide](/docs/guide/secrets) for per-tool wiring.

## Registry

| Field | Type | Notes |
|---|---|---|
| `server` | string | Registry hostname (e.g. `ghcr.io`). |
| `username_secret` | string | Name of the secret in your provider holding the username. |
| `password_secret` | string | Name of the secret holding the token/password. |

Yoink resolves these at deploy time and passes them as `X-Registry-Auth` on every pull.

## Proxy

The bundled Caddy reverse proxy. See the [proxy guide](/docs/guide/proxy) for the full operator-side picture.

| Field | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | implicit when any service has `domain:` | Force-on when no service has a domain (e.g. routes are only configured via `global_handlers:`). |
| `email` | string | unset | Let's Encrypt registration email. **Required when any service uses `tls: auto`.** Not used when `proxy.tls:` is set. |
| `image` | string | `caddy:2` | Pre-built caddy image (registry-pulled). Mutually exclusive with `xcaddy:`. |
| `xcaddy` | [Xcaddy](#xcaddy) | unset | Compile a custom caddy on each proxy host with plugins. Mutually exclusive with `image:`. |
| `cert_volume` | string | `yoink_caddy_data` | Named volume for ACME state and certs. Don't delete casually — Let's Encrypt rate-limits aggressively. |
| `bind` | string | unset (all interfaces) | Host IP to bind `:80` and `:443` to. Common use: bind to a Tailscale IP so the proxy is reachable only over the tailnet. The admin port stays on `127.0.0.1` regardless. |
| `tls` | [ProxyTls](#proxytls) | unset | Proxy-level TLS — every routed service inherits this cert (and optional mTLS) by default. ACME implicitly off when set. |
| `config_extra` | string (JSON) | unset | Top-level Caddy JSON snippet, deep-merged into the rendered config before `/load`. Escape hatch for global settings yoink doesn't model as typed fields — `trusted_proxies`, `storage`, plugin app blocks. |
| `global_handlers` | list of string (JSON) | `[]` | Caddy handlers (and/or routes) that run for every request *before* any per-service route matches. The natural place for proxy-wide concerns: CrowdSec bouncer, Coraza WAF, fleet-wide rate limiting. |
| `global_handlers_after` | list of string (JSON) | `[]` | Same shape as `global_handlers:`, but the chain runs *after* per-service routes match. Use for concerns that need the upstream's response (response headers, access-log shaping, post-processing). |

### Xcaddy

Builds a custom caddy on each proxy host using `xcaddy`. Content-addressed by build inputs, tagged locally as `yoink-caddy:<hash>`; subsequent `up` runs short-circuit until plugins or version change. No registry needed.

| Field | Type | Default | Notes |
|---|---|---|---|
| `plugins` | list of string | required (non-empty) | One entry per caddy module. Bare module path or pinned (`module@version`) — same syntax as `xcaddy build --with`. Sorted alphabetically before hashing/rendering. |
| `caddy_version` | string | xcaddy's latest tagged release | Caddy git tag to compile (e.g. `v2.8.4`). Pinned values pass verbatim to `xcaddy build`. |
| `base_image` | string | `caddy:2` | Runtime image. Override only to bind to a specific caddy patch version. |
| `builder_image` | string | `caddy:2-builder` | Build stage (carries xcaddy + Go toolchain). Pin to `caddy:<v>-builder` to also pin the xcaddy CLI. |
| `replace` | list of string | `[]` | Forwarded as `xcaddy build --replace` entries. Useful for testing a local fork of a plugin. |

### ProxyTls

When set, every routed service uses this cert by default. Per-service `tls_cert_secret:` / `tls_key_secret:` overrides remain available for the rare different-cert-per-service case. `:80 → :443` redirect auto-emitted.

| Field | Type | Default | Notes |
|---|---|---|---|
| `cert_secret` | string | required | Sealed-secret name holding the PEM cert (full chain). |
| `key_secret` | string | required | Sealed-secret name holding the PEM private key. |
| `client_auth` | [ClientAuth](#clientauth) | unset | Optional mTLS (e.g. Cloudflare origin-pull). |

### ClientAuth

| Field | Type | Default | Notes |
|---|---|---|---|
| `mode` | enum | `require_and_verify` | `request` / `require` / `verify_if_given` / `require_and_verify`. Most setups want the strict default. |
| `trust_pool_secret` | string | required | Sealed-secret name holding the PEM CA bundle that signs accepted client certs. |

## Service

| Field | Type | Default | Notes |
|---|---|---|---|
| `name` | string | required | Used as container name and DNS alias. Unique within the config. |
| `image` | string | required | Image reference. Bare names (no `/`) trigger no-registry mode. |
| `tag` | string | unset | Literal tag. Overridable at deploy via `--tag <name>=<value>`. |
| `build` | [Build](#build) | unset | Local-build config. Required for `--build` flow. |
| `hosts` | list of string | all hosts | Restrict which hosts run this service. Strings match `hosts[].address`. |
| `depends_on` | list of string | `[]` | Other service names. Topological-sort drives wave ordering. Must be a DAG. |
| `networks` | list of string | `[<deploy.network default>]` | Docker networks to attach. Must be declared in `deploy.networks`. |
| `secrets` | list of string | `[]` | Secret names exposed as env vars of the same name. |
| `env_from_secrets` | map of string | `{}` | `ENV_VAR_NAME: SECRET_NAME` — exposes a secret under a different env name. |
| `env` | map of string | `{}` | Plain env vars. **Don't put secrets here** — they end up in container labels. |
| `labels` | map of string | `{}` | Extra docker labels. |
| `description` | string | unset | Written to the container as `org.opencontainers.image.description`; surfaced in the TUI. |
| `kind` | enum | unset | Internal marker used by yoink-synthesized services (the proxy). Operators don't set this. |
| `pre_deploy` | list of [Hook](#hook) | `[]` | One-shot hooks that run before the swap (per service-wave; see [Hook](#hook) for the once-per-up vs once-per-wave distinction). |
| `domain` | string \| list of string | unset | Hostname(s) for the bundled proxy to route. Presence enables proxying. List form is the apex+www / multi-domain pattern. See the [proxy guide](/docs/guide/proxy). |
| `path_prefix` | string | unset | Match this path glob in addition to `domain:` so multiple services can share a hostname. Yoink orders routes so path-constrained ones come before catch-alls. |
| `tls` | enum | `auto` (when `domain:` set) | `auto` = ACME via Let's Encrypt; `off` = HTTP only; `cert` = inline cert from sealed secrets (set `tls_cert_secret:` / `tls_key_secret:`). |
| `tls_cert_secret` | string | unset | Sealed-secret name holding the PEM cert. Required when `tls: cert`. |
| `tls_key_secret` | string | unset | Sealed-secret name holding the PEM private key. Required when `tls: cert`. |
| `upstream_h2c` | bool | `false` | Talk to backend over HTTP/2 cleartext. Required for native gRPC backends (Tonic, grpc-go, grpc-java). |
| `compression` | bool | `false` | Emit `encode gzip zstd`. No-op behind a CDN. |
| `canonical_domain` | string | unset | One of the `domain:` entries. Yoink 308-redirects every other entry to it (apex/www patterns). |
| `hsts` | bool | `true` for TLS sites | Emit `Strict-Transport-Security: max-age=31536000; includeSubDomains`. |
| `caddy_extra_json` | string (JSON) | unset | Raw Caddy handler JSON merged into the route. Routes auto-wrapped in `subroute`. See the [snippets cookbook](/docs/guide/proxy#snippets-cookbook). |
| `caddy_extra_caddyfile` | string (Caddyfile) | unset | Same as above but in Caddyfile syntax. Yoink shells out to `caddy adapt` at render time (needs docker on operator). Mutually exclusive with `caddy_extra_json:`. |
| `run` | [Run](#run) | required | Runtime config (port, replicas, healthcheck, options). |

## Build

| Field | Type | Default | Notes |
|---|---|---|---|
| `context` | string | `.` | Build context, relative to the config file. |
| `dockerfile` | string | `Dockerfile` | Dockerfile path, relative to the context. |
| `target` | string | unset | Multi-stage build target. |
| `args` | map of string | `{}` | `--build-arg KEY=VALUE` pairs. |
| `extra_args` | list of string | `[]` | Raw extra args passed to the build command. |

## Run

| Field | Type | Default | Notes |
|---|---|---|---|
| `port` | u16 | required | Container port to healthcheck and route to. |
| `healthcheck_path` | string | unset | HTTP path. If unset, yoink does a TCP-connect probe instead. |
| `healthcheck_timeout` | duration | `60s` | How long to wait for a healthy response on the new container. |
| `replicas` | u32 | `1` | Number of containers per host. ≥2 enables rolling swap. |
| `drain_timeout` | duration | `30s` | Grace period before SIGKILL on the old container. |
| `entrypoint` | list of string | image default | Override `ENTRYPOINT`. |
| `cmd` | list of string | image default | Override `CMD`. |
| `publish` | list of string | `[]` | Host port mappings (`"80:80"`, `"443:443/udp"`). |
| `binds` | list of string | `[]` | Host bind mounts (`"src:dst:mode"`). Default mode is `:ro`. |
| `volumes` | list of string | `[]` | Named docker volumes. |
| `files` | list of string | `[]` | Same shape as `binds`, but the source content is hashed into `spec_hash` (config files trigger a redeploy on edit). |
| `options` | [RunOptions](#runoptions) | hardened defaults | Resource limits + security flags. |

## RunOptions

The hardened defaults make new containers prod-safe out of the box. Override per-service when needed.

| Field | Type | Default | Notes |
|---|---|---|---|
| `memory` | string | unset | Memory limit (`"512Mi"`, `"1Gi"`). |
| `cpus` | string | unset | CPU limit (`"1.5"` = 1.5 cores; `"2"` = 2 dedicated cores). |
| `pids_limit` | u32 | `1024` | Fork-bomb bound. |
| `cap_drop` | list of string | `["ALL"]` | Linux capabilities dropped. |
| `cap_add` | list of string | `[]` | Capabilities re-added. Use sparingly (e.g. `NET_BIND_SERVICE` for Caddy). |
| `security_opt` | list of string | `["no-new-privileges:true"]` | Docker security options. |
| `read_only` | bool | `true` | Read-only rootfs. Combine with `tmpfs:` for writable scratch. |
| `init` | bool | `true` | Run with tini as PID 1 (zombie reaping + proper SIGTERM). |
| `tmpfs` | map of string | `{}` | `mount_path: "size=N,mode=NNNN"`. Auto-applies `noexec,nosuid,nodev`. |
| `restart` | string | unset (docker default `no`) | Docker restart policy. Common values: `unless-stopped` (recommended for long-running services), `on-failure`, `always`. Set explicitly; yoink doesn't impose a default. |
| `user` | string | `"65534:65534"` (nobody) | UID/GID. Default runs non-root. Override with `"0:0"` for images that genuinely need root, or a specific uid:gid (`"1000:1000"`, `"redis"`) when the image has pre-baked file ownership. |
| `network_aliases` | list of string | `[]` | Extra DNS aliases on the attached networks. The container always gets the service `name` as an alias regardless; this field adds *more* names (e.g. for legacy hostname compat). |
| `devices` | list of string | `[]` | Host devices to expose. Each entry is `<host-path>[:<container-path>[:<perms>]]` (docker's `--device` syntax). `<perms>` is some combination of `r`, `w`, `m`; defaults to `rwm`. Both bind-mounts the device file and adds it to the cgroup `devices.allow` list. Narrower than `--privileged` — only the listed devices become accessible. Common uses: `/dev/nvidia*` (GPU), `/dev/dri` (Intel/AMD VAAPI), `/dev/ttyUSB*` (USB serial), `/dev/fuse` (with `cap_add: [SYS_ADMIN]`), `/dev/snd` (audio). |

See [Security defaults](/docs/guide/security) for the full picture.

## Hooks

Two places hooks live:

- **Per-service** `services[].pre_deploy:` runs before *that service's* wave starts (so a database migration finishes before the api container that depends on it is even pulled). Once per `up`, on the first host that has the service.
- **Top-level** `hooks.pre_deploy:` runs before *any* service-wave starts. Once per `up`, on the first host overall. Use for cluster-wide concerns that don't belong to a single service.

```yaml
hooks:
  pre_deploy:
    - name: cluster-bootstrap
      image: my/bootstrap-tool
      tag: v1
```

Both shapes use the same [Hook](#hook) entry shape below. A non-zero exit aborts the deploy before the swap; the old container stays live.

## Hook

A hook runs in one of two flavors, distinguished by the presence of `image:`:

- **Container hook** (`image:` + `tag:` set): yoink pulls the image and runs `cmd:` inside docker on the first host. Use for pinned tool images or isolation from the operator's filesystem.
- **Subprocess hook** (no `image:`, no `tag:`): yoink invokes `cmd[0]` directly on the operator's machine (or the CI runner). Inherits the parent env. Use for tools already on PATH (`terraform`, `dbmate`, `alembic`, `gcloud`, …) — see [Run Terraform from a hook](/docs/how-to/terraform-from-hooks).

| Field | Type | Default | Notes |
|---|---|---|---|
| `name` | string | required | Hook identifier (used in logs, lock label). |
| `image` | string | unset | Container hook only. Usually the same as the runtime service. |
| `tag` | string \| `{service: <name>}` | unset | Container hook only. Literal tag, or `{service: api}` to mirror the runtime tag. Required when `image:` is set; rejected otherwise. |
| `entrypoint` | list of string | image default | Container hook only. Override `ENTRYPOINT`. |
| `cmd` | list of string | image default | Container: override `CMD`. Subprocess: required; `cmd[0]` is the binary, the rest are args. |
| `working_dir` | string (path) | yoink.yaml's dir | Subprocess hook only. Resolved relative to `yoink.yaml`'s directory. |
| `env` | map of string | `{}` | Plain env vars. |
| `secrets` | list of string | `[]` | Secret names exposed as env vars of the same name. Often a different role than the runtime (e.g. a migrate role). |
| `env_from_secrets` | map of string | `{}` | `ENV_NAME: SECRET_NAME` mapping. |
| `secrets_profile` | string | unset | Reference a named recipe from `secrets.profiles`. Profile's `include` joins `secrets`; profile's `rename` joins `env_from_secrets`. Profile's `unset:` applies on subprocess hooks (via `Command::env_remove`); rejected on container hooks (no parent env to clear). See [Secrets profiles](#secrets-profiles). |

## Webhooks

Outbound HTTP calls fired on `RunStarted` / `RunFinished`. Operator-side: the request goes from the box running `yoink up`, not from each host. Best-effort: every fire records a `WebhookFired` audit event whose `ok` field carries the outcome; a failure never aborts the deploy. See [Notify chat / paging / observability on deploy](/docs/how-to/notify-on-deploy) for templating, secrets, and worked receiver examples (ntfy, Slack, Grafana).

```yaml
webhooks:
  - name: slack-deploys
    url: "https://hooks.slack.com/${secret:SLACK_WEBHOOK_PATH}"
    on: [run_succeeded, run_failed]
    headers:
      Content-Type: application/json
    body: '{ "text": "{{ command }} {{ services | join(\", \") }}{% if error %} — {{ error }}{% endif %}" }'
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `name` | string | required | Unique across the `webhooks:` list. Appears in `WebhookFired.name`. |
| `url` | string | required | Templated. Must resolve to `http(s)://…` after `${secret:NAME}` substitution. |
| `on` | list of trigger | required | Subset of `run_started`, `run_succeeded`, `run_failed`. At least one entry required. |
| `method` | `GET` \| `POST` \| `PUT` | `POST` |  |
| `timeout` | duration | `10s` | `humantime` syntax. |
| `headers` | map of string | `{}` | Each value templated. |
| `body` | string | unset | Templated. Omit for GET-style receivers. |

Templating runs in two passes per field: `${secret:NAME}` literal substitution from the sealed bundle, then minijinja rendering with `UndefinedBehavior::Strict`. Available template variables: `event`, `ok`, `command`, `services`, `deploy_id`, `actor`, `host`, `git_sha`, `yoink_version`, `error`, `ts`. See [Notify chat / paging / observability on deploy: Template context](/docs/how-to/notify-on-deploy#template-context) for the full table.

## Tag overrides at deploy time

Any service or hook with `tag:` unset (or set to a literal that you want to override) accepts `--tag` at the CLI:

```sh
yoink up --tag api=$(git rev-parse HEAD) --tag web=$(git rev-parse HEAD)
```

`tag: { service: api }` mirrors the runtime tag, useful for `pre_deploy` migrations that must use the exact image being deployed.

## Includes

`include:` globs are resolved relative to the config file's directory. Each matched file is parsed as a partial config; only `services:` and `hooks.pre_deploy:` are permitted in fragments and they merge into the entry config's lists. All other top-level fields (`hosts`, `deploy`, `secrets`, `registry`, `proxy`, `slug`) belong only in the entry config.

```yaml
# yoink.prod.yaml
hosts: [...]
deploy: { networks: [...] }
include:
  - services/prod/*.yaml
```

```yaml
# services/prod/api.yaml
services:
  - name: api
    image: ghcr.io/you/api
    # ...
```

## Environment-variable substitution

Any `${VAR}` reference in `yoink.yaml` (or in a fragment loaded via `include:`) is substituted against the process environment at config-load time, before YAML parsing.

```yaml
hosts:
  - address: ${HOST_IP}
    user: root
services:
  - name: hello
    domain:
      - ${HOST_IP}.nip.io
```

Run with `HOST_IP=1.2.3.4 yoink up …`. Useful for throwaway-host recipes, ephemeral preview environments, and any other case where one of the values isn't known until invocation time.

Rules:

- Only the unambiguous `${NAME}` form is recognized. Bare `$NAME` and lone `$` characters pass through unchanged so values containing literal dollar signs are unaffected.
- `NAME` must match `[A-Za-z_][A-Za-z0-9_]*`. Anything else (including `${HOST-IP}`, `${1BAD}`, or unterminated `${`) errors with a snippet pointing at the bad reference.
- Missing variables are a hard error rather than silent empty substitution; a quietly-empty `address:` produces baffling failures further down the deploy.
- The POSIX `${NAME:-default}` form supplies a fallback when `NAME` is unset. Useful for commands that parse the full config but don't actually use the value (e.g. `yoink secrets seal` against a config whose hosts/domains reference `${HOST_IP}`). Set the var explicitly when you do mean to use it.
- For long-lived secrets, prefer `secrets:` (sealed or `provider: command`) over passing values via env. The substitution path is intended for routing parameters (host IPs, hostnames, port numbers), not credentials.

## See also

- [CLI reference](/docs/reference/cli) — every subcommand and flag.
- [Examples](/docs/examples) — three complete annotated `yoink.yaml`s (hobby, polyglot, production).
- [Secrets](/docs/guide/secrets) — the mental model behind the `secrets:` block.
- [Reverse proxy](/docs/guide/proxy) — the schema and behavior of the `proxy:` block.
