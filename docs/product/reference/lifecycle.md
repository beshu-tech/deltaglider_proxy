# Lifecycle Rules

Lifecycle expiration for engine-visible objects: delete old objects, or
transition/archive them to another bucket/prefix.

The admin UI exposes this under **Configuration → Storage → Object lifecycle**.
Operators can edit YAML-backed rules, preview candidates without writing
history rows, run a guarded execution, and inspect persisted history/failures.

## Scope

Lifecycle does not implement AWS XML lifecycle compatibility and does not scan raw storage artifacts. Every delete goes through `engine.delete`; every transition goes through the same shared engine transfer primitive used by replication (`engine.retrieve` → `engine.store` / `store_with_multipart_etag`). DeltaGlider metadata, reference cleanup, encryption wrappers, multipart ETag preservation, provenance metadata, and event outbox behavior stay on the same paths as normal S3/replication operations.

Lifecycle is disabled by default. A rule has to be present, the global switch must be `enabled: true`, and the rule itself must be `enabled: true` before automatic scheduler or run-now execution deletes anything. Preview is available even while disabled and stays read-only: it does not create run-history rows or acquire distributed leases.

## YAML Shape

```yaml
storage:
  lifecycle:
    enabled: false                 # default; must be true for run-now/scheduler
    tick_interval: "1h"            # scheduler poll rate, min 60s
    max_failures_retained: 100     # cap returned failure/candidate details

    rules:
      - name: expire-old-builds
        enabled: false             # default; set true to allow execution
        bucket: artifacts
        prefix: "builds/"          # "" = whole bucket
        action: delete
        expire_after: "30d"        # humantime
        batch_size: 100
        include_globs: ["builds/**/*.zip"]
        exclude_globs: [".deltaglider/**", "builds/golden/**"]

      - name: archive-old-builds
        enabled: false
        bucket: artifacts
        prefix: "builds/"
        action:
          type: transition          # "archive" is accepted as an alias
          destination:
            bucket: artifact-archive
            prefix: "cold/builds/"
          delete_source_after_success: false
        expire_after: "90d"
        batch_size: 100
        include_globs: ["builds/**/*.zip"]
```

Rule names use `[A-Za-z0-9_.-]{1,64}` and must be unique.

`delete_source_after_success: false` makes transition an archive/copy. Set it
to `true` for move semantics; lifecycle copies first, verifies the destination
HEAD when possible, and deletes the source only after the copy succeeds.

## Admin API

All endpoints are session-gated.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/api/admin/lifecycle` | List configured rules, global status, and per-rule runtime state |
| `POST` | `/_/api/admin/lifecycle/rules/:name/preview` | Dry-run a rule and return candidate keys |
| `POST` | `/_/api/admin/lifecycle/rules/:name/run-now` | Execute a rule synchronously; returns 409 if global/rule disabled or already running |
| `GET` | `/_/api/admin/lifecycle/rules/:name/history?limit=N` | Recent persisted executions, newest first |
| `GET` | `/_/api/admin/lifecycle/rules/:name/failures?limit=N` | Recent per-object failures, newest first |

Run-now response:

```json
{
  "run_id": 42,
  "rule_name": "expire-old-builds",
  "status": "succeeded",
  "objects_scanned": 1200,
  "objects_affected": 17,
  "objects_skipped": 1183,
  "bytes_affected": 448000000,
  "errors": 0,
  "candidates": [
    {
      "bucket": "artifacts",
      "key": "builds/old/app.zip",
      "action": "transition",
      "destination_bucket": "artifact-archive",
      "destination_key": "cold/builds/old/app.zip",
      "delete_source_after_success": false,
      "created_at": "2026-03-01T12:00:00Z",
      "size": 1234
    }
  ],
  "failures": []
}
```

`run_id` is present only for actual executions. `candidates` and response-local `failures` are capped by `max_failures_retained`; counters still reflect the whole run.

Overview response rule entries include runtime state when the config DB is available:

```json
{
  "name": "expire-old-builds",
  "enabled": true,
  "bucket": "artifacts",
  "prefix": "builds/",
  "action": "delete",
  "expire_after": "30d",
  "last_status": "succeeded",
  "last_run_at": 1775140800,
  "next_due_at": 1775144400,
  "objects_affected_lifetime": 17,
  "bytes_affected_lifetime": 448000000,
  "include_globs": ["builds/**/*.zip"],
  "exclude_globs": [".deltaglider/**", "builds/golden/**"]
}
```

History rows include `id`, `triggered_by` (`scheduler` or `run-now`), `started_at`, `finished_at`, neutral affected object/byte counters, `errors`, and terminal `status`. `objects_affected` / `bytes_affected` means deleted objects/bytes for delete rules and transitioned objects/copied bytes for transition rules. Failure rows include `run_id`, `bucket`, `object_key`, `occurred_at`, and `error_message`; `run_id` links failures to the execution that observed them.

## Guardrails

Lifecycle skips:

- Directory markers (`folder/`).
- DeltaGlider config-sync/internal prefixes (`.deltaglider/**`, `.dg/**`).
- Storage artifacts if they ever leak through a backend listing (`reference.bin`, `*.delta`).
- Keys excluded by `exclude_globs`.
- Keys outside `include_globs` when includes are configured.
- Keys newer than `expire_after`.

Deletion is idempotent at the object level. Transition is copy-first: a copy failure never deletes the source, and a configured source delete runs only after the destination write verifies. Per-object failures are reported in the response and persisted in the config DB with the run id that observed them.

## Runtime State

The config DB stores:

- `lifecycle_state`: current `last_status`, `last_run_at`, `next_due_at`, lifetime expired-object/byte counters, and the active scheduler lease.
- `lifecycle_run_history`: one row per `run-now` or scheduler execution.
- `lifecycle_failures`: recent per-object failures, ring-bounded by `max_failures_retained` per rule.

The scheduler uses a per-rule DB lease so multiple proxy instances sharing the same config DB do not execute the same lifecycle rule concurrently. A boot-time reconciliation marks runs left in `running` by a dead process as `failed` and records an operator-visible failure row.

## Events

When a config DB is available, successful lifecycle deletes append a `LifecycleExpired` event to the durable event outbox with rule name, expiration age, object creation time, and content length. Successful transitions append `LifecycleTransitioned` with source/destination coordinates and copied bytes. If a transition rule also deletes the source, that source delete appends a `LifecycleExpired` event with action `transition-source-delete`.

## Lifecycle vs Replication

Replication continuously mirrors a live source prefix and has conflict/delete-replication policies. Lifecycle transition is age/filter driven: it only acts on expired candidates and optionally removes the source after copy. Both share the same transfer primitive, so multipart ETags, user metadata, compression/encryption routing, transient-copy retries, and provenance markers behave consistently.

## Deferred

- Multipart-upload cleanup.
