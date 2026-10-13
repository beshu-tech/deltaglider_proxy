# Lifecycle rules

Lifecycle rules act on engine-visible objects. A rule deletes old objects by age, keeps the newest *N* by count (`retain-newest`), or transitions (archives) objects to another bucket or prefix. Lifecycle rules appear on the unified Jobs surface (`GET /_/api/admin/jobs`, job id `lifecycle:<rule-name>`); see [Jobs](jobs.md).

## Scope

Lifecycle does not implement AWS XML lifecycle compatibility and does not scan raw storage artifacts. Every delete goes through `engine.delete`; every transition goes through the same shared engine transfer primitive used by replication (`engine.retrieve` → `engine.store` / `store_with_multipart_etag`). DeltaGlider metadata, reference cleanup, encryption wrappers, multipart ETag preservation, provenance metadata, and event outbox behavior stay on the same paths as normal S3/replication operations.

Lifecycle is disabled by default. A rule has to be present, the global switch must be `enabled: true`, and the rule itself must be `enabled: true` before automatic scheduler or run-now execution deletes anything. Preview is available even while disabled and stays read-only: it does not create run-history rows or take a lease.

## YAML grammar

```yaml
# validate
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
# fragment
        action:
          type: transition          # "archive" is accepted as an alias
          destination:
            bucket: db-archive
            prefix: "cold/nightly/"
          delete_source_after_success: false
```

`delete_source_after_success: false` makes transition an archive/copy; `true` gives move semantics. Transition is copy-first: lifecycle copies, verifies the destination HEAD when possible, and deletes the source only after the copy succeeds. The copy keeps the creation time of the source object, so the archived object shows its original `LastModified`, and a lifecycle rule on the archive bucket counts the object's age from its creation, not from the transition.

### Count-based retention: `retain-newest`

`retain-newest` keeps the newest `count` objects in a prefix and deletes the rest. It selects by *count* instead of age, which native S3 lifecycle does not offer. `expire_after` does not apply to a `retain-newest` rule and may be omitted.

```yaml
# fragment
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

- `count` (required, ≥ 1): how many of the newest *qualifying* objects to keep. The rule ranks objects by `created_at` descending, with a deterministic key-descending tie-break (stable across runs).
- `qualify` (optional): an eligibility filter, not a delete guard. An object that fails it is *invisible* to the rule: the rule never counts it toward `count` and never deletes it. The filter stops an accidental empty or truncated file (a stray `README`, a half-written dump) from taking a place in the keep set and pushing a real backup into the delete set.
  - `min_size_bytes`: the object's *original* (hydrated) size must be ≥ this value. It guards against empty and placeholder files.
  - `min_age` (humantime): the object must be older than this value. It stops the rule from counting in-flight uploads before they finish.
- `protect_younger_than` (optional, humantime): a delete-side guard. The rule spares an object that it selected for deletion *in this run* if the object is younger than this value. The rule never promotes the object into the keep set. In a later run, when the object is older, normal ranking applies. Most rules omit it.

The two settings differ on purpose. `qualify.min_age` means "too young to count yet" (ignored). `protect_younger_than` means "old enough to count, but do not physically delete it yet" (spared). Preview reports `objects_ignored` and `objects_protected`, so you see the result for each object before anything runs.

Unlike an age rule, a `retain-newest` run is **atomic per execution**. Its keep/delete decision needs the complete candidate set, so it does not resume from a cursor in the middle of a prefix. Instead, the read-only collect phase restarts. When a prefix has more than 200,000 candidate objects, the rule fails with an explicit error instead of ranking a truncated set.

## Admin API

All endpoints are session-gated. Lifecycle shares the unified Jobs API: the job id is `lifecycle:<rule-name>`.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/api/admin/jobs` | All jobs, lifecycle rules included: status, pause flag, runtime state |
| `POST` | `/_/api/admin/jobs/lifecycle:<name>/preview` | Dry-run a rule and return candidate keys. Read-only: no history rows, no leases. For 60 seconds, a preview of the same rule definition answers from the last walk, and previews that arrive while a walk runs wait for that walk. A run of the rule drops its saved previews |
| `GET` | `/_/api/admin/jobs/lifecycle:<name>/preview` | The progress of the preview walk of the rule that runs now: `{running, started_at, objects_scanned, objects_affected}`. The counters change after each listing page. When no walk runs, the answer is `running: false`. This request starts no walk. A retain-newest rule knows its matches only at the end of its walk, so `objects_affected` stays 0 for it while the walk runs |
| `POST` | `/_/api/admin/jobs/lifecycle:<name>/run-now` | Start a run in the background: `202` with `run_id` and `status: "running"`; `409` if lifecycle or the rule is disabled, the rule is paused or already running, two rules share its name, or a maintenance job is active on a bucket that the rule writes to; `503` while the backend health check marks the backend of a bucket that the rule writes to as unavailable; `400` if the rule has a config error |
| `POST` | `/_/api/admin/jobs/lifecycle:<name>/pause` / `/resume` | Pause controls. The pause persists across restarts, and both the scheduler and run-now skip a paused rule |
| `GET` | `/_/api/admin/jobs/lifecycle:<name>/runs?limit=N` | Recent persisted executions, newest first |
| `GET` | `/_/api/admin/jobs/lifecycle:<name>/failures?limit=N` | Recent per-object failures, newest first |

Preview returns `objects_scanned`, `objects_affected`, `objects_skipped`, `bytes_affected`, `errors`, a `candidates` array (bucket, key, action, destination coordinates, `created_at`, `size`), and a response-local `failures` array. `candidates` and `failures` are capped by `max_failures_retained`; counters still reflect the whole sweep.

The preview also returns `computed_at`, which is the time at which the walk ended, and `next_due_at`. The `next_due_at` field is the earliest time at which a scanned object that the rule does not match now will match. The planner compares the creation time of each object with `expire_after`, so an object becomes due at its creation time plus `expire_after`. The proxy reads these times from the metadata that the walk already has, so the field costs no extra request. Objects that the globs exclude never count. The field is absent when no scanned object will ever match, and for a retain-newest rule, because that rule has no age condition of its own.

Run-now does not wait for the run. The proxy opens the run-history row, starts the run in the background, and answers `202` with the new `run_id` and `status: "running"`; the counters in that response are zero. To get the result, poll `GET /_/api/admin/jobs/lifecycle:<name>/runs` until the row with that `id` has a terminal `status`. The row carries the counters (`objects_processed` is the affected count) and `errors`, and the failures of the run are in `GET …/failures`. The Jobs screen polls in the same way.

History rows include `id`, `triggered_by` (`scheduler` or `run-now`), `started_at`, `finished_at`, affected object/byte counters, `errors`, and terminal `status`. `objects_affected` / `bytes_affected` means deleted objects/bytes for delete rules and transitioned objects/copied bytes for transition rules. Failure rows include `run_id`, `bucket`, `object_key`, `occurred_at`, and `error_message`.

## Guardrails

Lifecycle skips:

- Directory markers (`folder/`).
- DeltaGlider config-sync/internal prefixes (`.deltaglider/**`, `.dg/**`).
- Storage artifacts if they ever leak through a backend listing (`reference.bin`, `*.delta`).
- Keys excluded by `exclude_globs`.
- Keys outside `include_globs` when includes are configured.
- Keys newer than `expire_after`.

Deletion is idempotent at the object level. A copy failure never deletes the source; a configured source delete runs only after the destination write verifies. The proxy reports per-object failures in the response and persists them in the jobs DB with the id of the run that observed them.

## Runtime state

The jobs DB (`deltaglider_jobs.db`, next to the config DB, never synced) stores:

- `lifecycle_state`: current `last_status`, `last_run_at`, `next_due_at`, lifetime expired-object/byte counters, and the active scheduler lease.
- `lifecycle_run_history`: one row per `run-now` or scheduler execution.
- `lifecycle_failures`: recent per-object failures, ring-bounded by `max_failures_retained` per rule.

The scheduler and run-now take a per-rule lease in the jobs DB, so two workers of one instance never run the same rule at the same time. The lease lasts 5 minutes and is renewed every 60 seconds; `advanced.jobs.lease_ttl` and `advanced.jobs.heartbeat_interval` change these values (see [Job leases](configuration.md#job-leases)). The lease is node-local, because the config sync does not carry the jobs DB, so two instances can run the same rule at the same time. A boot-time reconciliation marks runs left in `running` by a dead process as `failed` and records an operator-visible failure row.

Runs persist a continuation cursor: a run interrupted by a crash or restart resumes from the stored cursor instead of rescanning from the top. A listing that fails because the backend is slow or answers with a 5xx status is sent again with the same cursor, up to 5 times. Only a cursor that the backend refuses as invalid restarts the listing fresh, exactly once.

A run reads the creation time and the logical size of each delta object with a `HEAD` request. The proxy keeps these answers from one run to the next, for each object, together with the ETag, the size and the Last-Modified time that the listing shows for the stored object. An object whose stored version did not change therefore costs no `HEAD` request on the hourly rescan, and a run right after a preview sends no `HEAD` request for it either. An overwrite changes the stored version, so the next run reads the object again. The proxy also keeps these answers in its node-local jobs database (`deltaglider_jobs.db`), so the first preview or run after a restart or a configuration change sends no `HEAD` request for an unchanged object either. The jobs database keeps at most 200,000 such answers, and it drops an answer when no run or preview listed the object for 7 days. When the proxy cannot open the jobs database, it keeps the answers in memory only. The deletes of one listing page go to the engine as one batch: each delete still happens only while the object is the one that the run planned on, and the proxy checks each folder once for a `reference.bin` to remove.

While the backend health check marks the backend of a bucket that the rule writes to as unavailable, the scheduler does not start the rule. The rule stays due and runs on a tick after the backend recovers. Pause/resume state lives in the same row and survives restarts, as in replication.

## Events

When a jobs DB is available, successful lifecycle deletes append a `LifecycleExpired` event to the durable event outbox with rule name, expiration age, object creation time, and content length. Successful transitions append `LifecycleTransitioned` with source/destination coordinates and copied bytes. If a transition rule also deletes the source, that source delete appends a `LifecycleExpired` event with action `transition-source-delete`.

## Deferred

- Multipart-upload cleanup.
