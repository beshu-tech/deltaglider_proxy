# How to send events to webhooks and Slack

This guide shows you how to deliver object events (created, deleted, copied, replicated, expired) to an HTTP webhook or a Slack channel. Delivery uses the durable event outbox, so the S3 write path never waits for your endpoint. The [event log reference](../reference/event-outbox.md) describes the semantics.

## 1. Configure a webhook destination

```yaml
# validate
advanced:
  event_delivery:
    enabled: true
    webhook_url: "https://events.example.com/deltaglider"
    webhook_headers:
      authorization: "Bearer YOUR-TOKEN"
```

Add fan-out endpoints with `webhook_urls: [...]`. Every endpoint receives every event, and a row counts as delivered only when **all** endpoints return 2xx. The proxy records the result of each endpoint separately. When one endpoint fails, the proxy still posts to the other endpoints, and a retry posts only to the endpoints that have not yet returned 2xx, so the healthy endpoints do not receive the event twice. The proxy knows an endpoint by its position in the list and by its scheme, host and port. So when you rotate a token in the path or query of an endpoint URL, the endpoint keeps its state, and a retry does not post the event to it again. The same URL listed twice is one endpoint. Each POST body is the `{schema, event}` JSON envelope. The full payload schema and the tuning knobs (`tick_interval`, `batch_size`, `max_attempts`, retention) are in the [reference](../reference/event-outbox.md#yaml-grammar).

The URLs must use `https://`, and they must not point at a private, loopback or cloud-metadata address. The proxy refuses a config apply that adds such a URL, because every delivery to it would fail. When the receiver runs on the same host or in the same private network, set `allow_local: true` in `event_delivery` (in the admin UI, turn on **Allow local receivers**). The proxy then allows `http://` and private addresses for the webhook URLs, but it still refuses cloud-metadata addresses.

In the admin UI, open **Settings → Integrations → Event delivery**.

![Event delivery settings](/_/screenshots/events-webhook.jpg)

Delivery is at-least-once, so make the receiver idempotent, typically by deduplicating on `event.id`.

## 2. Filter what gets sent

Raw webhook mode delivers **every** event kind, so filter at the receiver on `event.kind` (`ObjectCreated`, `ObjectDeleted`, `ObjectCopied`, `ReplicationObjectCopied`, `LifecycleExpired`, `LifecycleTransitioned`).

In Slack format you filter at the source instead:

```yaml
    slack_notify_kinds: ["ObjectCreated", "ObjectDeleted"]   # default: ObjectCreated only
    slack_include_globs: ["firmware/**"]                     # empty = all user objects
    slack_exclude_globs: ["**/*.tmp"]                        # exclude wins
```

In Slack mode, the proxy never posts directory markers and DeltaGlider internals.

## 3. Slack instead of raw JSON

Set `format: slack` to post formatted messages. This uses no OAuth: you paste a credential. So it works even when the proxy runs at a private address, because delivery is outbound HTTPS only. There are two mutually exclusive modes:

- **If one channel is enough**, use an Incoming Webhook. In Slack: create an app → enable *Incoming Webhooks* → pick a channel → copy the `https://hooks.slack.com/services/…` URL:

  ```yaml
  advanced:
    event_delivery:
      enabled: true
      format: slack
      webhook_url: "https://hooks.slack.com/services/T000/B000/XXXX"
  ```

- **If you want multiple channels or per-bucket routing**, use a bot token. In Slack: create an app → add the `chat:write` and `chat:write.public` scopes → install to the workspace → copy the `xoxb-…` token:

  ```yaml
  advanced:
    event_delivery:
      enabled: true
      format: slack
      slack_bot_token: "xoxb-…"
      slack_channel: "#ops"          # fallback channel
      slack_routes:
        - name: "Releases → #ci"
          bucket: "releases"
          prefix_globs: ["firmware/**"]
          channel: "C_CI"
  ```

The proxy masks the bot token on export and in the GUI, and an unchanged round-trip preserves the real token. The UI at **Settings → Integrations → Event delivery** includes a live preview of the message that will land in the channel.

## 4. Test a delivery

1. Apply the config, then upload something:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 cp probe.txt s3://releases/probe.txt
   ```

2. Within one `tick_interval` (10s default) the dispatcher claims the row and POSTs it. Check your endpoint logs or the Slack channel.
3. Check the row's status in the event log at **Settings → Integrations → Event log**, or:

   ```bash
   curl -b cookies "https://s3.acme.example/_/api/admin/event-outbox?limit=10"
   ```

## 5. Retries and requeue

Failed attempts retry with exponential backoff. After `max_attempts` (default 8), a row becomes permanently `failed` until you requeue it. Fix the endpoint first, and then requeue the row:

```bash
# one row
curl -b cookies -X POST https://s3.acme.example/_/api/admin/event-outbox/123/requeue
# several
curl -b cookies -X POST https://s3.acme.example/_/api/admin/event-outbox/requeue \
  -H 'Content-Type: application/json' -d '{"ids": [123, 124]}'
```

A row whose error cannot go away on a retry fails at once, after one attempt: for example, when the outbound-URL policy refuses the URL, or when a header value is invalid. Its error text starts with `[permanent]`. The Event delivery page shows the delivery state **Failing**, with the last error, when the newest delivery attempt failed.

Requeue does not create a new event. It changes `failed` back to `pending`, keeps the attempt history, and makes the row due immediately. The Event log page does the same with a button.

Slack's Web API returns HTTP 200 even on failure. The dispatcher therefore checks the JSON `ok` field and retries on `{"ok": false}` (for example `channel_not_found`), so a Slack misconfiguration shows up as retries and not as silent drops. When an event goes to several Slack channels or several Incoming Webhook URLs, the proxy records the result of each channel and each URL separately. A retry posts only to the channels and URLs that failed, so a channel that already shows the message does not get it twice.

## 6. Monitor the event log

The outbox list response carries per-status counts (`pending`, `in_progress`, `delivered`, `failed`). A growing `pending` count means that the dispatcher cannot keep up or that the endpoint is down. A non-zero `failed` count means that rows wait for your action. Watch the counts on the Event log page, poll the endpoint above from your monitoring, and see the [metrics reference](../reference/metrics.md) for the Prometheus side. The proxy prunes delivered rows automatically, but not pending and failed rows.

## Verify

1. A fresh PUT produces an `ObjectCreated` row that reaches `delivered` within seconds.
2. Your receiver (or Slack channel) shows the event, with the expected filtering applied.
3. The `failed` count is zero. If it is not zero, fix the endpoint and requeue the rows to drain it.

## Related

- [Event log reference](../reference/event-outbox.md): payload schema, all delivery knobs, admin API.
- [Jobs and durability](../explanation/jobs-and-durability.md): why delivery uses a durable outbox instead of blocking the write path.
- [How to replicate a bucket to another backend](replicate-a-bucket.md): the other consumer of the same outbox.
- [How to expire and archive objects](expire-and-archive-objects.md): the lifecycle events you will see.
- [Metrics reference](../reference/metrics.md): alerting on delivery health.
