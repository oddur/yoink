---
title: Configuration
description: The complete yoink.yaml schema with every field, its type, and its default.
weight: 5
---

The canonical source of the `yoink.yaml` schema is [`yoink/src/config.rs`](https://github.com/oddur/yoink/blob/main/yoink/src/config.rs).

## Top level

| Field | Type | Default | Notes |
|---|---|---|---|
| `slug` | string | unset | One short line that the TUI shows as a banner in its top chrome. It marks the environment, for example `"PRODUCTION — TREAD CAREFULLY"`. |
| `hosts` | list of [Host](#host) | `[]` | Deploy targets. Config load accepts an empty list. A command that connects to a host needs a minimum of one entry. |
| `deploy` | [Deploy](#deploy) | `networks: [yoink]` | Defaults that apply across services (networks). |
| `secrets` | [Secrets](#secrets) | unset | Secret provider config. Required if a service uses `secrets:` or `env_from_secrets:`. |
| `registry` | [Registry](#registry) | unset | Image-pull credentials. Not necessary when yoink builds every service locally, or when all pulls come from public registries. |
| `proxy` | [Proxy](#proxy) | unset | Bundled Caddy reverse proxy. A service with `domain:` enables it. |
| `services` | list of [Service](#service) | `[]` | Services to deploy, inline or in fragments that `include:` loads. |
| `hooks` | [Hooks](#hooks) | `{}` | Top-level hooks that do not belong to one service. Only `pre_deploy:` exists. |
| `webhooks` | list of [Webhook](#webhooks) | `[]` | Outbound HTTP notifications on run-level events. The computer that runs yoink sends them, best-effort. |
| `include` | list of glob | `[]` | Glob paths, relative to the config file, of the fragments to merge. See [Includes](#includes). |

## Host

Each host has exactly one of `address:` and `address_secret:`. Config load rejects a host that has neither field or both fields.

| Field | Type | Notes |
|---|---|---|
| `address` | string | Hostname or IP. Any value that your local SSH client accepts: a raw IP, a DNS name, a `~/.ssh/config` alias, or a tailnet hostname. |
| `address_secret` | string (optional) | Name of the sealed-secret entry that holds the address (IP or hostname). yoink resolves it at config-load time, after the bundle decrypts. It is for a sensitive address in a public repo, for example a public IP behind Cloudflare. |
| `user` | string | SSH user. Must be in the `docker` group on the host (or be `root`). |
| `ssh_key_secret` | string (optional) | Name of the sealed-secret entry that holds a PEM SSH private key. yoink decrypts the key into a per-process tempfile (mode `0o600`). yoink uses it for the docker daemon connection and the pre-flight ssh probe of this host. The deploy key can thus ship in the repo, encrypted in `secrets.age`, and your `ssh-agent` does not need it. |

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

`yoink hosts add --address-secret <KEY> --user <USER> --name <NAME>` writes a host fragment with a sealed address. `--name` is required, because no literal address exists to make the fragment filename from.

## Deploy

| Field | Type | Default | Notes |
|---|---|---|---|
| `networks` | list of string | `[yoink]` when `deploy:` is absent | Declared docker networks. Required when `deploy:` is present, with a minimum of one entry. Services attach to them with `services[].networks:`. yoink creates each network that does not exist. |

## Secrets

The `provider:` field selects one of two providers.

### `provider: age` (default, batteries-included)

One sealed dotenv file sits in the repo next to `yoink.yaml`. At deploy time, yoink decrypts the file with your age **identity** (the private half of the keypair). [Identity resolution](/docs/reference/secrets#identity-resolution) gives the order in which yoink looks for the identity.

| Field | Type | Default | Notes |
|---|---|---|---|
| `provider` | string | required | `age` |
| `recipients` | list of string | `[]` | Public age **recipients** (`age1...`), one for each principal that must decrypt. yoink seals new writes against every entry. Decryption needs only one matching identity. Config load rejects an empty list. |
| `file` | string | `secrets.age` | Path of the sealed file, relative to the directory of the config file. yoink rejects `..` and absolute paths. |
| `profiles` | map of [SecretsProfile](#secretsprofile) | `{}` | Named env-shape recipes for downstream consumers (Terraform state-backend creds, provider tokens, CI shell exports). `yoink secrets env --profile <NAME>` and `hooks.pre_deploy[].secrets_profile` use them. See [Secrets profiles](#secrets-profiles). |

[Sealed secrets (age)](/docs/explanation/secrets#sealed-secrets-age) explains the recipient and identity model.

`yoink secrets key generate` writes a new identity to `~/.config/yoink/keys/<recipient>.key` (mode 0600). It prints the matching recipient (`age1…`), which goes into `recipients:`.

- `--out PATH` writes the identity to a specific file at mode 0600 instead.
- `--print` sends the identity to stdout, for a pipe into a CI secret store (`… --print | gh secret set YOINK_AGE_KEY`).

`yoink secrets key public` prints the recipient of the identity that yoink resolves. The output shows if the key in your shell is the key that `yoink.yaml` expects.

#### Secrets profiles

A profile names the env shape that one downstream tool expects. One profile drives a shell (Taskfile, CI workflow) and a yoink pre-deploy hook. The `bundle-key → env-var-name` mapping is thus in one place, next to the bundle.

##### `SecretsProfile`

| Field | Type | Default | Notes |
|---|---|---|---|
| `include` | list of string | `[]` | Allowlist of the bundle keys that this consumer uses. An empty list means every key in the bundle. If a listed key is not in the bundle, resolution stops with an error. |
| `rename` | map of string→string | `{}` | `bundle_key → env_var_name`. yoink applies the renames after `include`. Keys that are not in the map keep their name. Targets must match `[A-Za-z_][A-Za-z0-9_]*`. Lowercase is permitted for `TF_VAR_<varname>`. |
| `unset` | list of string | `[]` | Env vars that the importing shell clears before the exports arrive. For example, it clears a `B2_ENDPOINT` that a devbox `init_hook` leaked. Config load rejects a container hook that references a profile with `unset`, because a new container has no parent env to clear. |

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

The profile in a Taskfile:

```yaml
env:
  YOINK_TF:
    sh: cd ../../deploy-prod && yoink secrets env --profile terraform-cloudflare
tasks:
  tf:plan:
    cmds:
      - eval "$YOINK_TF" && terraform plan
```

The profile in a subprocess pre-deploy hook:

```yaml
hooks:
  pre_deploy:
    - name: cloudflare-tf-apply
      working_dir: ../terraform/cloudflare
      cmd: ["sh", "-c",
            "terraform init -input=false && terraform apply -auto-approve"]
      secrets_profile: terraform-cloudflare
```

On a subprocess hook, yoink calls `Command::env_remove` for each `unset:` entry of the profile. [Run Terraform from a hook](/docs/how-to/terraform-from-hooks) gives the full procedure.

When `GITHUB_ENV` is set, `yoink secrets env` puts one `echo '::add-mask::<value>'` line for each revealed value before the exports. A new key in the profile thus needs no mask line in the CI workflow.

### `provider: command` (bring-your-own-tool)

yoink runs the configured command and reads a secrets bundle (dotenv or JSON) from stdout. Any external manager with a CLI connects this way: Doppler, 1Password, HashiCorp Vault, AWS Secrets Manager, Infisical, Bitwarden.

| Field | Type | Default | Notes |
|---|---|---|---|
| `provider` | string | required | `command` |
| `command` | list of string | required | Argv to spawn. The first element is the binary. No shell interprets the command. Pipes need the form `["sh", "-c", "..."]`. |
| `format` | string | `auto` | `auto` looks at the first non-whitespace byte (`{` → JSON, else dotenv). `dotenv` and `json` force the parser. |

```yaml
secrets:
  provider: command
  command: ["doppler", "secrets", "download", "--no-file", "--format", "env"]
```

[`provider: command`](/docs/reference/secrets#provider-command) gives the run rules and the bundle formats. The [secrets guide](/docs/explanation/secrets) has the config for each tool.

## Registry

| Field | Type | Notes |
|---|---|---|
| `server` | string | Registry hostname (for example `ghcr.io`). |
| `username_secret` | string | Name of the secret in your provider that holds the username. |
| `password_secret` | string | Name of the secret that holds the token or password. |

yoink resolves these secrets at deploy time. It passes them as `X-Registry-Auth` on each pull of an image from `server`. A pull from another registry is anonymous. As a result, a public image such as the default proxy image `caddy:2` still pulls.

## Proxy

The [proxy guide](/docs/explanation/proxy) explains how the bundled Caddy reverse proxy operates.

| Field | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | `true` when a service has `domain:` | Forces the proxy on or off. For example, `true` when only `global_handlers:` configures the routes. |
| `email` | string | unset | Registration email for Let's Encrypt. **Required when a service uses `tls: auto`.** Not used when `proxy.tls:` is set. |
| `image` | string | `caddy:2` | Pre-built caddy image that yoink pulls from a registry. Mutually exclusive with `xcaddy:`. |
| `xcaddy` | [Xcaddy](#xcaddy) | unset | Compiles a custom caddy with plugins on each proxy host. Mutually exclusive with `image:`. |
| `cert_volume` | string | `yoink_caddy_data` | Named volume for ACME state and certs. If you remove the volume, Caddy requests new certs, and Let's Encrypt has strict rate limits. |
| `bind` | string | unset (all interfaces) | Host IP that `:80` and `:443` bind to. For example, a Tailscale IP makes the proxy reachable only over the tailnet. The admin port always stays on `127.0.0.1`. |
| `tls` | [ProxyTls](#proxytls) | unset | Proxy-level TLS. Every routed service inherits this cert (and the optional mTLS) by default. When the field is set, ACME is off. |
| `config_extra` | string (JSON) | unset | Top-level Caddy JSON object. yoink deep-merges it into the rendered config before `/load`. It is for global Caddy config that has no typed field: `trusted_proxies`, `storage`, plugin app blocks. |
| `global_handlers` | list of string (JSON) | `[]` | Caddy handlers or routes that run for every request before a per-service route matches. It is for proxy-wide concerns: CrowdSec bouncer, Coraza WAF, fleet-wide rate limiting. |
| `global_handlers_after` | list of string (JSON) | `[]` | Same shape as `global_handlers:`, but the chain runs after the per-service routes match. It is for concerns that need the upstream response (response headers, access-log shaping). |

### Xcaddy

`xcaddy` builds a custom caddy on each proxy host, with no registry. The local tag is `yoink-caddy:<hash>`, where the hash covers the build inputs. Later `up` runs skip the build until an input changes.

| Field | Type | Default | Notes |
|---|---|---|---|
| `plugins` | list of string | required (non-empty) | One entry for each caddy module: a bare module path or a pinned one (`module@version`), as in `xcaddy build --with`. yoink sorts the entries before it hashes and renders them. |
| `caddy_version` | string | xcaddy's latest tagged release | Caddy git tag to compile (for example `v2.8.4`). yoink passes the value verbatim to `xcaddy build`. |
| `base_image` | string | `caddy:2` | Runtime image. An override is necessary only to pin a specific caddy patch version. |
| `builder_image` | string | `caddy:2-builder` | Build stage (contains xcaddy and the Go toolchain). A pin to `caddy:<v>-builder` also pins the xcaddy CLI. |
| `env` | map of string | `{}` | Env vars for the build stage, set before `xcaddy build` runs. For example `GOPRIVATE` or `GOPROXY` for private Go modules. |
| `replace` | list of string | `[]` | yoink forwards these as `xcaddy build --replace` entries. For example, an entry builds a fork of a plugin. |

### ProxyTls

When `proxy.tls:` is set, every routed service uses this cert by default, and yoink emits a `:80 → :443` redirect. The per-service fields `tls_cert_secret:` and `tls_key_secret:` override the cert for one service.

| Field | Type | Default | Notes |
|---|---|---|---|
| `cert_secret` | string | required | Sealed-secret name that holds the PEM cert (full chain). |
| `key_secret` | string | required | Sealed-secret name that holds the PEM private key. |
| `client_auth` | [ClientAuth](#clientauth) | unset | Optional mTLS (for example Cloudflare origin-pull). |

### ClientAuth

| Field | Type | Default | Notes |
|---|---|---|---|
| `mode` | enum | `require_and_verify` | `request` / `require` / `verify_if_given` / `require_and_verify`. The default is the strictest mode. |
| `trust_pool_secret` | string | required | Sealed-secret name that holds the PEM CA bundle that signs the accepted client certs. |

## Service

| Field | Type | Default | Notes |
|---|---|---|---|
| `name` | string | required | The container name and the DNS alias. Unique in the config. |
| `image` | string | required | Image reference, without the tag. |
| `tag` | string | unset | Literal tag. See [Tag overrides at deploy time](#tag-overrides-at-deploy-time). |
| `build` | [Build](#build) | unset | Local-build config. Required for the `--build` flow. |
| `hosts` | list of string | all hosts | Restricts the hosts that run this service. The strings match `hosts[].address`. |
| `depends_on` | list of string | `[]` | Names of other services. A topological sort of these names gives the wave order. Must be a DAG. |
| `networks` | list of string | every network in `deploy.networks` | Docker networks to attach. Each one must be declared in `deploy.networks`. Config load rejects an empty list. |
| `secrets` | list of string | `[]` | Secret names. Each secret becomes an env var of the same name. |
| `env_from_secrets` | map of string | `{}` | `ENV_VAR_NAME: SECRET_NAME`. Exposes a secret under a different env name. |
| `env` | map of string | `{}` | Plain env vars. **Do not put secrets here.** |
| `labels` | map of string | `{}` | Extra docker labels. |
| `description` | string | unset | yoink writes it to the container as `org.opencontainers.image.description`. The TUI shows it. |
| `kind` | enum | unset | Internal marker for services that yoink synthesizes (the proxy). You do not set this field. |
| `pre_deploy` | list of [Hook](#hook) | `[]` | One-shot hooks that run before yoink starts the new containers of this service. See [Hooks](#hooks). |
| `domain` | string \| list of string | unset | One or more hostnames that the bundled proxy routes to this service. The field enables the proxy. Requires `run.port`. See the [proxy guide](/docs/explanation/proxy). |
| `path_prefix` | string | unset | Matches this path glob in addition to `domain:`, so that many services can share a hostname. yoink puts routes with a path constraint before the catch-all routes. |
| `tls` | enum | `auto` (when `domain:` set) | `auto` = ACME with Let's Encrypt. `off` = HTTP only. `cert` = inline cert from sealed secrets (with `tls_cert_secret:` and `tls_key_secret:`). |
| `tls_cert_secret` | string | unset | Sealed-secret name that holds the PEM cert. Required when `tls: cert`. |
| `tls_key_secret` | string | unset | Sealed-secret name that holds the PEM private key. Required when `tls: cert`. |
| `upstream_h2c` | bool | `false` | The proxy connects to the backend over HTTP/2 cleartext. Required for native gRPC backends (Tonic, grpc-go, grpc-java). |
| `compression` | bool | `false` | Emits `encode gzip zstd`. Has no effect behind a CDN. |
| `canonical_domain` | string | unset | One of the `domain:` entries. yoink sends a 308 redirect from every other entry to it (apex and www patterns). |
| `hsts` | bool | `true` for TLS sites | Emits `Strict-Transport-Security: max-age=31536000; includeSubDomains`. |
| `caddy_extra_json` | string (JSON) | unset | Raw Caddy handler JSON that yoink merges into the route. yoink wraps routes in `subroute`. See the [snippets cookbook](/docs/explanation/proxy#snippets-cookbook). |
| `caddy_extra_caddyfile` | string (Caddyfile) | unset | The same as `caddy_extra_json:`, but in Caddyfile syntax. yoink runs `caddy adapt` at render time, which needs docker on your computer. Mutually exclusive with `caddy_extra_json:`. |
| `run` | [Run](#run) | required | Runtime config (port, replicas, healthcheck, run options). |

## Build

| Field | Type | Default | Notes |
|---|---|---|---|
| `context` | string | `.` | Build context, relative to the config file. |
| `dockerfile` | string | `Dockerfile` | Dockerfile path, relative to the context. |
| `target` | string | unset | Multi-stage build target. |
| `args` | map of string | `{}` | `--build-arg KEY=VALUE` pairs. |
| `extra_args` | list of string | `[]` | Raw extra args that yoink passes to the build command. |

## Run

| Field | Type | Default | Notes |
|---|---|---|---|
| `port` | u16 | unset | Container port for the healthcheck and for proxy routes. Required when `healthcheck_path:` or `domain:` is set. Without a port, yoink skips the healthcheck. |
| `healthcheck_path` | string | unset | HTTP path for the probe. If the field is unset, yoink does a TCP-connect probe on `port`. |
| `healthcheck_timeout` | duration | `60s` | The time that yoink waits for a healthy response from the new container. |
| `replicas` | u32 | `1` | Number of containers on each host. A value ≥2 enables the rolling swap. |
| `drain_timeout` | duration | `15s` | Grace period before SIGKILL on the old container. |
| `entrypoint` | list of string | image default | Overrides `ENTRYPOINT`. |
| `cmd` | list of string | image default | Overrides `CMD`. |
| `publish` | list of string | `[]` | Host port mappings (`"80:80"`, `"443:443/udp"`). |
| `binds` | list of string | `[]` | Host bind mounts (`"src:dst:mode"`). The default mode is `:ro`. |
| `volumes` | list of string | `[]` | Named docker volumes. |
| `files` | list of string | `[]` | Files that yoink uploads to the host and mounts read-only (`"local_path:container_path[:ro]"`). `:ro` is optional. Validation rejects `:rw`. yoink hashes the content into `spec_hash`, so an edit triggers a redeploy. For data that the container must write, use `binds` or `volumes`. |
| `options` | [RunOptions](#runoptions) | hardened defaults | Resource limits and security flags. |

## RunOptions

The defaults are a hardened profile. Each service can override them.

| Field | Type | Default | Notes |
|---|---|---|---|
| `memory` | string | unset | Memory limit (`"512Mi"`, `"1Gi"`). |
| `cpus` | string | unset | CPU limit in cores of CPU time (`"1.5"` = 1.5 cores). |
| `pids_limit` | integer | `1024` | Limit against fork bombs. |
| `cap_drop` | list of string | `["ALL"]` | Linux capabilities that the container loses. |
| `cap_add` | list of string | `[]` | Capabilities that the container gets back, for example `NET_BIND_SERVICE` for Caddy. |
| `security_opt` | list of string | `["no-new-privileges:true"]` | Docker security options. |
| `read_only` | bool | `true` | Read-only rootfs. `tmpfs:` adds writable scratch space. |
| `init` | bool | `true` | Runs tini as PID 1 (zombie reaping and correct SIGTERM). |
| `tmpfs` | map of string | `{}` | `mount_path: "size=N,mode=NNNN"`. yoink applies `noexec,nosuid,nodev` automatically. |
| `restart` | enum | `unless-stopped` | Docker restart policy: `no`, `always`, `unless-stopped`, or `on-failure`. Write `'no'` with quotes, because YAML reads a bare `no` as a boolean. |
| `user` | string | `"65534:65534"` (nobody) | UID/GID. The default is non-root. `"0:0"` is for images that need root. A specific value (`"1000:1000"`, `"redis"`) is for images with pre-baked file ownership. |
| `network_aliases` | list of string | `[]` | Extra DNS aliases on the attached networks, in addition to the service `name`. For example, a legacy hostname. |
| `devices` | list of string | `[]` | Host devices to expose. Each entry is `<host-path>[:<container-path>[:<perms>]]` (the `--device` syntax of docker). `<perms>` is a combination of `r`, `w`, `m`, and the default is `rwm`. An entry bind-mounts the device file and adds it to the cgroup `devices.allow` list. Only the listed devices become accessible, which is narrower than `--privileged`. Examples: `/dev/nvidia*` (GPU), `/dev/dri` (Intel/AMD VAAPI), `/dev/ttyUSB*` (USB serial), `/dev/fuse` (with `cap_add: [SYS_ADMIN]`), `/dev/snd` (audio). |

[Security defaults](/docs/explanation/security) explains the reasons for these defaults.

## Hooks

Hooks are in two places. Each hook runs once for each `up`.

| Place | When it runs |
|---|---|
| Top-level `hooks.pre_deploy:` | Before any service wave starts. It is for cluster-wide concerns that do not belong to one service. |
| Per-service `services[].pre_deploy:` | After yoink pulls the image and creates the new containers of that service, before yoink starts them. A database migration thus completes before the new api container starts. |

```yaml
hooks:
  pre_deploy:
    - name: cluster-bootstrap
      image: my/bootstrap-tool
      tag: v1
```

The two places use the same [Hook](#hook) entry shape. A non-zero exit stops the deploy before the swap, and the old container stays live.

## Hook

The presence of `image:` selects one of two flavors:

- **Container hook** (`image:` and `tag:` set): yoink pulls the image and runs `cmd:` in docker on the first host in `hosts:`. It is for pinned tool images, or for isolation from your filesystem.
- **Subprocess hook** (no `image:`, no `tag:`): yoink runs `cmd[0]` directly on your computer (or on the CI runner). The hook inherits the parent env. It is for tools that are already on PATH, for example `terraform`, `dbmate`, `alembic`, `gcloud`. See [Run Terraform from a hook](/docs/how-to/terraform-from-hooks).

| Field | Type | Default | Notes |
|---|---|---|---|
| `name` | string | required | Hook identifier. It appears in logs and in the name of the hook container. |
| `image` | string | unset | Container hook only. Usually the same image as the runtime service. |
| `tag` | string \| `{service: <name>}` | unset | Container hook only. A literal tag, or `{service: api}` to use the tag that yoink resolves for the service `api`. Required when `image:` is set. Rejected when `image:` is not set. |
| `entrypoint` | list of string | image default | Container hook only. Overrides `ENTRYPOINT`. |
| `cmd` | list of string | image default | Container: overrides `CMD`. Subprocess: required. `cmd[0]` is the binary and the other elements are args. |
| `working_dir` | string (path) | directory of `yoink.yaml` | Subprocess hook only. yoink resolves it relative to the directory of `yoink.yaml`. |
| `env` | map of string | `{}` | Plain env vars. |
| `secrets` | list of string | `[]` | Secret names. Each secret becomes an env var of the same name. |
| `env_from_secrets` | map of string | `{}` | `ENV_NAME: SECRET_NAME` mapping. |
| `secrets_profile` | string | unset | Name of a profile in `secrets.profiles`, for `hooks.pre_deploy` entries. The `include` of the profile joins `secrets`, and its `rename` joins `env_from_secrets`. `unset:` applies only on subprocess hooks. See [Secrets profiles](#secrets-profiles). |

## Webhooks

yoink sends webhooks on `RunStarted` and `RunFinished`, from the computer that runs `yoink up`, not from each host. Webhooks are best-effort: a failure never stops the deploy. Each call records a `WebhookFired` audit event, and its `ok` field holds the outcome. [Notify chat / paging / observability on deploy](/docs/how-to/notify-on-deploy) has receiver examples (ntfy, Slack, Grafana).

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
| `name` | string | required | Unique in the `webhooks:` list. Appears in `WebhookFired.name`. |
| `url` | string | required | Templated. Must resolve to `http(s)://…` after the `${secret:NAME}` substitution. |
| `on` | list of trigger | required | Subset of `run_started`, `run_succeeded`, `run_failed`. A minimum of one entry is required. |
| `method` | `GET` \| `POST` \| `PUT` | `POST` |  |
| `timeout` | duration | `10s` | `humantime` syntax. |
| `headers` | map of string | `{}` | Each value is templated. |
| `body` | string | unset | Templated. Not necessary for GET-style receivers. |

Templating runs in two passes for each field:

1. The literal `${secret:NAME}` substitution from the sealed bundle.
2. minijinja rendering with `UndefinedBehavior::Strict`.

The template variables are `event`, `ok`, `command`, `services`, `deploy_id`, `actor`, `host`, `git_sha`, `yoink_version`, `error`, `ts`. [Webhooks: Template context](/docs/reference/webhooks#template-context) describes each one.

## Tag overrides at deploy time

`yoink up` resolves the tag of each selected service in this order:

1. `--tag <name>=<value>`.
2. A bare `--tag <value>`, which applies to every selected service.
3. `tag:` in the service config.
4. With `--service` and no `--tag`: the git short SHA, for each selected service that has no `tag:`. This rule needs a clean working tree and does not apply with `--allow-dirty`.

`--here` applies the git short SHA to every selected service, also over a config `tag:`. `--here` does not look for uncommitted changes.

```sh
yoink up --tag api=$(git rev-parse HEAD) --tag web=$(git rev-parse HEAD)
```

On a hook, `tag: { service: api }` resolves to the tag that yoink deploys for the service `api`. A `pre_deploy` migration thus uses the exact image that yoink deploys.

## Includes

yoink resolves `include:` globs relative to the directory of the config file and parses each matched file as a fragment. A fragment can contain only `services:`, `hosts:`, and `hooks.pre_deploy:`. yoink appends these lists to the lists of the entry config. All other top-level fields (`deploy`, `secrets`, `registry`, `proxy`, `slug`) belong only in the entry config.

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

At config-load time, before YAML parsing, yoink substitutes each `${VAR}` reference against the process environment. This applies to `yoink.yaml` and to each fragment that `include:` loads.

```yaml
hosts:
  - address: ${HOST_IP}
    user: root
services:
  - name: hello
    domain:
      - ${HOST_IP}.nip.io
```

The command for this config is `HOST_IP=1.2.3.4 yoink up …`. Substitution is for values that are not known before the run, for example the IP of a throwaway host or of a preview environment.

Rules:

- yoink recognizes only the `${NAME}` form. Bare `$NAME` and lone `$` characters pass through unchanged.
- `NAME` must match `[A-Za-z_][A-Za-z0-9_]*`. Any other reference (`${HOST-IP}`, `${1BAD}`, an unterminated `${`) gives an error with a snippet of the bad reference.
- A missing variable is an error, not an empty substitution.
- The POSIX `${NAME:-default}` form supplies a fallback when `NAME` is unset. It is for commands that parse the full config but do not use the value. An example is `yoink secrets seal` against a config that references `${HOST_IP}`.
- Substitution is for routing parameters (host IPs, hostnames, port numbers). Credentials belong in `secrets:`.

## See also

- [CLI reference](/docs/reference/cli): every subcommand and flag.
- [Examples](/docs/reference/examples): three complete annotated `yoink.yaml` files (hobby, polyglot, production).
- [Glossary](/docs/reference/glossary): definitions of the terms on this page.
- [Secrets resolution and bundle formats](/docs/reference/secrets): identity lookup order and the rules for `provider: command`.
