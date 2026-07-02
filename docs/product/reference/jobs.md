# Jobs

One API surface and one admin screen for everything that runs in the background: replication rules, lifecycle rules, and one-off maintenance jobs (bucket re-encryption, bucket migration).

## The model

Three subsystems, one surface. Every job appears as a row in `GET /_/api/admin/jobs` and on the Settings **Jobs** screen with the same normalized shape: kind, scope (bucket/prefix/target), status (`idle` / `queued` / `running` / `cancelling` / `succeeded` / `failed` / `cancelled`), progress, and last run. Job ids are namespaced:

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
| `POST` | `/_/api/admin/buckets/:bucket/migrate` | Create a migrate job: `{"target_backend", "delete_source"}` → `202` + `maintenance:<n>` |
| `GET` | `/_/api/admin/jobs/bucket/:bucket` | Busy state for one bucket; readable by non-admin browser sessions |

All routes except the last are session-gated admin routes.

## The write gate

While a re-encrypt or migrate job is active, S3 **writes** (PUT, DELETE, POST, multipart) to that bucket return `503 SlowDown`; AWS SDKs back off and retry automatically. Reads pass untouched. The gate engages at job creation (no create-to-claim window), drains in-flight writes before the copy starts, and lifts when the job finishes — for migrations, writes resume the moment the bucket flips to the new backend, before any optional source cleanup. The embedded object browser shows a busy banner on gated buckets via `GET /_/api/admin/jobs/bucket/:bucket`.

## Durability

Maintenance jobs live in the encrypted config DB (`maintenance_jobs` + failures tables) and are re-queued on boot: a proxy restart mid-job resumes the job rather than orphaning a half-migrated bucket. A cancel before a migration's routing flip unwinds cleanly; the source is never deleted on a failed or cancelled run. All three subsystems share the same leader-lease, failure-ring, and zombie-run machinery, and all paginated work goes through one cursor state machine with crash-resume and a one-shot poison-token guard.

## Related

- [Replication](replication.md) — rule shape, triggers, conflict policies.
- [Lifecycle rules](lifecycle.md) — expiration/transition rule shape, guardrails.
- [About encryption at rest](../explanation/encryption-at-rest.md) — what re-encryption rewrites.
- [Admin API](admin-api.md#jobs--one-surface-for-everything-background) — the full endpoint table.
- [About jobs, write gates, and durability](../explanation/jobs-and-durability.md)
- [How to move a bucket to another backend](../how-to/move-a-bucket-between-backends.md)
- [How to rotate or change encryption keys](../how-to/rotate-encryption-keys.md)
