# Jobs

*One screen and one API for everything that runs in the background:
replication rules, lifecycle rules, and one-off maintenance jobs
(bucket re-encryption, bucket migration).*

## The model

Three subsystems, one surface. Every job appears as a row in
`GET /_/api/admin/jobs` and on the Settings **Jobs** screen with the same
normalized shape: kind, scope (bucket/prefix/target), status
(`idle` / `queued` / `running` / `cancelling` / `succeeded` / `failed` /
`cancelled`), progress, and last run. Job ids are namespaced:

| Kind | Id | Defined by | Actions |
|---|---|---|---|
| Replication rule | `replication:<name>` | YAML (`storage.replication.rules[]`) | pause, resume, run-now |
| Lifecycle rule | `lifecycle:<name>` | YAML (`storage.lifecycle.rules[]`) | pause, resume, run-now, preview |
| Maintenance one-off | `maintenance:<n>` | created via API/GUI, stored in the config DB | cancel |

Rules are recurring and YAML-authored (GitOps-friendly); maintenance jobs are
one-offs born in the DB. Actions outside a kind's matrix return `405` with the
supported list. `GET /jobs/:id/runs` and `GET /jobs/:id/failures` work for all
kinds — a one-off synthesizes a single run, because the job IS its run.

## Re-encrypt a bucket

`POST /_/api/admin/jobs/reencrypt` with `{"buckets": ["a", "b"]}` (max 100), or
**Jobs → + New job → Re-encrypt buckets…**. One durable job per bucket. The
worker rewrites every object whose stored encryption markers don't match the
backend's currently-configured encryption — that covers enabling encryption on
a plaintext bucket, decrypting after switching a backend to `none`, and key
rotation via the `legacy_key` shim. Progress (objects + bytes) is visible on
the job row. The Backends page proposes a re-encrypt job automatically when you
change a backend's encryption settings.

## Migrate a bucket between backends

`POST /_/api/admin/buckets/:bucket/migrate` with
`{"target_backend": "new", "delete_source": false}` → `202 Accepted` and a
`maintenance:<n>` job, or **Storage → Buckets → (bucket) → Migrate data…**.
The job stages the destination, copies through the engine (encryption and
delta stay transparent — each side applies its own config), verifies, flips
the bucket's routing, and cleans up. `delete_source` defaults to `false` —
the safe path leaves the source copy for you to remove later. Cancel before
the flip unwinds cleanly; the source is never deleted on a failed or
cancelled run.

## The write gate

While a re-encrypt or migrate job is active, S3 **writes** (PUT, DELETE, POST,
multipart) to that bucket return `503 SlowDown` — AWS SDKs back off and retry
automatically, so a deploy pipeline stalls briefly instead of failing. Reads
pass untouched. The gate engages at job creation (no create-to-claim window),
drains in-flight writes before the copy starts, and lifts when the job
finishes — for migrations, writes resume the moment the bucket flips to the
new backend, before any optional source cleanup. The embedded object browser shows a busy banner on gated
buckets (`GET /_/api/admin/jobs/bucket/:bucket` — readable by non-admin
browser sessions too).

## Durability

Maintenance jobs live in the encrypted config DB (`maintenance_jobs` +
failures tables) and are re-queued on boot: a proxy restart mid-job resumes the
job rather than orphaning a half-migrated bucket. All three subsystems share
the same leader-lease, failure-ring, and zombie-run machinery, and all
paginated work goes through one cursor state machine with crash-resume and a
one-shot poison-token guard.

## Related

- [Replication](replication.md) — rule shape, triggers, conflict policies.
- [Lifecycle rules](lifecycle.md) — expiration/transition rule shape, guardrails.
- [Encryption at rest](encryption-at-rest.md) — what re-encryption rewrites and the rotation recipes.
- [Admin API](admin-api.md#jobs--one-surface-for-everything-background) — the full endpoint table.
