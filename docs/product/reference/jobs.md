# Jobs

One API surface and one admin screen for everything that runs in the background: replication rules, lifecycle rules, and one-off maintenance jobs (bucket re-encryption, bucket migration, metadata backfill).

## The model

Three subsystems, one surface. Every job appears as a row in `GET /_/api/admin/jobs` and on the Settings **Jobs** screen with the same normalized shape: kind, scope (bucket/prefix/target), status (`idle` / `queued` / `running` / `cancelling` / `succeeded` / `completed_with_errors` / `failed` / `cancelled`), progress, and last run. A one-off job that runs to its end with one or more failed objects is never `succeeded`: it is `completed_with_errors` when some objects went through, and `failed` when none did. Its row shows the failure count, which opens the job's Failures tab. Job ids are namespaced:

| Kind | Id | Defined by | Actions |
|---|---|---|---|
| Replication rule | `replication:<name>` | YAML (`storage.replication.rules[]`) | pause, resume, run-now, verify, kill, delete |
| Lifecycle rule | `lifecycle:<name>` | YAML (`storage.lifecycle.rules[]`) | pause, resume, run-now, preview, delete |
| Maintenance one-off | `maintenance:<n>` | created via API/GUI, stored in the config DB | cancel |

`run-now` is a deliberate one-off. For **replication** it runs even a disabled or paused rule once (without flipping the flag); for **lifecycle** it returns `409` on a disabled or paused rule. `kill` interrupts a running replication run mid-object (replication only). `verify` runs a parity audit; it returns `409` while a replication run is in flight for the same rule. `delete` refuses (`409`) while the rule has a run or verify in progress.

Rules are recurring and YAML-authored; maintenance jobs are one-offs born in the DB. An action outside a kind's capability matrix returns `405` with the supported list. `GET /jobs/:id/runs` and `GET /jobs/:id/failures` work for all kinds — a one-off synthesizes a single run, because the job is its run.

## API

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/api/admin/jobs` | All jobs, normalized rows |
| `GET` | `/_/api/admin/jobs/:id/runs?limit=N` | Recent runs for one job |
| `GET` | `/_/api/admin/jobs/:id/failures?limit=N` | Recent per-object failures |
| `POST` | `/_/api/admin/jobs/:id/pause` / `resume` / `run-now` / `preview` / `cancel` / `verify` / `kill` / `delete` | Per-kind actions; `405` outside the matrix |
| `POST` | `/_/api/admin/jobs/reencrypt` | Create re-encrypt jobs: `{"buckets": [...]}` (max 100), one job per bucket |
| `POST` | `/_/api/admin/jobs/backfill-metadata` | Create metadata-backfill jobs: `{"buckets": [...], "refresh_last_modified": false}` (max 100), one job per bucket |
| `POST` | `/_/api/admin/buckets/:bucket/migrate` | Create a migrate job: `{"target_backend", "delete_source", "target"}` → `202` + `maintenance:<n>`. `target` is `empty` (default: the job fails in `stage` when the destination holds objects) or `mirror` (destination objects absent at the source are deleted before the flip, audited) |
| `GET` | `/_/api/admin/jobs/bucket/:bucket` | Busy state for one bucket; readable by non-admin browser sessions |

All routes except the last are session-gated admin routes.

## Metadata backfill

An object that reached the backend without the proxy (it was there before the proxy, or another tool wrote it) has none of the proxy's metadata: no content hash, no created-at. The proxy still serves it, but it cannot show or verify its checksum. The `backfill-metadata` job adds that metadata. It reads each such object once to compute its hashes and then rewrites only the metadata: an S3 backend does a server-side copy of the object onto itself, and a filesystem backend rewrites the extended attributes. The object bytes are not uploaded again, and objects that the proxy wrote are skipped.

By default the job keeps the Last-Modified time that the proxy serves for each object, so sync tools and replication do not copy the objects again. Set `refresh_last_modified: true` to make the backfilled objects read as modified at the time of the job. A multipart object keeps the ETag that clients know. Start the job from **Settings → Jobs → New job → Backfill metadata…**, or with the API row above.

## The write gate

While a re-encrypt, migrate or backfill job is active, S3 **writes** (PUT, DELETE, POST, multipart) to that bucket return `503 SlowDown`; AWS SDKs back off and retry automatically. Reads pass untouched. The gate engages at job creation (no create-to-claim window), drains in-flight writes before the copy starts, and lifts when the job finishes — for migrations, writes resume the moment the bucket flips to the new backend, before any optional source cleanup. The embedded object browser shows a busy banner on gated buckets via `GET /_/api/admin/jobs/bucket/:bucket`.

## Durability

Maintenance jobs live in the encrypted config DB (`maintenance_jobs` + failures tables) and are re-queued on boot: a proxy restart mid-job resumes the job rather than orphaning a half-migrated bucket. A migrate checks for a cancel every 20 objects. A cancel (or a failure) before the routing flip releases the write gate on the source bucket at once, deletes the copies that this job wrote to the destination (it finds them by the job id in their `dg-migration` metadata, so objects that the job did not write stay), and removes the staging route. The source is never deleted on a failed or cancelled run. All three subsystems share the same leader-lease, failure-ring, and zombie-run machinery (replication's lease upgrades to a cross-instance S3 lease when a coordination bucket is configured; lifecycle and maintenance leases stay node-local), and all paginated work goes through one cursor state machine with crash-resume and a one-shot poison-token guard.

## Related

- [Replication](replication.md) — rule shape, triggers, conflict policies.
- [Lifecycle rules](lifecycle.md) — expiration/transition rule shape, guardrails.
- [About encryption at rest](../explanation/encryption-at-rest.md) — what re-encryption rewrites.
- [Admin API](admin-api.md#jobs--one-surface-for-everything-background) — the full endpoint table.
- [About jobs, write gates, and durability](../explanation/jobs-and-durability.md)
- [How to move a bucket to another backend](../how-to/move-a-bucket-between-backends.md)
- [How to rotate or change encryption keys](../how-to/rotate-encryption-keys.md)
