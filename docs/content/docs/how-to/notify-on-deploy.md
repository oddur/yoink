---
title: Notify chat / paging / observability on deploy
description: Send HTTP webhooks with templated bodies and sealed credentials to ntfy, Slack, Grafana, or a different receiver when a run starts or finishes.
---

The `webhooks:` block declares outbound HTTP calls. yoink sends them on the lifecycle events of a run: `run_started`, `run_succeeded`, and `run_failed`. The url, the headers, and the body are templates. yoink resolves `${secret:NAME}` placeholders from the sealed bundle, so tokens are never plaintext in the config.

The call goes out from the computer that runs `yoink up`. A webhook error never aborts the deploy. yoink records the failure as a `WebhookFired { ok: false }` audit event.

The [Webhooks reference](/docs/reference/webhooks) lists the triggers, the template variables, and the fields.

## ntfy.sh

ntfy.sh sends public push notifications. It needs no authentication.

1. Add this webhook to `yoink.yaml`:

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

2. Subscribe to the topic in the ntfy app or with `ntfy sub <topic>`.

The `Tags` header uses the emoji aliases of ntfy, so success shows a rocket and failure shows a warning sign.

## Slack incoming webhooks

A Slack [incoming webhook](https://api.slack.com/messaging/webhooks) takes a JSON body with a `text` field. The webhook URL is the credential, so seal it.

1. Put the path of the webhook URL (the part under `hooks.slack.com`) in the sealed bundle as `SLACK_WEBHOOK_PATH`.
2. Add this webhook to `yoink.yaml`:

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

The full URL is `https://hooks.slack.com/services/T…/B…/…`. The config keeps the hostname as a literal and seals only the path.

## Grafana annotations

The [annotations API of Grafana](https://grafana.com/docs/grafana/latest/developers/http_api/annotations/) marks a vertical line on every dashboard at the moment of the call. Then you can see if a deploy caused a change in a graph.

1. Make sure that `GRAFANA_API_TOKEN` is in the sealed bundle.
2. Add this webhook to `yoink.yaml`:

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

An annotation applies to all dashboards unless you set `dashboardUID:` or `panelId:`. The Grafana API docs describe these fields.

## Verifying without a real deploy

1. Point one webhook at a local listener.
2. Run a deploy that triggers the webhook:

   ```sh
   nc -l 9000 &
   yoink up    # or any subcommand that emits RunStarted/RunFinished
   ```

   The listener prints the body and the headers on stdout.

3. Run `yoink audit log --event WebhookFired`. The output shows one event for each call, with its `ok`, `status`, and `error` values.
4. Stop the listener and run the deploy again. The new event shows `ok: false, error: "transport: …"`.

## Audit trail

Each webhook call, success or failure, records a [`WebhookFired`](/docs/reference/audit-event-schema#webhookfired) audit event. To read these events, run one of these commands:

```sh
yoink audit log --event WebhookFired --since 1h
yoink audit run <deploy-id> | grep WebhookFired
```

If a webhook failed, read the `error` field of its event. The [Webhooks reference](/docs/reference/webhooks#error-values) lists the values.

## See also

- [Webhooks reference](/docs/reference/webhooks): the triggers, the template variables, the fields, the secret substitution, and the error values.
- [Configuration: webhooks](/docs/reference/config#webhooks): the full schema.
- [Read the audit log](/docs/how-to/audit-log): `yoink audit run <id>` shows every event for one deploy, webhooks included.
- [Sealed secrets workflow](/docs/how-to/sealed-secrets-workflow): how `${secret:NAME}` values get into the bundle.
