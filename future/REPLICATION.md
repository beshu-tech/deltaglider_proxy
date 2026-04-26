# Eventually-Consistent Background Replication

> **Status:** superseded by shipped lazy bucket replication.
> Current operator docs live in
> [`docs/product/reference/replication.md`](../docs/product/reference/replication.md).
> The shipped design is rule-based source → destination object
> replication through the DeltaGlider engine, with scheduler controls,
> run-now, pause/resume, history, failures, and provenance-guarded
> delete replication. This planning note is kept for archaeology only.

## Value Proposition

Async copy of every write to a secondary backend. Primary = local filesystem (fast writes), secondary = cheap cloud (durability). Not real-time cross-region replication — an eventually-consistent background sync.

## Design

- Config per bucket: `replicate_to = "cloud-backup"` (a named backend from `[[backends]]`)
- On successful PUT/DELETE → enqueue replication task
- Background workers process queue: copy from primary to secondary
- Reads always go to primary. Writes replicate async.
- Replication status per object: pending, synced, failed
- Admin GUI: replication lag dashboard, failed items list
- Manual failover on primary failure (not automatic HA)

## Architecture

- **Queue**: In-memory bounded channel + persist to SQLite for crash recovery
- **Workers**: Configurable concurrency (default 4 parallel copies)
- **Conflict resolution**: Last-writer-wins (monotonic sequence numbers, not timestamps)
- **Monitoring**: Prometheus metrics `replication_pending_count`, `replication_lag_seconds`

## Delta Compression Interaction

**Replicate raw stored bytes** (deltas, references, passthroughs) — NOT reconstructed objects.

- Secondary gets a mix of deltas, references, and passthroughs
- Secondary is NOT independently readable without DGP (deltas need reference.bin)
- For DR: replicate BOTH delta AND reference.bin
- **Alternative**: replicate reconstructed (decompressed) files. Pros: secondary readable by other tools. Cons: no compression savings, higher bandwidth.
- **Recommendation**: Raw bytes. Secondary is a backup for DGP, not a standalone system.

## Encryption Interaction

- If encryption enabled, replicated blobs are already encrypted — safe over the wire
- Encryption key must be same on recovery (part of DGP config, not per-backend)

## Multipart Interaction

- Replication happens AFTER `engine.store()` — the assembled, compressed, (encrypted) object is replicated
- No interaction with multipart assembly. Clean.

## Failure Modes

| Failure | Handling |
|---------|----------|
| Backend unreachable | Queue grows. Bounded (10K items) + disk spillover via SQLite. |
| DELETE replication | Must replicate deletes too, otherwise secondary accumulates garbage. |
| DGP crash | Persist queue to SQLite. On restart, replay pending items. |
| Initial sync | "Full sync" mode when first enabled on existing bucket. Background, interruptible. |
| Split brain | NOT possible — one primary. Secondary is write-only (from replication). |

## Performance

- Network bandwidth: 1:1 with write throughput (every PUT replicated)
- CPU: minimal (just COPY, no re-compression)
- Memory: bounded queue + one in-flight object per worker
- **Latency on PUT: ZERO** (replication is async, post-write)
- Optional bandwidth cap per worker

## Effort

~2 weeks.

## Addresses

Stefano requirement: Replica
