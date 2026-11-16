---
title: Webhooks
description: Triggers, template variables, fields, secret substitution, and error values for the webhooks block.
---

## Triggers

| trigger | fired after | template `event` | `error` populated |
|---|---|---|---|
| `run_started` | the `RunStarted` audit line is written | `"run_started"` | no |
| `run_succeeded` | `RunFinished { ok: true }` | `"run_succeeded"` | no |
| `run_failed` | `RunFinished { ok: false }` | `"run_failed"` | yes: the error message that the operator sees |

Each webhook subscribes to a subset of the triggers. `on:` must list at least one trigger.

## Template context

minijinja renders every templated field in strict mode. A typo such as `{{ servicse }}` fails the render. yoink then sends no request and records `WebhookFired { ok: false }`.

| variable | type | notes |
|---|---|---|
| `event` | string | `"run_started"`, `"run_succeeded"`, `"run_failed"`. |
| `ok` | bool | `true` for `run_started` / `run_succeeded`, `false` for `run_failed`. |
| `command` | string | `up`. A rollback also runs as `up`. |
| `services` | array&lt;string&gt; | The selected services. Without `--service`, every service in the config. |
| `deploy_id` | string | `UUIDv7` that matches the `deploy_id` in the audit log. |
| `actor` | string | `$USER@$HOSTNAME` of the operator. |
| `host` | string | Hostname of the operator. |
| `git_sha` | string \| null | Short SHA of the working directory of the operator. |
| `yoink_version` | string |  |
| `error` | string \| null | The failure message that the operator sees. Only set on `run_failed`. |
| `ts` | string | RFC 3339 timestamp of the moment that yoink built the context. |

## Field reference

| field | type | default | notes |
|---|---|---|---|
| `name` | string | required | Identifier that `WebhookFired` audit events show. Must be unique across the `webhooks:` list. |
| `url` | string | required | Templated. Must start with `http://` or `https://`, or contain a `${secret:NAME}` reference. |
| `on` | list of trigger | required | One or more of `run_started`, `run_succeeded`, `run_failed`. |
| `method` | `GET` \| `POST` \| `PUT` | `POST` |  |
| `timeout` | duration | `10s` | Timeout for each request. `humantime` parses the value, for example `5s` or `2m`. |
| `headers` | map of string | `{}` | Each value is templated. |
| `body` | string | unset | Templated. Not necessary for GET-style receivers, for example ntfy triggers that use only the URL. |

## Secrets in templates

yoink renders `url`, each header value, and `body` in two passes:

1. yoink replaces each literal `${secret:NAME}` with the value from the secrets bundle. A missing key is an error. A config with no `secrets:` provider is also an error. yoink never substitutes an empty string.
2. minijinja renders the result in strict mode.

Secrets resolve first, so minijinja also reads a secret value that contains `{{` as template syntax.

An error in either pass becomes `WebhookFired { ok: false, error: "<reason>" }` in the audit log. A webhook failure never stops the deploy.

## Error values

The `error` field of a `WebhookFired` event contains one of these values:

| Value | Cause |
|---|---|
| `transport: <reqwest message>` | DNS, TCP, or TLS failure. |
| `non-2xx: <code>` | The receiver returned a status outside 200 to 299. |
| `timed out after <duration>` | No response in the `timeout:` time. |
| `template parse: …`, `template render: …` | A minijinja error. |
| `webhook references secret "X" but it is not in the sealed bundle` | The bundle has no key `X`. |
| `webhook references secret "X" but no `secrets:` provider is configured` | The config has no `secrets:` block. |

An error in a header value has the prefix `header "<name>": `.

## See also

- [Configuration: webhooks](/docs/reference/config#webhooks): the `webhooks:` block in the full schema of `yoink.yaml`.
- [Audit event schema: `WebhookFired`](/docs/reference/audit-event-schema#webhookfired): the fields of the audit event for each webhook call.
- [Notify chat / paging / observability on deploy](/docs/how-to/notify-on-deploy): receiver examples for ntfy, Slack, and Grafana.
- [Secrets](/docs/explanation/secrets): how the sealed bundle works.
