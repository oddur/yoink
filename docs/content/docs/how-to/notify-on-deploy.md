---
title: Notify chat / paging / observability on deploy
description: Fire HTTP webhooks on `RunStarted` / `RunFinished` to ntfy, Slack, Grafana, or any other receiver. Templated bodies, `${secret:NAME}` for auth, audited via `WebhookFired`.
---

A deploy that succeeds quietly is fine until the room wants to know it shipped. The `webhooks:` block declares outbound HTTP calls that fire on the run's lifecycle events — `run_started`, `run_succeeded`, `run_failed` — with templated url, headers, and body, and `${secret:NAME}` placeholders resolved against the sealed bundle so bearer tokens never sit in the config plaintext.

Webhooks are operator-side and best-effort: the call goes out from the box running `yoink up`, a failure is recorded as a `WebhookFired { ok: false }` audit event, and the deploy never aborts on a webhook error.

## Triggers

| trigger | fired after | template `event` | `error` populated |
|---|---|---|---|
| `run_started` | the `RunStarted` audit line lands | `"run_started"` | no |
| `run_succeeded` | `RunFinished { ok: true }` | `"run_succeeded"` | no |
| `run_failed` | `RunFinished { ok: false }` | `"run_failed"` | yes — operator-facing error message |

Subscribe each webhook to a subset; `on:` must list at least one trigger.

## Template context

Every templated field renders through minijinja with strict undefined-variable behavior — a typo in `{{ servicse }}` fails the render and lands as `WebhookFired { ok: false }` instead of silently sending an empty string.

| variable | type | notes |
|---|---|---|
| `event` | string | `"run_started"`, `"run_succeeded"`, `"run_failed"`. |
| `ok` | bool | `true` for `run_started` / `run_succeeded`, `false` for `run_failed`. |
| `command` | string | `up`, `rollback`, etc. |
| `services` | array&lt;string&gt; | Selected services. Empty for fleet-wide commands. |
| `deploy_id` | string | `UUIDv7` matching the `deploy_id` in the audit log. |
| `actor` | string | `$USER@$HOSTNAME` of the operator. |
| `host` | string | Operator's hostname. |
| `git_sha` | string \| null | Short SHA of the operator's working tree. |
| `yoink_version` | string |  |
| `error` | string \| null | Operator-visible failure message; only set on `run_failed`. |
| `ts` | string | RFC 3339 timestamp the webhook rendered at. |

## ntfy.sh

Public push notifications, no auth, GET-or-POST to `https://ntfy.sh/<topic>`. Subscribe to the topic in the ntfy app or `ntfy sub <topic>`.

```yaml
webhooks:
  - name: ntfy
    url: https://ntfy.sh/my-deploys
    on: [run_started, run_succeeded, run_failed]
    headers:
      Title: "yoink {{ event }}"
      Tags: "{% if ok %}rocket{% else %}warning{% endif %}"
    body: >
      {{ command }} {{ services | join(",") }} on {{ host }}
      ({{ deploy_id }}){% if error %} — {{ error }}{% endif %}
```

`Tags` rides on ntfy's emoji aliasing — `rocket` renders as a rocket, `warning` as a warning sign, so success and failure render with different glyphs.

## Slack incoming webhooks

Slack's [incoming webhooks](https://api.slack.com/messaging/webhooks) take a JSON body with a `text` field. The webhook URL itself is the credential — keep it sealed.

```yaml
secrets:
  provider: age
  recipients: [age1…]
  # SLACK_WEBHOOK_PATH = services/T0…/B0…/abc123  (the path under hooks.slack.com)

webhooks:
  - name: slack-deploys
    url: "https://hooks.slack.com/${secret:SLACK_WEBHOOK_PATH}"
    on: [run_succeeded, run_failed]
    headers:
      Content-Type: application/json
    body: |
      {
        "text": "{% if ok %}:white_check_mark:{% else %}:x:{% endif %} *{{ command }}* {{ services | join(', ') }} ({{ deploy_id }}){% if error %} — `{{ error }}`{% endif %}"
      }
```

The full URL is `https://hooks.slack.com/services/T…/B…/…`, but only the path varies between workspaces — splitting at `/services/` keeps the host literal in the config and only the path sealed.

## Grafana annotations

[Grafana's annotations API](https://grafana.com/docs/grafana/latest/developers/http_api/annotations/) marks a vertical line on every dashboard at the moment of the call. Pin deploy events on your latency / error-rate dashboards so the cause of an inflection is one glance away.

```yaml
webhooks:
  - name: grafana-annotation
    url: https://grafana.example.com/api/annotations
    on: [run_succeeded]
    headers:
      Authorization: "Bearer ${secret:GRAFANA_API_TOKEN}"
      Content-Type: application/json
    body: |
      {
        "tags": ["deploy", "yoink", "{{ command }}"],
        "text": "{{ services | join(', ') }} → {{ git_sha }} ({{ deploy_id }})"
      }
```

Annotations are dashboard-wide unless `dashboardUID:` / `panelId:` are set; see the Grafana API docs.

## Field reference

| field | type | default | notes |
|---|---|---|---|
| `name` | string | required | Identifier shown in `WebhookFired` audit events. Must be unique across the `webhooks:` list. |
| `url` | string | required | Templated. Must resolve to `http(s)://…` after `${secret:NAME}` substitution. |
| `on` | list of trigger | required | One or more of `run_started`, `run_succeeded`, `run_failed`. |
| `method` | `GET` \| `POST` \| `PUT` | `POST` |  |
| `timeout` | duration | `10s` | Per-request timeout, parsed by `humantime` (`5s`, `2m`, …). |
| `headers` | map of string | `{}` | Each value is templated. |
| `body` | string | unset | Templated. Omit for GET-style receivers (e.g. ntfy URL-only triggers). |

## Secrets in templates

`${secret:NAME}` inside `url`, any header value, or `body` is replaced with the decrypted value from the sealed bundle before minijinja runs. Two passes, in this order:

1. Literal `${secret:NAME}` → bundle value. Missing key (or no `secrets:` provider configured at all) is an error — no silent empty substitution.
2. minijinja render with `UndefinedBehavior::Strict`. A typo in a `{{ var }}` fails the render.

Both kinds of error land as `WebhookFired { ok: false, error: "<reason>" }` in the audit log; the deploy proceeds.

The two-pass order matters: secrets resolve first, so a secret value that happens to contain `{{` won't be re-templated.

## Audit trail

Every fire — success or failure — records a [`WebhookFired`](/docs/reference/audit-event-schema#webhookfired) audit event:

```sh
yoink audit log --event WebhookFired --since 1h
yoink audit run <deploy-id> | grep WebhookFired
```

Failure modes captured in `error`:

- `transport: <reqwest message>` — DNS, TCP, TLS.
- `non-2xx: <code>` — receiver returned a 4xx/5xx.
- `timed out after <duration>`.
- `template parse: …` / `template render: …` — minijinja error.
- `webhook references secret "X" but it is not in the sealed bundle`.

## Verifying without a real deploy

Point one webhook at a local listener and run any deploy that triggers it:

```sh
nc -l 9000 &
yoink up    # or any subcommand that emits RunStarted/RunFinished
```

The body and rendered headers print on stdout, and `yoink audit log --event WebhookFired` shows the corresponding `ok: true, status: 200` line. Stop the listener and re-run to see `ok: false, error: "transport: …"`.

## See also

- [Configuration: webhooks](/docs/reference/config#webhooks) — full schema.
- [Audit event schema: `WebhookFired`](/docs/reference/audit-event-schema#webhookfired) — what's logged for each fire.
- [Read the audit log](/docs/how-to/audit-log) — `yoink audit run <id>` reconstructs every event for one deploy, webhooks included.
- [Sealed secrets workflow](/docs/how-to/sealed-secrets-workflow) — how `${secret:NAME}` values get into the bundle.
