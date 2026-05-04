# Lazy bucket replication (v1)

*Engine-routed source → destination object copy, transparent to
per-backend encryption and delta compression. v1 ships automatic
scheduled runs, run-now, pause/resume, state, history, and delete
replication.*

## Why it lives in the proxy

- `aws s3 sync` outside the proxy bypasses delta compression and
  loses DG metadata on the wire.
- Storage-native replication (S3 CRR, filesystem rsync) can't cross
  encryption boundaries and bypasses the engine entirely.
- Application-level dual-write forces every client to know the
  secondary and inflates latency.

Replication lives at the engine seam: source GET plaintext →
destination PUT plaintext. Each side decides independently whether
to delta-compress and which encryption mode to apply.

![Object replication settings](/_/screenshots/object-replication.jpg)

## v1 scope

- One-way, bucket/prefix-level replication through the DeltaGlider
  engine. The scheduler runs due rules automatically; operators can
  also trigger a rule through the admin API or GUI.
- Rules carry `interval` and persisted `next_due_at` state. Disabled
  rules and paused rules are skipped by both the scheduler and run-now.
- A per-rule DB lease prevents the scheduler and run-now from executing
  the same rule at the same time. If a rule is already leased, run-now
  returns `409 Conflict` and the scheduler skips that tick. Long runs
  heartbeat the lease before starting new pages/objects; if the lease is
  lost, the worker stops before doing more work and records a failure.
- At-least-once semantics. Conflict policies: `newer-wins` (default),
  `source-wins`, `skip-if-dest-exists`.
- Optional delete replication for destination objects previously
  written by the same rule.
- Optional include / exclude glob filters per rule.
- Static validation at config load: rule-name regex, humantime
  interval parsing, self-loop rejection, multi-hop cycle detection.

## YAML shape

```yaml
storage:
  replication:
    enabled: true                    # master kill-switch
    tick_interval: "30s"             # scheduler poll rate (min 5s)
    lease_ttl: "60s"                 # failover window for a dead runner (min 15s)
    heartbeat_interval: "20s"        # lease renewal cadence (min 5s; must be < lease_ttl)
    max_failures_retained: 100       # per-rule failure ring size

    rules:
      - name: prod-to-backup
        enabled: true
        source:
          bucket: prod-artifacts
          prefix: ""                 # "" = entire bucket
        destination:
          bucket: backup-artifacts
          prefix: ""                 # optional remap
        interval: "15m"              # humantime (min 30s; used for next_due_at)
        batch_size: 100              # objects per scheduler yield
        replicate_deletes: false
        conflict: newer-wins
        include_globs: []
        exclude_globs: [".deltaglider/**"]
```

Rule-name grammar: `[A-Za-z0-9_.-]{1,64}`. Name is also the primary
key in the `replication_state` DB table.

## Admin API

All endpoints are session-gated (no IAM gating — replication is
operator-level storage config). Response shapes are JSON.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/api/admin/replication` | Overview: global + per-rule state. |
| `POST` | `/_/api/admin/replication/rules/:name/run-now` | Trigger a synchronous run. Returns 409 Conflict on a paused rule. |
| `POST` | `/_/api/admin/replication/rules/:name/pause` | Set paused=true. Persists across restarts. |
| `POST` | `/_/api/admin/replication/rules/:name/resume` | Clear the paused flag. |
| `GET` | `/_/api/admin/replication/rules/:name/history?limit=N` | Recent runs (default 20, max 100), including `triggered_by` (`scheduler`, `run-now`, or `unknown` for legacy rows). |
| `GET` | `/_/api/admin/replication/rules/:name/failures?limit=N` | Recent per-object failures. |

### Run-now response

```json
{
  "run_id": 42,
  "status": "succeeded",
  "objects_scanned": 3,
  "objects_copied": 3,
  "objects_skipped": 0,
  "bytes_copied": 15,
  "errors": 0
}
```

## Conflict policies

| Policy | Behavior |
|---|---|
| `newer-wins` (default) | Copy only if source is strictly newer than destination. Ties fall through to skip — the clocks of two storage tiers aren't comparable. |
| `source-wins` | Always copy, overwriting destination. |
| `skip-if-dest-exists` | Never copy when destination exists. Useful for seed-once rules. |

## Delete replication

When `replicate_deletes: true`, a run also checks the rule's
previously-written destination objects. If the corresponding source
object no longer exists, the worker deletes the destination copy.

The guardrail is provenance: delete replication only targets objects
that carry this rule's replication marker. Manually-created destination
objects and objects written by a different rule are preserved.

## What doesn't replicate

- Directory markers (`folder/`) — destination recreates them on-demand.
- DeltaGlider-managed config-sync prefix (`.deltaglider/**`). This
  protects `.deltaglider/config.db` when the same bucket is also used
  for user data.
- Storage-layer delta artifacts (`reference.bin`, `*.delta`) are not
  normally visible to replication because the engine listing filters
  them before planning.
- Anything matched by `exclude_globs`.
- When `include_globs` is non-empty, only keys that match at least one
  pattern replicate.

## Durability model

- **Rules** are YAML (GitOps-authored). Changes apply through the
  section PUT pipeline; cycle detection runs on every load.
- **Runtime state** lives in the encrypted config DB (`ConfigDb` v6):
    - `replication_state`: one row per rule. Scheduling state +
      pause flag + lifetime counters + continuation token + leader
      lease columns. `INSERT OR IGNORE` on config load preserves
      operator-set pause + lifetime counters across reloads.
    - `replication_run_history`: append-only per-run records. CASCADE
      DELETE on rule removal.
    - `replication_failures`: per-object error ring, bounded by
      `max_failures_retained`.
- **Boot reconciliation**: any `status='running'` rows left from a
  previous process are flipped to `failed` on startup with a
  diagnostic failure entry. Prevents zombie run rows.

## Static validation (`Config::check`)

Warnings (surfaced at startup; do not block config load):

- Invalid rule name (regex violation, >64 chars).
- Duplicate rule names (first wins).
- Interval unparseable or below 30s.
- `tick_interval` below 5s (scheduler anti-thrash).
- `batch_size` outside `[1, 10_000]`.
- Self-loop (source == destination).
- Multi-hop cycles (A→B + B→A with overlapping prefixes) — flagged
  with the full cycle path.
- Invalid include/exclude glob patterns.

## Transparency guarantees

Every copy goes through `engine.retrieve` → `engine.store`. That
means:

- **Encryption**: source decrypts on read (regardless of mode —
  `aes256-gcm-proxy` / `sse-kms` / `sse-s3` / `none`); destination
  encrypts on write in its configured mode. The cryptographic
  boundary is per-backend.
- **Compression**: deltas reconstruct to plaintext on read; the
  destination applies its own `max_delta_ratio` / bucket policy.
  Cross-backend compression asymmetry is invisible.
- **Metadata**: `content-type` + user metadata are propagated.
  `multipart_etag` (the H1 fix) propagates verbatim if present on
  source.

## Failure modes

| Failure | Outcome |
|---|---|
| Source object deleted mid-run | Recorded as a per-object failure with "source retrieve failed". Run continues on the next object. |
| Destination backend down | `engine.store` error. Failure row captures the error message. Run reports `errors > 0`. |
| List fails (source bucket gone) | Entire run marked `failed` with a single "list source failed" row. |
| Planner error (malformed glob at runtime) | Entire run marked `failed`. Should never happen post-`Config::check`. |
| All copies error out | Run marked `failed` even if some objects were skipped legitimately. |
| Some copies error, some succeed | Run marked `succeeded` with `errors > 0` — lazy-sync catches up on the next tick. |

## Deferred

- Continuation-token resumption for long runs that straddle ticks.
- Periodic scheduler loop that wakes on `tick_interval` and runs due
  rules automatically.
