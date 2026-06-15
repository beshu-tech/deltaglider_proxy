# Lifecycle rules

Lifecycle for engine-visible objects: delete old objects by age, keep the newest *N* by count (`retain-newest`), or transition/archive them to another bucket/prefix. Lifecycle rules appear on the unified Jobs surface (`GET /_/api/admin/jobs`, job id `lifecycle:<rule-name>`); see [Jobs](jobs.md).

## Scope

Lifecycle does not implement AWS XML lifecycle compatibility and does not scan raw storage artifacts. Every delete goes through `engine.delete`; every transition goes through the same shared engine transfer primitive used by replication (`engine.retrieve` → `engine.store` / `store_with_multipart_etag`). DeltaGlider metadata, reference cleanup, encryption wrappers, multipart ETag preservation, provenance metadata, and event outbox behavior stay on the same paths as normal S3/replication operations.

Lifecycle is disabled by default. A rule has to be present, the global switch must be `enabled: true`, and the rule itself must be `enabled: true` before automatic scheduler or run-now execution deletes anything. Preview is available even while disabled and stays read-only: it does not create run-history rows or acquire distributed leases.

## YAML grammar

```yaml
storage:
  lifecycle:
    enabled: false                 # default; must be true for run-now/scheduler
    tick_interval: "1h"            # scheduler poll rate, min 60s
    max_failures_retained: 100     # cap returned failure/candidate details

    rules:
      - name: expire-nightly-dumps
        enabled: false             # default; set true to allow execution
        bucket: db-archive
        prefix: "nightly/"         # "" = whole bucket
        action: delete
        expire_after: "90d"        # humantime
        batch_size: 100
        include_globs: ["nightly/**/*.dump"]
        exclude_globs: [".deltaglider/**", "nightly/golden/**"]
```

Rule names use `[A-Za-z0-9_.-]{1,64}` and must be unique.

`action` is either the string `delete` or a tagged transition object:

```yaml
        action:
          type: transition          # "archive" is accepted as an alias
          destination:
            bucket: db-archive
            prefix: "cold/nightly/"
          delete_source_after_success: false
```

`delete_source_after_success: false` makes transition an archive/copy; `true` gives move semantics. Transition is copy-first: lifecycle copies, verifies the destination HEAD when possible, and deletes the source only after the copy succeeds.

### Count-based retention: `retain-newest`

`retain-newest` keeps the newest `count` objects in a prefix and deletes the rest — selection by *count*, not age (the rule native S3 lifecycle never shipped). `expire_after` does not apply to a `retain-newest` rule and may be omitted.

```yaml
      - name: keep-last-two-nightly-dumps
        enabled: true
        bucket: db-archive
        prefix: "nightly/"
        action:
          type: retain-newest
          count: 2                  # keep the 2 newest QUALIFYING objects
          qualify:                  # only objects passing ALL of these are ranked
            min_size_bytes: 1048576 #   ignore truncated/empty junk (1 MiB)
            min_age: "1h"           #   ignore objects still being uploaded
          protect_younger_than: "7d" # optional delete-side guard (see below)
        include_globs: ["nightly/**/*.dump"]
```

- **`count`** (required, ≥ 1) — how many of the newest *qualifying* objects to keep. Objects are ranked by `created_at` descending, with a deterministic key-descending tie-break (stable across runs).
- **`qualify`** (optional) — an **eligibility filter**, not a delete guard. An object failing it is *invisible* to the rule: never counted toward `count`, never deleted. This is what stops an accidental empty/truncated file (a stray `README`, a half-written dump) from anchoring the keep set and pushing a real backup into the delete set.
  - **`min_size_bytes`** — the object's *original* (hydrated) size must be ≥ this. Guards against empty/placeholder files.
  - **`min_age`** (humantime) — object must be older than this. Guards against in-flight uploads being counted before they finish.
- **`protect_younger_than`** (optional, humantime) — a **delete-side guard**: an object selected for deletion is spared *this run* if it is younger than this. It is never promoted into the keep set; next run, once older, normal ranking applies. Most rules omit it.

The eligibility-vs-guard distinction is deliberate: `qualify.min_age` means "too young to count yet" (ignored); `protect_younger_than` means "old enough to count, but don't physically delete it yet" (spared). Preview reports `objects_ignored` and `objects_protected` so the disposition is visible before anything runs.

Unlike age rules, a `retain-newest` run is **atomic per execution** — its keep/delete decision needs the complete candidate set, so it does not resume mid-prefix from a cursor (the read-only collect phase simply restarts). A prefix with more than 200,000 candidate objects fails the rule loudly rather than rank a truncated set.

## Admin API

All endpoints are session-gated. Lifecycle shares the unified Jobs API: the job id is `lifecycle:<rule-name>`.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/api/admin/jobs` | All jobs, lifecycle rules included: status, pause flag, runtime state |
| `POST` | `/_/api/admin/jobs/lifecycle:<name>/preview` | Dry-run a rule and return candidate keys — read-only, no history rows, no leases |
| `POST` | `/_/api/admin/jobs/lifecycle:<name>/run-now` | Execute a rule synchronously; 409 if global/rule disabled, paused, or already running |
| `POST` | `/_/api/admin/jobs/lifecycle:<name>/pause` / `/resume` | Pause controls — persisted across restarts; paused rules are skipped by the scheduler and run-now alike |
| `GET` | `/_/api/admin/jobs/lifecycle:<name>/runs?limit=N` | Recent persisted executions, newest first |
| `GET` | `/_/api/admin/jobs/lifecycle:<name>/failures?limit=N` | Recent per-object failures, newest first |

Run-now and preview return `objects_scanned`, `objects_affected`, `objects_skipped`, `bytes_affected`, `errors`, a `candidates` array (bucket, key, action, destination coordinates, `created_at`, `size`), and a response-local `failures` array. `run_id` is present only for actual executions. `candidates` and response-local `failures` are capped by `max_failures_retained`; counters still reflect the whole run.

History rows include `id`, `triggered_by` (`scheduler` or `run-now`), `started_at`, `finished_at`, affected object/byte counters, `errors`, and terminal `status`. `objects_affected` / `bytes_affected` means deleted objects/bytes for delete rules and transitioned objects/copied bytes for transition rules. Failure rows include `run_id`, `bucket`, `object_key`, `occurred_at`, and `error_message`.

## Guardrails

Lifecycle skips:

- Directory markers (`folder/`).
- DeltaGlider config-sync/internal prefixes (`.deltaglider/**`, `.dg/**`).
- Storage artifacts if they ever leak through a backend listing (`reference.bin`, `*.delta`).
- Keys excluded by `exclude_globs`.
- Keys outside `include_globs` when includes are configured.
- Keys newer than `expire_after`.

Deletion is idempotent at the object level. A copy failure never deletes the source; a configured source delete runs only after the destination write verifies. Per-object failures are reported in the response and persisted in the config DB with the run id that observed them.

## Runtime state

The config DB stores:

- `lifecycle_state`: current `last_status`, `last_run_at`, `next_due_at`, lifetime expired-object/byte counters, and the active scheduler lease.
- `lifecycle_run_history`: one row per `run-now` or scheduler execution.
- `lifecycle_failures`: recent per-object failures, ring-bounded by `max_failures_retained` per rule.

The scheduler uses a per-rule DB lease so multiple proxy instances sharing the same config DB do not execute the same lifecycle rule concurrently. A boot-time reconciliation marks runs left in `running` by a dead process as `failed` and records an operator-visible failure row.

Runs persist a continuation cursor: a run interrupted by a crash or restart resumes from the stored cursor instead of rescanning from the top. A poison-token guard restarts the listing fresh exactly once if the stored cursor is rejected. Pause/resume state lives in the same row and survives restarts — parity with replication.

## Events

When a config DB is available, successful lifecycle deletes append a `LifecycleExpired` event to the durable event outbox with rule name, expiration age, object creation time, and content length. Successful transitions append `LifecycleTransitioned` with source/destination coordinates and copied bytes. If a transition rule also deletes the source, that source delete appends a `LifecycleExpired` event with action `transition-source-delete`.

## Deferred

- Multipart-upload cleanup.
