# How to send events to webhooks and Slack

This guide shows you how to deliver object events (created, deleted, copied, replicated, expired) to an HTTP webhook or a Slack channel. Delivery uses the durable event outbox, so the S3 write path never waits for your endpoint. The [event log reference](../reference/event-outbox.md) describes the semantics.

## 1. Configure a webhook destination

In the admin UI:

1. In the sidebar, open **Integrations → Event delivery** (`/_/admin/integrations/event-delivery`).
2. Turn on **Enable delivery**. Leave **Payload format** on **Raw webhook**.

   ![The Event delivery page with delivery off; callout 2 marks the Enable delivery switch, and Payload format is on Raw webhook.](/_/screenshots/events-enable.webp)

3. Under **Endpoints**, click **Add endpoint**, and type the URL of your receiver, for example `https://events.acme.example/deltaglider`. Click **Add endpoint** again for each further receiver.
4. If the receiver runs on the same host or in the same private network, turn on **Allow local receivers**.
5. Under **Headers**, click **Add header**, and type the header name and its value, for example `Authorization` and `Bearer YOUR-TOKEN`. The value is stored encrypted and shown masked.

   ![The raw webhook destination holds one endpoint and an Authorization header; callout 3 marks the endpoint URL, callout 4 marks the Allow local receivers switch, and callout 5 marks the header.](/_/screenshots/events-endpoint.webp)

6. Click **Review & apply** in the bar at the bottom of the page. Check the diff in the dialog, and then click **Apply and Persist**.

   ![The review dialog shows the event_delivery change with the endpoint and the masked header; the arrow points at Apply and Persist.](/_/screenshots/events-apply.webp)

Every endpoint receives every event, and a row counts as delivered only when **all** endpoints return 2xx. The proxy records the result of each endpoint separately. When one endpoint fails, the proxy still posts to the other endpoints, and a retry posts only to the endpoints that have not yet returned 2xx, so the healthy endpoints do not receive the event twice. The proxy knows an endpoint by its position in the list and by its scheme, host and port. So when you rotate a token in the path or query of an endpoint URL, the endpoint keeps its state, and a retry does not post the event to it again. The same URL listed twice is one endpoint. Each POST body is the `{schema, event}` JSON envelope. The full payload schema and the tuning knobs (`tick_interval`, `batch_size`, `max_attempts`, retention) are in the [reference](../reference/event-outbox.md#yaml-grammar). In the admin UI, the knobs are under **Delivery tuning (retry, retention, batching)**.

The URLs must use `https://`, and they must not point at a private, loopback or cloud-metadata address. The proxy refuses a config apply that adds such a URL, because every delivery to it would fail. **Allow local receivers** (`allow_local: true`) allows `http://` and private addresses for the webhook URLs, but the proxy still refuses cloud-metadata addresses.

Delivery is at-least-once, so make the receiver idempotent, typically by deduplicating on `event.id`.

### The same change in YAML

The steps above write this configuration into the `advanced` section of `deltaglider_proxy.yaml` ([why the UI and the file hold the same configuration](../explanation/two-ways-to-configure.md)):

```yaml
# validate
advanced:
  event_delivery:
    enabled: true
    webhook_urls:
      - "https://events.acme.example/deltaglider"
    webhook_headers:
      Authorization: "Bearer YOUR-TOKEN"
```

Write the endpoints as the list `webhook_urls`, even for one endpoint. The older single field `webhook_url` still loads, but the admin UI saves every endpoint into `webhook_urls` and clears `webhook_url`, so a file that uses the list stays the same after a change in the UI. Keep the token out of the file with a `${env:NAME}` reference, for example `Authorization: "Bearer ${env:EVENTS_TOKEN}"`. Add `allow_local: true` for a receiver on a private address.

After you edit the file, restart the proxy, or apply the file to the running proxy with `POST /_/api/admin/config/apply` or with `deltaglider_proxy config apply deltaglider_proxy.yaml --server https://s3.acme.example`. Delivery reads its settings on every tick, so no restart is needed.

## 2. Filter what gets sent

Raw webhook mode delivers **every** event kind, so filter at the receiver on `event.kind` (`ObjectCreated`, `ObjectDeleted`, `ObjectCopied`, `ReplicationObjectCopied`, `LifecycleExpired`, `LifecycleTransitioned`).

In Slack format, you filter at the source instead. The filters are part of the Slack connector (section 3): open **What gets posted (event kinds + prefix filters)**, select the **Event kinds**, and add globs under **Include prefixes** and **Exclude prefixes**. The default kind is `ObjectCreated` only. An empty include list means every user object, and an exclude glob wins over an include glob.

![The What gets posted section of the Slack connector is open; the box marks the event kinds and the include and exclude prefix filters.](/_/screenshots/events-slack-filter.webp)

In YAML, the three filters are `slack_notify_kinds`, `slack_include_globs` and `slack_exclude_globs`. The recap at the end of section 3 shows them.

In Slack mode, the proxy never posts directory markers and DeltaGlider internals.

## 3. Slack instead of raw JSON

The Slack format posts formatted messages. It uses no OAuth: you paste a credential. So it works even when the proxy runs at a private address, because delivery is outbound HTTPS only. There are two mutually exclusive ways to connect.

**If one channel is enough**, use an Incoming Webhook. In Slack, create an app, enable *Incoming Webhooks*, pick a channel, and copy the `https://hooks.slack.com/services/…` URL. Then, in the admin UI:

1. On **Integrations → Event delivery**, turn on **Enable delivery**, and select **Slack** in **Payload format**.
2. In **How to connect**, keep **Incoming Webhook (simplest)**.
3. Click **Add webhook URL**, and paste the URL into **Incoming Webhook URL**.

   ![Event delivery in Slack format with an Incoming Webhook; callout 1 marks Slack in Payload format, callout 2 marks How to connect, and callout 3 marks the Incoming Webhook URL.](/_/screenshots/events-slack-webhook.webp)

4. Click **Review & apply**, and then click **Apply and Persist**.

**If you want several channels or routing per bucket**, use a bot token. In Slack, create an app, add the `chat:write` and `chat:write.public` scopes, install it to the workspace, and copy the `xoxb-…` token. Then, in the admin UI:

1. On **Integrations → Event delivery**, turn on **Enable delivery**, and select **Slack** in **Payload format**.
2. In **How to connect**, select **Bot token (multi-channel + @mentions)**.
3. Paste the token into **Bot token**, and type the fallback channel into **Channel**, for example `#ops`.

   ![Event delivery in Slack format with a bot token; callout 1 marks Bot token in How to connect, callout 2 marks the Bot token field, and callout 3 marks the Channel field.](/_/screenshots/events-slack-bot.webp)

4. To send some buckets or prefixes to another channel, open **Channel routing (per bucket / prefix)** and add a route.
5. Click **Review & apply**, and then click **Apply and Persist**.

The page shows a live preview of the message that lands in the channel. The proxy masks the bot token on export and in the GUI, and an unchanged round-trip preserves the real token.

### The same change in YAML

The Incoming Webhook steps and the filters of section 2 write this configuration into the `advanced` section of `deltaglider_proxy.yaml` ([why the UI and the file hold the same configuration](../explanation/two-ways-to-configure.md)):

```yaml
# validate
advanced:
  event_delivery:
    enabled: true
    format: slack
    webhook_urls:
      - "https://hooks.slack.com/services/T000/B000/XXXX"
    slack_notify_kinds: ["ObjectCreated", "ObjectDeleted"]
    slack_include_globs: ["firmware/**"]
    slack_exclude_globs: ["**/*.tmp"]
```

The bot token steps, with one route, write this configuration instead:

```yaml
# validate
advanced:
  event_delivery:
    enabled: true
    format: slack
    slack_bot_token: "xoxb-0000-0000-example"
    slack_channel: "#ops"
    slack_routes:
      - name: "Releases to #ci"
        bucket: "releases"
        prefix_globs: ["firmware/**"]
        channel: "C_CI"
```

Keep the bot token out of the file with a `${env:NAME}` reference. Apply the file in the same way as in section 1.

## 4. Test a delivery

1. Apply the config, then upload something:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 cp probe.txt s3://releases/probe.txt
   ```

2. Within one `tick_interval` (10s default) the dispatcher claims the row and POSTs it. Check your endpoint logs or the Slack channel.
3. Check the status of the row in the event log at **Integrations → Event log** (`/_/admin/integrations/event-outbox`), or with the admin API:

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

Requeue does not create a new event. It changes `failed` back to `pending`, keeps the attempt history, and makes the row due immediately.

The admin UI does the same on the Event log page:

1. In the sidebar, open **Integrations → Event log** (`/_/admin/integrations/event-outbox`).
2. Optionally, select **Failed** in the status list, so that the table shows only the failed rows.
3. Click **Requeue** in the row. To requeue every failed row that the table shows, click **Requeue failed shown**.

   ![The Event log lists an event whose delivery failed; the arrow points at the Requeue button of that row.](/_/screenshots/events-log-requeue.webp)

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
