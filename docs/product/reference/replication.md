# Bucket replication

Engine-routed source → destination object copy, transparent to per-backend encryption and delta compression. Replication is event-driven: object mutations are copied in near-real time, with a periodic full reconcile as the self-healing safety net. Every copy goes through `engine.retrieve` → `engine.store`, so each side applies its own encryption and compression configuration independently.

## Triggers

Replication has two paths, primary and backstop:

- **Event-driven (primary).** Object mutations (PUT / DELETE / COPY / CompleteMultipartUpload) are appended to the durable `event_outbox` by the S3 write path. A per-process event consumer drains the outbox in near-real time over its own per-listener cursor (`WHERE id > cursor`, independent of the webhook-delivery listener), compacts a burst of events for one `(bucket, key)` into a single liveness verdict, and fans each surviving key out to every replication rule whose `source` matches. Copy-vs-skip / delete-vs-noop idempotency is the planner's job (`should_replicate` + a destination HEAD) — the same logic reconcile uses, so there is no separate per-key sync table. See [event-outbox.md](event-outbox.md) for the cursor/compaction model.
- **Full reconcile (safety net).** Each rule's `interval` (default 24h) schedules a full source-vs-destination reconcile that catches anything a dropped event missed. Events are the primary trigger; the reconcile is the backstop, not the main copy path. It runs as a directory-scoped tree walk (see below), so it starts copying within seconds of a run beginning rather than waiting on a full up-front scan.

## How the reconcile walk works

The full reconcile is a **directory-scoped tree walk**, not an up-front full listing. It descends the source and destination trees one directory pair at a time (delimiter-scoped listings), and syncs each folder the moment it is discovered:

- **Copying starts immediately** — the first folder is reconciled while the rest of the tree is still being discovered, so a run begins moving bytes within seconds regardless of tree size. `dir_concurrency` (default 4) controls how many directories are listed in parallel; object copies remain bounded by `transfers`.
- **Minimal backend requests.** Listings run in lite mode; a subtree missing on the destination is proven absent from the folder comparison itself, so its objects copy with no existence probes. When source and destination are both filesystem backends (whose listings carry authoritative metadata), an entire reconcile — initial sync or converged re-check — issues **zero** per-object HEAD requests. On S3 backends, only keys present on *both* sides are HEAD-resolved, in batches.
- **Fine-grained resume.** A killed, paused, or interrupted run resumes from its exact position in the tree — even mid-directory — via a scope-stamped cursor persisted as it goes.
- **Observability.** Prometheus counters `deltaglider_replication_list_calls_total`, `..._head_calls_total`, and `..._dirs_completed_total` expose the walk's I/O; the Jobs screen shows directories completed/pending for a running reconcile.

## Scope

- One-way, bucket/prefix-level replication through the DeltaGlider engine. The event consumer replicates mutations automatically; the reconcile scheduler runs due rules on their `interval`; a rule can also be triggered through the admin API (`POST /_/api/admin/jobs/replication:<name>/run-now`) or the Jobs screen.
- Disabled rules and paused rules are skipped by the event consumer, the reconcile scheduler, and run-now alike.
- A per-rule leader lease prevents two executions of the same rule at the same time. Single-instance (no `config_sync_bucket`) it is a node-local DB lease; with a coordination bucket configured it is an S3 conditional-write lease object (`_dgp/leases/replication/<rule>.json`) visible to every instance — a dead leader's lease lapses and a peer takes over automatically. If a rule is already leased, run-now returns `409 Conflict` and the scheduler skips that tick. Long runs heartbeat the lease before starting new pages/objects; if the lease is lost, the worker stops before doing more work and records a failure.
- At-least-once semantics. Conflict policies: `newer-wins` (default), `content-diff`, `skip-if-dest-exists`.
- Optional delete replication for destination objects previously written by the same rule.
- Optional include / exclude glob filters per rule.
- Static validation at config load: rule-name regex, humantime interval parsing, self-loop rejection, multi-hop cycle detection.

## YAML shape

```yaml
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
| `newer-wins` (default) | Copy only if source is strictly newer than destination. Ties fall through to skip — the clocks of two storage tiers aren't comparable. |
| `content-diff` | Keep the destination an exact mirror: copy only when the bytes differ (size differs, or both sides carry a logical SHA-256 and those differ). Byte-identical objects are skipped, so a recurring rule converges. |
| `skip-if-dest-exists` | Never copy when destination exists (seed-once semantics). |

## Delete replication

When `replicate_deletes: true`, a run also checks the rule's previously-written destination objects. If the corresponding source object no longer exists, the worker deletes the destination copy.

The guardrail is provenance: delete replication only targets objects that carry this rule's replication marker. Manually-created destination objects and objects written by a different rule are preserved.

## What doesn't replicate

- Directory markers (`folder/`) — destination recreates them on-demand.
- DeltaGlider-managed config-sync prefix (`.deltaglider/**`). This protects `.deltaglider/config.db` when the same bucket is also used for user data.
- Storage-layer delta artifacts (`reference.bin`, `*.delta`) are not normally visible to replication because the engine listing filters them before planning.
- Anything matched by `exclude_globs`.
- When `include_globs` is non-empty, only keys that match at least one pattern replicate.

## Durability model

- **Rules** are YAML-authored. Changes apply through the section PUT pipeline; cycle detection runs on every load.
- **Runtime state** lives in the encrypted config DB (`ConfigDb` v6):
    - `replication_state`: one row per rule. Scheduling state + pause flag + lifetime counters + resume cursor + leader lease columns (the node-local lease; with a coordination bucket the authoritative lease is the S3 lease object, and these columns back the run-now/worker bookkeeping on the leader). The resume cursor is a scope-stamped position in the reconcile walk's tree traversal — an interrupted run resumes exactly where it stopped; a cursor from an incompatible earlier format is discarded and the next run starts a fresh (idempotent) pass. `INSERT OR IGNORE` on config load preserves operator-set pause + lifetime counters across reloads.
    - `replication_run_history`: append-only per-run records. CASCADE DELETE on rule removal.
    - `replication_failures`: per-object error ring, bounded by `max_failures_retained`.
- **Boot reconciliation**: any `status='running'` rows left from a previous process are flipped to `failed` on startup with a diagnostic failure entry. Prevents zombie run rows.

## Static validation (`Config::check`)

Warnings (surfaced at startup; do not block config load):

- Invalid rule name (regex violation, >64 chars).
- Duplicate rule names (first wins).
- Interval unparseable or below 30s.
- `tick_interval` below 5s (scheduler anti-thrash).
- `batch_size` outside `[1, 10_000]`.
- `dir_concurrency` outside `[1, 16]`.
- Self-loop (source == destination).
- Multi-hop cycles (A→B + B→A with overlapping prefixes) — flagged with the full cycle path.
- Invalid include/exclude glob patterns.

## Transparency guarantees

Every copy goes through `engine.retrieve` → `engine.store`. That means:

- **Encryption**: source decrypts on read (regardless of mode — `aes256-gcm-proxy` / `sse-kms` / `sse-s3` / `none`); destination encrypts on write in its configured mode. The cryptographic boundary is per-backend.
- **Compression**: deltas reconstruct to plaintext on read; the destination applies its own `max_delta_ratio` / bucket policy. Cross-backend compression asymmetry is invisible.
- **Metadata**: `content-type` + user metadata are propagated. `multipart_etag` propagates verbatim if present on source.

## Failure modes

| Failure | Outcome |
|---|---|
| Source object deleted mid-run | Recorded as a per-object failure with "source retrieve failed". Run continues on the next object. |
| Destination backend down | `engine.store` error. Failure row captures the error message. Run reports `errors > 0`. |
| List fails (source bucket gone) | Entire run marked `failed` with a single "list source failed" row. |
| Planner error (malformed glob at runtime) | Entire run marked `failed`. Should never happen post-`Config::check`. |
| All copies error out | Run marked `failed` even if some objects were skipped legitimately. |
| Some copies error, some succeed | Run marked `succeeded` with `errors > 0` — lazy-sync catches up on the next tick. |

## Resumption

Long runs persist a continuation cursor, so a run interrupted mid-page (crash, restart) resumes where it left off instead of restarting from the top. A poison-token guard restarts a run fresh exactly once if the stored token is rejected by the backend.
