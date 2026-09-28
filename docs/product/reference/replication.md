# Bucket replication

Replication copies objects from a source to a destination through the engine, so per-backend encryption and delta compression stay transparent. Replication is event-driven: the proxy copies object mutations in near-real time, and a periodic full reconcile is the safety net that repairs anything missed. Every copy goes through `engine.retrieve` → `engine.store`, so each side applies its own encryption and compression configuration independently.

## Triggers

Replication has two paths, primary and backstop:

- **Event-driven (primary).** The S3 write path appends object mutations (PUT / DELETE / COPY / CompleteMultipartUpload) to the durable `event_outbox`. A per-process event consumer drains the outbox in near-real time with its own per-listener cursor (`WHERE id > cursor`, independent of the webhook-delivery listener). It compacts a burst of events for one `(bucket, key)` into a single liveness verdict, and it fans each surviving key out to every replication rule whose `source` matches. The planner decides copy or skip, and delete or no-op (`should_replicate` + a destination HEAD), so the result is idempotent. Reconcile uses the same logic, so there is no separate per-key sync table. See [event-outbox.md](event-outbox.md) for the cursor/compaction model.
- **Full reconcile (safety net).** Each rule's `interval` (default 24h) schedules a full source-vs-destination reconcile that catches anything a dropped event missed. Events are the primary trigger, and the reconcile is the backstop. The reconcile runs as a directory-scoped tree walk (see below), so it starts to copy within seconds after a run begins, without waiting for a full up-front scan.

## How the reconcile walk works

The full reconcile is a directory-scoped tree walk instead of an up-front full listing. It descends the source and destination trees one directory pair at a time (delimiter-scoped listings), and it syncs each folder at the moment it discovers the folder:

- **Copying starts immediately.** The walk reconciles the first folder while it still discovers the rest of the tree, so a run starts to move bytes within seconds for any tree size. `dir_concurrency` (default 4) controls how many directories the walk lists in parallel. `transfers` still bounds the object copies.
- **Minimal backend requests.** Listings run in lite mode. When a subtree is missing on the destination, the folder comparison itself proves that it is absent, so its objects copy with no existence probes. When the source and the destination are both filesystem backends (whose listings carry authoritative metadata), an entire reconcile sends **zero** per-object HEAD requests, both for an initial sync and for a re-check of a converged pair. On S3 backends, the walk sends HEAD requests in batches, and only for keys present on *both* sides.
- **Fine-grained resume.** A killed, paused, or interrupted run resumes from its exact position in the tree, even in the middle of a directory. The run persists a scope-stamped cursor as it goes.
- **Observability.** Prometheus counters `deltaglider_replication_list_calls_total`, `..._head_calls_total`, and `..._dirs_completed_total` expose the walk's I/O; the Jobs screen shows directories completed/pending for a running reconcile.

## Scope

- One-way, bucket/prefix-level replication through the DeltaGlider engine. The event consumer replicates mutations automatically. The reconcile scheduler runs due rules on their `interval`. You can also start a rule through the admin API (`POST /_/api/admin/jobs/replication:<name>/run-now`) or the Jobs screen.
- The event consumer and the reconcile scheduler skip disabled rules and paused rules. A paused rule copies and deletes nothing, and the proxy does not keep for it the events that arrive during the pause. Resuming a paused rule makes it due at once, so the next scheduler tick starts a full reconcile from the start, which brings the destination in sync (with `replicate_deletes: true`, that run also applies the deletes of the pause). Run-now starts one run on request: it runs a disabled or paused rule once, and it does not change either flag.
- A per-rule leader lease prevents two executions of the same rule at the same time. On a single instance (no `config_sync_bucket`), it is a node-local DB lease. With a coordination bucket configured, it is an S3 conditional-write lease object (`_dgp/leases/replication/<rule>.json`) that every instance sees. The lease of a dead leader lapses, and a peer takes over automatically. The scheduler, the event consumer, run-now and rule delete all take this same lease. If a rule is already leased, run-now returns `409 Conflict` and the scheduler skips that tick. While another instance runs a reconcile of a rule, the event consumer holds that rule's events until the run ends. A heartbeat renews the lease every `heartbeat_interval` for the whole run, and the run releases the lease on every exit. When a renewal fails with an error, the heartbeat retries it while the next retry still lands before the lease expires. When the backend refuses a renewal, the worker has lost the lease: it stops before it does more work, and it records a failure.
- At-least-once semantics. Conflict policies: `newer-wins` (default), `content-diff`, `skip-if-dest-exists`.
- Optional delete replication (faithful mirror): the rule removes any destination object that is absent at the source. This requires a destination bucket dedicated to the rule.
- Optional include / exclude glob filters per rule.
- Static validation at config load: rule-name regex, humantime interval parsing, self-loop rejection, multi-hop cycle detection.

## YAML shape

```yaml
# validate
storage:
  replication:
    enabled: true                    # master kill-switch
    tick_interval: "30s"             # scheduler poll rate (min 5s)
    lease_ttl: "300s"                # failover window for a dead runner (min 15s; default 5m)
    heartbeat_interval: "60s"        # lease renewal cadence (min 5s; must be < lease_ttl)
    max_failures_retained: 100       # per-rule failure ring size
    dir_concurrency: 4               # concurrent directory listings in the reconcile walk (1-16); copies stay bounded by transfers

    rules:
      - name: mirror-releases-to-dr
        enabled: true
        source:
          bucket: releases
          prefix: ""                 # "" = entire bucket
        destination:
          bucket: releases-dr
          prefix: ""                 # optional remap
        interval: "24h"              # full-reconcile safety net (humantime, min 30s) — NOT the primary trigger
        batch_size: 100              # objects per scheduler yield
        replicate_deletes: false
        conflict: newer-wins
        include_globs: []
        exclude_globs: [".deltaglider/**"]
```

Rule-name grammar: `[A-Za-z0-9_.-]{1,64}`. The name is also the primary key in the `replication_state` DB table and the suffix of the job id (`replication:mirror-releases-to-dr`); see [Jobs](jobs.md) for the unified jobs API (run-now, pause/resume, runs, failures).

## Conflict policies

| Policy | Behavior |
|---|---|
| `newer-wins` (default) | Copy only if the source is strictly newer than the destination. A tie means skip, because the clocks of two storage tiers are not comparable. |
| `content-diff` | Keep the destination an exact mirror: copy only when the bytes differ (size differs, or both sides carry a logical SHA-256 and those differ). Byte-identical objects are skipped, so a recurring rule converges. |
| `skip-if-dest-exists` | Never copy when destination exists (seed-once semantics). |

A replica keeps the creation time of its source object, so its `LastModified` is the source's `LastModified` and not the time of the copy. This matters for `newer-wins`, because the policy compares these times. If a replica carried the copy time, it would look newer than its source, and a rule in the opposite direction would copy it back. It also matters for a lifecycle rule on the destination bucket, because that rule counts an object's age from its creation time. To read the creation time, the copy sends one extra metadata request to the source for each object that it copies.

## Delete replication

When `replicate_deletes: true`, the destination is a **faithful mirror** of the source: the rule deletes any destination object that is not present at the source, no matter who wrote it. This applies to both the scheduled reconcile and the event-driven path.

The only guardrail is source absence: the rule deletes a destination object only after a source HEAD confirms that the key is gone (a `NoSuchKey`). Any other error keeps the object. Delete replication removes *anything* absent at the source, including objects that other tools or a different rule wrote, so the destination bucket must be **dedicated to this rule**. A bucket shared with another writer is not a supported setup for delete replication.

## What doesn't replicate

- Directory markers (`folder/`). The destination recreates them on demand.
- DeltaGlider-managed config-sync prefix (`.deltaglider/**`). This protects `.deltaglider/config.db` when the same bucket is also used for user data.
- Storage-layer delta artifacts (`reference.bin`, `*.delta`) are not normally visible to replication because the engine listing filters them before planning.
- Anything matched by `exclude_globs`.
- When `include_globs` is non-empty, only keys that match at least one pattern replicate.

## Durability model

- Rules are YAML-authored. Changes apply through the section PUT pipeline, and cycle detection runs on every load.
- Runtime state lives in the encrypted config DB:
    - `replication_state`: one row per rule. Scheduling state + pause flag + lifetime counters + resume cursor + leader lease columns (the node-local lease; with a coordination bucket the authoritative lease is the S3 lease object, and these columns back the run-now/worker bookkeeping on the leader). The resume cursor is a scope-stamped position in the tree traversal of the reconcile walk, so an interrupted run resumes exactly where it stopped. The proxy discards a cursor in an incompatible earlier format, and the next run starts a fresh (idempotent) pass. `INSERT OR IGNORE` on config load preserves operator-set pause + lifetime counters across reloads.
    - `replication_run_history`: append-only per-run records. CASCADE DELETE on rule removal.
    - `replication_failures`: per-object error ring, bounded by `max_failures_retained`.
- Boot reconciliation: on startup, the proxy flips to `failed` any `status='running'` rows that a previous process left, and it adds a diagnostic failure entry. This prevents zombie run rows.

## Static validation (`Config::check`)

Warnings (surfaced at startup; do not block config load):

- Invalid rule name (regex violation, >64 chars).
- Duplicate rule names (first wins).
- Interval unparseable or below 30s.
- `tick_interval` below 5s (scheduler anti-thrash).
- `batch_size` outside `[1, 10_000]`.
- `dir_concurrency` outside `[1, 16]`.
- Self-loop (source == destination).
- Multi-hop cycles (A→B + B→A with overlapping prefixes). The warning shows the full cycle path.
- Invalid include/exclude glob patterns.

## Transparency guarantees

Every copy goes through `engine.retrieve` → `engine.store`. That means:

- **Encryption**: the source decrypts on read, in any mode (`aes256-gcm-proxy` / `sse-kms` / `sse-s3` / `none`). The destination encrypts on write in its configured mode. The cryptographic boundary is per-backend.
- **Compression**: deltas reconstruct to plaintext on read, and the destination applies its own `max_delta_ratio` and bucket policy. A difference in compression between the two backends is invisible.
- **Metadata**: the copy propagates `content-type` and user metadata. `multipart_etag` propagates verbatim if present on source.

## Failure modes

| Failure | Outcome |
|---|---|
| Source object deleted mid-run | Recorded as a per-object failure with "source retrieve failed". Run continues on the next object. |
| Destination backend down | `engine.store` error. Failure row captures the error message. Run reports `errors > 0`. |
| List fails (source bucket gone) | Entire run marked `failed` with a single "list source failed" row. |
| Planner error (malformed glob at runtime) | Entire run marked `failed`. Should never happen post-`Config::check`. |
| All copies error out | Run marked `failed` even if some objects were skipped legitimately. |
| Some copies error, some succeed | Run marked `completed_with_errors` with `errors > 0`. The next scheduled run tries the failed objects again. |

## Resumption

Long runs persist a continuation cursor, so a run interrupted mid-page (crash, restart) resumes where it left off instead of restarting from the top. A poison-token guard restarts a run fresh exactly once if the backend rejects the stored token.
