---
title: Deploy mechanics
description: The inputs of spec_hash, the deploy lock parameters, the secrets lookup order, the prune rules, and the container host config that yoink builds.
---

## `spec_hash` inputs

The label `yoink.spec_hash` holds the first 16 hex characters of the `sha256` of these values. The container name uses the first 8.

| Input | Detail |
|---|---|
| `image` and `tag` | Resolved, with your `--tag` overrides applied. |
| env map | Literals, and every `secrets:` value as resolved at deploy time. |
| networks | The network aliases and the set of attached networks. |
| labels | Your `labels:`, the `description`, and the proxy fields of the service (`domain`, `tls`, the TLS secret names, a hash of `caddy_extra_json`). |
| `binds` | As declared, before the automatic `:ro` rewrite. |
| `volumes` | As declared. |
| `tmpfs` | As declared, before the automatic `noexec,nosuid,nodev` rewrite. |
| run options | `memory`, `cpus`, `pids_limit`, `cap_drop`, `cap_add`, `security_opt`, `devices`, `read_only`, `init`, `user`, `restart`. |
| ports | The `publish` strings. |
| `entrypoint` and `cmd` | As declared. |
| `files:` mounts | The **content hash** of every mount. |

## Values not in `spec_hash`

| Value | Reason |
|---|---|
| Container name | It contains the short hash, so it changes on each deploy. |
| `yoink.deployed-by` and `yoink.deployed-at` labels | They are audit labels. |
| Host address | The same spec runs on every applicable host. |

## Deploy lock

| Property | Value |
|---|---|
| Scope | One lock for each host. |
| Container name | `yoink-deploy-lock` |
| Image | `alpine:3.20` |
| Heartbeat | Every 5 seconds, the operator side runs `docker exec` in the sentinel to touch `/tmp/heartbeat`. |
| Expiry | The sentinel exits after approximately 30 seconds without a heartbeat. |
| Sentinel runs on the host | `up` stops with the error "another deploy in progress." |
| Sentinel exists but is stopped | yoink removes it and starts a new one. |
| End of `up` (success or error) | yoink stops the heartbeat and force-removes the sentinel. |
| Manual release | `yoink lock release --yes` |

### Log lines

| Log line | Meaning |
|---|---|
| `lock heartbeat exec failed host=… error=…container is not running` | The heartbeat of your local yoink tried to touch a sentinel that a different operator removed. Your lock is gone. Close the session and open it again. |

## `age` identity lookup order

The `age` provider decrypts the sealed `secrets.age` file with one X25519 identity. yoink uses the first identity that it finds.

| Priority | Source | Use |
|---|---|---|
| 1 | `YOINK_AGE_KEY` env | CI |
| 2 | `YOINK_AGE_KEY_FILE` env | Explicit override |
| 3 | `~/.config/yoink/keys/<recipient>.key` | Laptop default. yoink scans the directory and selects the key whose public key is in `secrets.recipients:`. |
| 4 | `~/.config/yoink/age.key` | Legacy fallback |

| Command | Effect |
|---|---|
| `yoink secrets key generate` | Writes a key to the keys directory (source 3). |
| `yoink secrets key generate --print \| gh secret set YOINK_AGE_KEY` | Sends the key to the secret store of your CI. The workflow supplies it as `YOINK_AGE_KEY` at deploy time. |

## `command` provider output

| Property | Value |
|---|---|
| Input | The stdout of the command from the config. |
| JSON | stdout that starts with `{`. |
| dotenv | All other stdout (`KEY=value\n`). |
| Number of runs | One for each `yoink up`, and one at TUI startup. |
| Stderr | Captured. yoink shows it when the command exits non-zero. |

## `yoink prune` rules

`yoink prune` removes a container that has the `yoink.managed=true` label when one of these conditions is true:

| Condition | Reason in the output |
|---|---|
| Its service is not in the current config (renamed or removed). The container can be running or stopped. | `service not in config` |
| Its service is in the current config and the container is exited, dead, or created (never started). | `stale exited` |

`yoink prune` does **not** remove:

- Containers without the `yoink.managed=true` label: legacy containers, containers that you started by hand, and the deploy lock sentinel.
- Running, paused, and restarting containers of a service that is in the current config.
- Images, volumes, or networks. `docker volume prune` and `docker network prune` remove volumes and networks.

The second rule also removes the previous generation that `yoink rollback` reads. After a prune, a rollback of that service is not possible.

`yoink prune --dry-run` prints the containers that match and removes nothing.

## Container host config

yoink builds the docker host config from `service.run.options` and these fixed parts.

| Part | Behavior |
|---|---|
| `tmpfs` mount options | yoink adds `noexec,nosuid,nodev`, unless you set `exec`, `suid`, or `dev` explicitly. |
| `binds` | yoink adds the `:ro` suffix when no mode is set. `:rw` must be explicit. |
| `init` | `true` by default. tini as PID 1 reaps zombies and forwards SIGTERM. |
| Network mode | The first network in the `networks:` list of the service. Without that list, the first network in `deploy.networks`. yoink attaches the other networks before it starts the container. |
| Port bindings | Parsed from the `publish:` entries. |
| Restart policy | The `restart:` value (`no`, `always`, `unless-stopped`, `on-failure`). The default is `unless-stopped`. |

## Container names

| Case | Name |
|---|---|
| One replica | `<service>-<short_hash>` |
| Replicas | `<service>-<short_hash>-<idx>` |

## Healthcheck probe

| Config | Probe |
|---|---|
| `healthcheck_path` is set | HTTP GET on the path. |
| No path is set | TCP connect on `port`. |
| No `port` is set | No probe. |
| `healthcheck_timeout` | yoink tries again with a backoff until the probe passes or this time elapses. |
| `drain_timeout` | The time that docker gives the old container to stop before it kills the container. yoink keeps the stopped container for `yoink rollback`. |

## See also

- [How it works](/docs/explanation/architecture): why yoink uses a hash, a sentinel lock, and a gated swap.
- [Configuration reference](/docs/reference/config): the fields that these mechanisms read.
- [Security defaults](/docs/reference/security-defaults): the default value of each run option.
- [Troubleshooting](/docs/how-to/troubleshooting): what to do when a lock or a healthcheck fails.
