---
title: Audit event schema
description: The JSON fields of each event variant in the yoink audit log.
---

Each line in an audit log file is one JSON object: the envelope fields, and the fields of the variant that `event` names.

## Envelope (every event)

| field | type | notes |
|---|---|---|
| `v` | int | Schema version. Currently `1`. It increases on backward-incompatible changes. Older readers can refuse newer lines. |
| `event_id` | string | `UUIDv7` for each line. The merge view removes duplicates on this field. The TUI uses it as a stable row identifier. |
| `origin` | string | `"operator"` for a line in the log on the computer of the operator. `"host"` for a line in the log on the host. See [Storage](#storage). |
| `ts` | string | RFC 3339 wall-clock time with millisecond precision (`2026-05-01T15:42:01.317Z`). |
| `deploy_id` | string | `UUIDv7`, one for each run. A lexicographic sort orders by start time. |
| `actor` | string | `$USER@$HOSTNAME` of the operator. `?` replaces a part that yoink cannot resolve. |
| `yoink_version` | string | The yoink version of the operator. |
| `git_sha` | string \| null | Short git SHA of the working directory of the operator at the time of the run. `null` when git gives no SHA, for example outside a git repo. |
| `host` | string | The host that the event applies to. Empty for `origin: "operator"` events that are not host-specific (for example, a `RunStarted` that goes to more than one host). |
| `event` | string | Discriminator: selects the variant below. |

## Envelope events

### `RunStarted`

| field | type | notes |
|---|---|---|
| `command` | string | `up`. A rollback also runs as `up`. |
| `services` | array&lt;string&gt; | Selected service names. Without `--service`, every service in the config. |

### `RunFinished`

| field | type | notes |
|---|---|---|
| `command` | string | Same as `RunStarted.command`. |
| `ok` | bool | `true` on a clean reconcile. `false` if a host or a service failed. |
| `error` | string \| null | The error message that the operator sees when `ok=false`. |

### `LockAcquired`, `LockReleased`

No additional fields. The time between the paired events is the hold time of the deploy lock on that host.

## Per-service / per-container events

### `HookStarted`, `HookFinished`

yoink writes the hook events to the operator log, with the origin `"operator"`. `yoink audit log --origin host` does not show them.

| field | type |
|---|---|
| `name` | string |

### `PullStarted`, `PullFinished`

| field | type |
|---|---|
| `image` | string |
| `tag` | string |

### `NetworkReady`

| field | type | notes |
|---|---|---|
| `network` | string | Docker network name. |
| `created` | bool | `true` if yoink created the network on this run. `false` if the network already existed. |

### `ContainerStarted`

| field | type |
|---|---|
| `service` | string |
| `container` | string |
| `spec_hash` | string |
| `tag` | string |

yoink started the container of a new replica. The healthcheck comes after this event.

### `HealthcheckHealthy`

| field | type | notes |
|---|---|---|
| `service` | string |  |
| `container` | string |  |
| `attempts` | int | The number of probes until the healthcheck passed. |

### `ContainerCreated`

The new replicas passed their healthchecks and yoink completed the swap. This event records that the deploy occurred. yoink writes one for each service on each host. `container` is the first replica.

| field | type |
|---|---|
| `service` | string |
| `container` | string |
| `spec_hash` | string |
| `tag` | string |

### `AlreadyAtSpec`

| field | type |
|---|---|
| `service` | string |
| `container` | string |
| `spec_hash` | string |

Every replica was already at the desired spec. yoink did no work.

### `OldContainerStopped`, `OldContainerRestarted`, `ContainerRemoved`

| field | type |
|---|---|
| `service` | string |
| `container` | string |

`OldContainerRestarted`: a deploy of a service with `publish:` failed. yoink started the old container again, which it had stopped for the deploy.

### `DeployFailed`

A new replica did not pass its healthcheck. yoink removed every container that the failed attempt created for the service on that host, and no old container.

| field | type | notes |
|---|---|---|
| `service` | string |  |
| `container` | string | The replica that failed. |
| `log_tail` | array&lt;string&gt; | The last 30 stdout/stderr lines of the failed container. yoink captures them before it removes the container. If yoink cannot get the logs, the one entry is the reason. |

### `RollbackStarted`, `RollbackFinished`

| field | type | notes |
|---|---|---|
| `service` | string |  |
| `target_tag` (Started) | string | The tag that yoink restores. |
| `ok` (Finished) | bool |  |
| `error` (Finished) | string \| null |  |

### `ContainerPruned`

| field | type | notes |
|---|---|---|
| `service` | string \| null | `null` when the container had no `yoink.service` label. |
| `container` | string |  |

### `SecretsRotated`

| field | type | notes |
|---|---|---|
| `new_recipient` | string | The new `age1…` public key. The old recipient is in the earlier `secrets:` block. A diff of the git history shows it. |

### `FileUploaded`

| field | type | notes |
|---|---|---|
| `service` | string | The service that owns the mount. |
| `sha256` | string | Content hash. It is a part of the name of the staged file. |
| `remote_path` | string | Absolute path on the host. |

### `WebhookFired`

One for each outbound webhook call, success or failure. The origin is always `"operator"`.

| field | type | notes |
|---|---|---|
| `name` | string | The `name:` of the webhook in `yoink.yaml`. |
| `ok` | bool | `true` only on a 2xx response. |
| `status` | int \| null | HTTP status code when a response arrived. `null` when the call did not get a status (DNS / TCP / TLS / timeout / template error). |
| `error` | string \| null | The failure reason. See [Webhooks: error values](/docs/reference/webhooks#error-values). `null` when `ok=true`. |

### `AdminNetworkContainersDisconnected`

| field | type | notes |
|---|---|---|
| `containers` | array&lt;string&gt; | The yoink app containers that yoink disconnected from `yoink-proxy-admin`. |

The `yoink-proxy-admin` network had no `yoink.role` label: an earlier yoink version created it. Before v0.21, services joined this network by default. yoink disconnected them, because the proxy admin API has no authentication.

### `AdminRelayCreated`

No fields. yoink created the admin relay `yoink-proxy-admin-relay` on the host. Or, the relay did not run or had another spec, and yoink replaced it.

## Variants that yoink does not write

Readers accept these variants, but yoink writes none of them to a log file.

| Variant | Reason |
|---|---|
| `PullFinished`, `ContainerRemoved`, `RollbackStarted`, `RollbackFinished`, `ContainerPruned`, `SecretsRotated`, `FileUploaded` | No command emits the event. |

## Forensic vs. progress

In the host log, yoink appends a **forensic** event over SSH immediately, so a `kill -9` during a deploy does not lose it. A **progress** event goes into a buffer for its host. yoink flushes the buffer with the next forensic event for that host, and at the end of the run. In the operator log, yoink writes every event immediately.

| Forensic | Progress |
|---|---|
| `RunStarted`, `RunFinished` | `HookStarted` |
| `LockAcquired`, `LockReleased` | `PullStarted`, `PullFinished` |
| `HookFinished` | `NetworkReady` |
| `ContainerCreated`, `ContainerRemoved` | `ContainerStarted` |
| `DeployFailed`, `OldContainerRestarted` | `HealthcheckHealthy` |
| `RollbackStarted`, `RollbackFinished` | `OldContainerStopped` |
| `ContainerPruned` | `AlreadyAtSpec` |
| `SecretsRotated` |  |
| `FileUploaded` |  |
| `WebhookFired` |  |
| `AdminNetworkContainersDisconnected`, `AdminRelayCreated` |  |

## Storage

The two files have the same line shape and different paths:

| origin | path | who writes |
|---|---|---|
| `operator` | `$XDG_STATE_HOME/yoink/audit/events.jsonl` (defaults to `~/.local/state/yoink/audit/`) | The `yoink` process of the operator, as a local file write. Holds `RunStarted`, `RunFinished`, `HookStarted`, `HookFinished`, and `WebhookFired`. |
| `host` | `~/.local/state/yoink/host-audit/events.jsonl` on each host (`$XDG_STATE_HOME/yoink/host-audit/` when the variable is set) | The operator over SSH (`cat >>`). The file is in the home directory of the SSH deploy user, so no root access and no host setup are necessary. Permissions are `0640`. Holds all other events. |

Before v0.21, the host log was in `/var/lib/yoink/audit/`, which only root can create. yoink does not write there now. `yoink audit log` and `yoink audit gc` still read that directory, so the history of a host that has it stays visible.

- **Rotation**: when an active file is larger than 5 MiB after a write, yoink renames it to `events-<UTC-timestamp>.jsonl`. The next write starts a new active file.
- **Retention**: yoink does not enforce one. `yoink audit gc --keep <DURATION>` removes rotated files that are older than the duration (default `90d`), only on hosts. It never removes the active file. A cron job can run it. Rotated files on the operator side accumulate until you remove them manually.

## See also

- [Read the on-host audit log](/docs/how-to/audit-log): the procedure to read and filter the log.
- [CLI reference: `yoink audit`](/docs/reference/cli): every flag of the audit commands.
- [Architecture: drift detection](/docs/explanation/architecture#drift-detection): the source of the `spec_hash` in `ContainerCreated.spec_hash`.
