---
title: Audit event schema
description: Per-variant JSON fields for events in `/var/lib/yoink/audit/events.jsonl`.
---

Every line in the on-host audit log is a single JSON object. Top-level fields are constant; the `event` discriminator selects which variant fields are present. See [Read the on-host audit log](/docs/how-to/audit-log) for the operator-facing reading flow.

## Envelope (every event)

| field | type | notes |
|---|---|---|
| `v` | int | Schema version. Currently `1`. Bumped on backward-incompatible changes; older readers can refuse newer lines. |
| `event_id` | string | `UUIDv7` per individual line. The merge view dedupes on this; the TUI uses it as a stable row identifier. |
| `origin` | string | `"operator"` (line written to `$XDG_STATE_HOME/yoink/audit/events.jsonl` on the operator's machine) or `"host"` (line written to `/var/lib/yoink/audit/events.jsonl` on the managed host). |
| `ts` | string | RFC 3339 wall-clock with millisecond precision (`2026-05-01T15:42:01.317Z`). |
| `deploy_id` | string | `UUIDv7` — one per `yoink up` / `rollback` / `prune` / `secrets rotate` run. Lexicographic sort orders by start time. |
| `actor` | string | `$USER@$HOSTNAME` of the operator. `?@?` when not resolvable. |
| `yoink_version` | string | Operator's yoink version. |
| `git_sha` | string \| null | Short git SHA of the operator's working tree at the time of the run. `null` when not a git repo or `--allow-dirty` was set. |
| `host` | string | The host the event applies to. Empty for `origin: "operator"` events that aren't host-specific (e.g. a `RunStarted` that fans out to multiple hosts). |
| `event` | string | Discriminator — picks the variant below. |

## Envelope events

### `RunStarted`

| field | type | notes |
|---|---|---|
| `command` | string | `up`, `rollback`, `prune`, `secrets rotate`. |
| `services` | array&lt;string&gt; | Selected service names; empty when the run targets the full config. |

### `RunFinished`

| field | type | notes |
|---|---|---|
| `command` | string | Same as `RunStarted.command`. |
| `ok` | bool | `true` on clean reconcile; `false` if any host or service failed. |
| `error` | string \| null | First operator-facing error string when `ok=false`. |

### `LockAcquired`, `LockReleased`

No additional fields. The gap between paired events is the deploy-lock hold time on that host.

## Per-service / per-container events

### `HookStarted`, `HookFinished`

| field | type |
|---|---|
| `name` | string |

`HookFinished` is forensic — it lands per-event so a triage tail of the audit log shows hooks even on aborted runs.

### `PullStarted`, `PullFinished`

| field | type |
|---|---|
| `image` | string |
| `tag` | string |

### `NetworkReady`

| field | type | notes |
|---|---|---|
| `network` | string | Docker network name. |
| `created` | bool | `true` if yoink created it on this run; `false` if it already existed. |

### `ContainerStarted`

| field | type |
|---|---|
| `service` | string |
| `container` | string |
| `spec_hash` | string |
| `tag` | string |

Progress event — fired when the new replica's container is created and started, before the healthcheck. The forensic counterpart is `ContainerCreated`.

### `HealthcheckHealthy`

| field | type | notes |
|---|---|---|
| `service` | string |  |
| `container` | string |  |
| `attempts` | int | Polls before passing. |

### `ContainerCreated`

Forensic event: the new replica passed its healthcheck and was promoted into routing. This is the "we did the deploy" event.

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

Fired when every replica was already at the desired spec — no work done.

### `OldContainerStopped`, `ContainerRemoved`

| field | type |
|---|---|
| `service` | string |
| `container` | string |

`OldContainerStopped` is progress (best-effort flush). `ContainerRemoved` is forensic.

### `DeployFailed`

Forensic. Fired when the new replica never passed its healthcheck and yoink rolled back.

| field | type | notes |
|---|---|---|
| `service` | string |  |
| `container` | string |  |
| `log_tail` | array&lt;string&gt; | The last few stdout/stderr lines from the failed container, captured before yoink removed it. |

### `RollbackStarted`, `RollbackFinished`

| field | type | notes |
|---|---|---|
| `service` | string |  |
| `target_tag` (Started) | string | The tag yoink is restoring to. |
| `ok` (Finished) | bool |  |
| `error` (Finished) | string \| null |  |

### `ContainerPruned`

Fired by `yoink prune` for every removed container.

| field | type | notes |
|---|---|---|
| `service` | string \| null | `null` when the container had no `yoink.service` label. |
| `container` | string |  |

### `SecretsRotated`

| field | type | notes |
|---|---|---|
| `new_recipient` | string | The new `age1…` public key. The old recipient is in the prior `secrets:` block — diff git history to see it. |

### `FileUploaded`

| field | type | notes |
|---|---|---|
| `service` | string | Service that owns the mount. |
| `sha256` | string | Content hash — same as the path under `/var/lib/yoink/files/`. |
| `remote_path` | string | Absolute path on the host. |

## Forensic vs. progress

The flush policy is per-variant. **Forensic** events bypass the per-host buffer and SSH-flush immediately, so a `kill -9` mid-deploy still lands them on disk. **Progress** events buffer and flush at end of run.

| Forensic | Progress |
|---|---|
| `RunStarted`, `RunFinished` | `HookStarted` |
| `LockAcquired`, `LockReleased` | `PullStarted`, `PullFinished` |
| `HookFinished` | `NetworkReady` |
| `ContainerCreated`, `ContainerRemoved` | `ContainerStarted` |
| `DeployFailed` | `HealthcheckHealthy` |
| `RollbackStarted`, `RollbackFinished` | `OldContainerStopped` |
| `ContainerPruned` | `AlreadyAtSpec` |
| `SecretsRotated` |  |
| `FileUploaded` |  |

## Storage

Two files; same line shape, different paths:

| origin | path | who writes |
|---|---|---|
| `operator` | `$XDG_STATE_HOME/yoink/audit/events.jsonl` (defaults to `~/.local/state/yoink/audit/`) | the operator's `yoink` process — local file write |
| `host` | `/var/lib/yoink/audit/events.jsonl` on each managed host | operator over SSH (`tee -a`); permissions `0640`, owned by the SSH deploy user |

- **Rotation**: each active file rotates at 5 MiB into `events-<UTC-timestamp>.jsonl`; a fresh active file starts.
- **Retention**: yoink does not enforce one. Run `yoink audit gc --keep <DURATION>` from cron to prune (host-side only currently; operator-side rotated files accumulate until you delete them manually).

## See also

- [Read the on-host audit log](/docs/how-to/audit-log) — operator-facing how-to.
- [CLI reference: `yoink audit`](/docs/reference/cli) — every flag.
- [Architecture: drift detection](/docs/guide/architecture#drift-detection) — `spec_hash` is the same one that appears in `ContainerCreated.spec_hash`.
