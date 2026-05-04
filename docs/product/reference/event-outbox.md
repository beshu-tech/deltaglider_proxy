# Event outbox

Durable object events are written to the encrypted config DB after successful
S3 mutations. The outbox is append-first: PUT/COPY/DELETE, replication copy
successes, and lifecycle delete/transition successes do not call external
systems directly, so object operations do not wait on webhook latency or
failures.

## Semantics

- Rows are stored in `event_outbox` with status `pending`, `in_progress`,
  `delivered`, or `failed`.
- Delivery is disabled by default. With no delivery config, the outbox is an
  operator-visible journal only.
- When HTTP delivery is enabled, a background dispatcher claims due rows in
  small batches, POSTs each row to every configured webhook endpoint, and marks
  it delivered only after all endpoints return 2xx.
- Delivery is at-least-once. Webhook receivers must be idempotent, typically by
  deduplicating on `event.id`.
- Multiple webhooks are fan-out, not independent subscriptions. If one endpoint
  fails, the row is retried and endpoints that already accepted the event may
  see it again.
- Failed attempts use exponential backoff. After `max_attempts`, the row becomes
  permanently `failed` until an operator requeues it.
- Requeue does not create a new event. It changes only `failed` rows back to
  `pending`, clears claim/error fields, preserves `attempts` as delivery
  history, and makes the row due immediately.
- Stale `in_progress` claims are reclaimable so a crashed dispatcher does not
  wedge rows forever.
- Delivered rows are pruned by the dispatcher after `delivered_retention` and
  capped by `delivered_max_rows`. Pending, in-progress, and failed rows are not
  deleted by retention pruning because they still need operator or dispatcher
  action.
- Default healthy-state DB bound is: all non-delivered rows plus at most 10,000
  delivered rows. If delivery is broken, pending/failed rows can exceed that
  until the operator fixes delivery, requeues, or manually clears the DB.

## YAML

```yaml
advanced:
  event_delivery:
    enabled: true
    webhook_url: "https://events.example.com/deltaglider"
    webhook_urls:
      - "https://audit.example.com/deltaglider"
    webhook_headers:
      authorization: "Bearer redacted-token"
      x-dgp-env: "prod"
    tick_interval: "10s"
    batch_size: 50
    request_timeout: "5s"
    max_attempts: 8
    retry_base: "5s"
    retry_max: "5m"
    stale_claim_after: "60s"
    delivered_retention: "24h"
    delivered_max_rows: 10000
    prune_batch: 100
```

Defaults are intentionally inert:

```yaml
advanced:
  event_delivery:
    enabled: false
```

`enabled=true` without `webhook_url` or `webhook_urls` is treated as inactive
and surfaces a config warning. `webhook_url` is kept as the single-endpoint
shortcut; `webhook_urls` adds fan-out endpoints.

## Webhook Payload

Event kinds are currently `ObjectCreated`, `ObjectDeleted`, `ObjectCopied`,
`ReplicationObjectCopied`, `LifecycleExpired`, and `LifecycleTransitioned`.

Each POST body is JSON:

```json
{
  "schema": "deltaglider.event.v1",
  "event": {
    "id": 123,
    "kind": "ObjectCreated",
    "bucket": "releases",
    "key": "builds/app.zip",
    "source": "s3_api",
    "occurred_at": 1777900000,
    "payload": {},
    "status": "in_progress",
    "attempts": 1,
    "next_attempt_at": null,
    "claimed_by": "event-delivery:...",
    "claimed_at": 1777900005,
    "delivered_at": null,
    "last_error": null,
    "created_at": 1777900000
  }
}
```

## Admin API

All routes are session-gated.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/api/admin/event-outbox?limit=50&offset=0&sort=occurred_at&order=desc` | Paged outbox rows plus status counts. |
| `GET` | `/_/api/admin/event-outbox?status=failed&limit=50&offset=0` | Paged rows for one status (`pending`, `in_progress`, `delivered`, `failed`). |
| `POST` | `/_/api/admin/event-outbox/:id/requeue` | Requeue one `failed` row. Returns `409` if the row is not currently failed. |
| `POST` | `/_/api/admin/event-outbox/requeue` | Requeue failed rows by id: `{ "ids": [123, 124] }`. Non-failed ids are ignored. |

`limit` defaults to 50 and is clamped to 500. Sort fields are `id`,
`occurred_at`, `created_at`, `next_attempt_at`, `delivered_at`, `attempts`,
`status`, `kind`, `bucket`, and `key`; `order` is `asc` or `desc`.

Response:

```json
{
  "rows": [],
  "counts": { "pending": 0, "in_progress": 0, "delivered": 0, "failed": 0 },
  "total": 0,
  "limit": 50,
  "offset": 0,
  "status": null,
  "sort": "occurred_at",
  "order": "desc",
  "delivery_enabled": false,
  "delivery_active": false
}
```
