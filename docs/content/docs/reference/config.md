---
title: Configuration
weight: 5
---

Full schema for `yoink.yaml`. Canonical source: [`yoink/src/config.rs`](https://github.com/oddur/yoink/blob/main/yoink/src/config.rs).

## Top level

| Field | Type | Default | Notes |
|---|---|---|---|
| `slug` | string (optional) | unset | Banner shown in the TUI's top chrome — use to mark a config (e.g. `"PRODUCTION — TREAD CAREFULLY"`) so the operator can't miss which environment they're pointed at. Keep it short (single line). |
| `hosts` | list of [Host](#host) | required | One or more deploy targets. |
| `deploy` | [Deploy](#deploy) | `{}` | Cross-service defaults (networks, etc.). |
| `secrets` | [Secrets](#secrets) | unset | Secret provider config. Required if any service uses `secrets:` / `env_from_secrets:`. |
| `registry` | [Registry](#registry) | unset | Image-pull credentials. Skip for `--no-registry` or public images. |
| `services` | list of [Service](#service) | `[]` | Services to deploy. Can be defined inline or split via `include:`. |
| `include` | list of glob | `[]` | Glob paths (relative to the config file) merged into `services`. |

## Host

| Field | Type | Notes |
|---|---|---|
| `address` | string | Hostname or IP. Anything your local SSH client accepts: a raw IP, a DNS name, a `~/.ssh/config` alias, or a tailnet hostname. |
| `user` | string | SSH user. Must be in the `docker` group on the host (or be `root`). |
| `ssh_key_secret` | string (optional) | Name of an entry in your sealed-secrets bundle holding a PEM-formatted SSH private key. When set, yoink decrypts the key into a per-process tempfile (mode `0o600`) and uses it for this host's SSH connections — both bollard's daemon connection and the pre-flight `ssh_probe`. Lets you ship the deploy key with the repo (encrypted at rest in `secrets.age`) instead of relying on every operator's personal `ssh-agent`. |

```yaml
hosts:
  - { address: prod-eu-1, user: deploy }
  - { address: prod-us-1, user: deploy }

  # Deploy-key-in-repo flavour (for fresh VPS hosts without
  # ssh-agent already configured for them):
  - address: 1.2.3.4
    user: root
    ssh_key_secret: PROD_HOST_SSH_KEY
```

## Deploy

| Field | Type | Default | Notes |
|---|---|---|---|
| `networks` | list of string | `[]` | Declared docker networks. Services attach via `services[].networks:`. Yoink creates each one if missing. |

## Secrets

Two providers, selected by the `provider:` tag.

### `provider: age` (default, batteries-included)

A single sealed dotenv file committed alongside `yoink.yaml`, decrypted at deploy time with the operator's age **identity** (private half) resolved in priority order:

1. `YOINK_AGE_KEY` env (raw key) — CI / managed-env contexts
2. `YOINK_AGE_KEY_FILE` env (path) — explicit override
3. `~/.config/yoink/keys/*.key` — laptop default; yoink scans the dir and picks whichever key's public half matches one of `recipients:` below
4. `~/.config/yoink/age.key` — legacy single-key fallback

The keys-dir scan (#3) is what makes `yoink secrets key generate` work without per-project env-var setup — multiple projects with distinct identities coexist there, picked by recipient match.

| Field | Type | Default | Notes |
|---|---|---|---|
| `provider` | string | required | `age` |
| `recipients` | list of string | `[]` | Public age **recipients** (`age1...`), one per principal that needs to decrypt. New writes are sealed against every entry; decryption only needs *one* matching identity. Validates non-empty at config-load. |
| `file` | string | `secrets.age` | Sealed file path, relative to the config file's directory. `..` and absolute paths are rejected. |

The recipient/identity split is the asymmetric-keypair mental model — see [Two halves of one key](/docs/recipes/sealed-secrets#two-halves-of-one-key) in the recipe for the full explanation.

Bootstrap: `yoink secrets key generate` writes a fresh **identity** to `~/.config/yoink/keys/<recipient>.key` (mode 0600) by default and prints the matching **recipient** (`age1…`, public; paste into `recipients:` above). `--out PATH` writes to a specific file at mode 0600 instead. `--print` sends the secret to stdout for piping into a CI secret store (`… --print | gh secret set YOINK_AGE_KEY`). `yoink secrets key public` re-derives the recipient from whichever identity yoink would use right now (handy "is the key in my shell the same one yoink.yaml expects?" check). See the [sealed-secrets recipe](/docs/recipes/sealed-secrets).

### `provider: command` (bring-your-own-tool)

Yoink invokes the configured command and reads a secrets bundle from stdout. Format auto-detects between dotenv and JSON. Lets operators wire any external manager (Doppler, 1Password, HashiCorp Vault, AWS Secrets Manager, the Infisical CLI, Bitwarden, …) without yoink growing first-party integrations for each.

| Field | Type | Default | Notes |
|---|---|---|---|
| `provider` | string | required | `command` |
| `command` | list of string | required | Argv to spawn. First element is the binary, the rest are arguments. No shell interpretation — wrap in `["sh", "-c", "..."]` if you need pipes. |
| `format` | string | `auto` | `auto` inspects the first non-whitespace byte (`{` → JSON, else dotenv). `dotenv` / `json` force the parser. |

```yaml
secrets:
  provider: command
  command: ["doppler", "secrets", "download", "--no-file", "--format", "env"]
```

See the [external-secrets recipe](/docs/recipes/secrets-external-cli) for per-tool wiring.

## Registry

| Field | Type | Notes |
|---|---|---|
| `server` | string | Registry hostname (e.g. `ghcr.io`). |
| `username_secret` | string | Name of the secret in your provider holding the username. |
| `password_secret` | string | Name of the secret holding the token/password. |

Yoink resolves these at deploy time and passes them as `X-Registry-Auth` on every pull.

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
| `pre_deploy` | list of [Hook](#hook) | `[]` | One-shot hooks that run once per `up`, before the swap. |
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
| `restart` | string | `unless-stopped` | Docker restart policy. |
| `user` | string | `"65534:65534"` (nobody) | UID/GID. Default runs non-root. Override with `"0:0"` for images that genuinely need root, or a specific uid:gid (`"1000:1000"`, `"redis"`) when the image has pre-baked file ownership. |
| `network_aliases` | list of string | `[name]` | Extra DNS names on the attached networks. |

See [Security defaults](/docs/guide/security-defaults) for the full picture.

## Hook

`pre_deploy` hook entries. Run once per `up`, on the first host that has the service, before the runtime swap.

| Field | Type | Default | Notes |
|---|---|---|---|
| `name` | string | required | Hook identifier (used in logs, lock label). |
| `image` | string | required | Image reference. Usually the same as the runtime service. |
| `tag` | string \| `{service: <name>}` | unset | Literal tag, or `{service: api}` to mirror the runtime tag exactly. |
| `entrypoint` | list of string | image default | Override `ENTRYPOINT`. |
| `cmd` | list of string | image default | Override `CMD`. |
| `env` | map of string | `{}` | Plain env vars. |
| `secrets` | list of string | `[]` | Secret names exposed as env vars of the same name. Often a different role than the runtime (e.g. a migrate role). |
| `env_from_secrets` | map of string | `{}` | `ENV_NAME: SECRET_NAME` mapping. |

A non-zero exit aborts the deploy before the swap. The old container stays live.

## Tag overrides at deploy time

Any service or hook with `tag:` unset (or set to a literal that you want to override) accepts `--tag` at the CLI:

```sh
yoink up --tag api=$(git rev-parse HEAD) --tag web=$(git rev-parse HEAD)
```

`tag: { service: api }` mirrors the runtime tag — useful for `pre_deploy` migrations that must use the exact image being deployed.

## Includes

`include:` globs are resolved relative to the config file's directory. Each matched file is parsed as a partial config and merged into `services:`. Top-level fields (`hosts`, `deploy`, `secrets`, `registry`) belong only in the entry config.

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
