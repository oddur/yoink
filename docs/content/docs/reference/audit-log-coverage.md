---
title: Audit log coverage
description: The operations that yoink records in the audit log and the operations that it does not record.
---

## Recorded

yoink records each `yoink up` run. A rollback deploys through `up`, so yoink records it as an `up` run.

- `RunStarted` / `RunFinished`: one pair for each run.
- `LockAcquired` / `LockReleased`: one pair for each host. The time between the two events is the hold time of the lock.
- `ContainerCreated`: one for each completed swap of a service on a host.
- `DeployFailed` with `log_tail` when a healthcheck never passes.
- `WebhookFired`: one for each webhook call.
- Progress events, for example `PullStarted`, `NetworkReady`, and `HealthcheckHealthy`. These events give more detail for triage.

## Not recorded

- `yoink prune` and `yoink secrets rotate`. These commands write no events.
- Hook runs. yoink writes `HookStarted` and `HookFinished` to no log file.
- Read commands: `status`, `history`, `doctor`, `diff`, `tui`, `audit log`, `secrets show`, `secrets env`. A run of `audit log` does not appear in its own output.
- Operator identity other than `$USER@$HOSTNAME`. No central identity provider exists. The field is for information only.
- Secret values. Event payloads contain names, never values.

## See also

- [Audit event schema](/docs/reference/audit-event-schema): the fields of each event variant, and the storage paths.
- [Read the audit log](/docs/how-to/audit-log): the commands that read and filter the log.
- [Security](/docs/explanation/security): the security model that the audit log is a part of.
