# How to use a backend without conditional writes

Some backends, such as Backblaze B2, do not support conditional writes (compare-and-swap, or CAS). This guide explains how to use such a non-CAS backend safely, and it explains the proxy's backend capability validation: what it checks, when it refuses a configuration, the exact messages you'll see, and how to fix each one. Read it when a startup log or a config-apply error points you here, or before you put a low-cost backend such as Backblaze B2 into a multi-instance deployment.

## What the proxy validates, and why

Some S3 backends support conditional writes (`If-None-Match: *` create-if-absent and `If-Match: <etag>` compare-and-swap, or "CAS"). Others don't:

| Backend | Conditional writes |
|---|---|
| AWS S3 | Yes |
| MinIO (recent) | Yes |
| Ceph RGW (e.g. Hetzner Object Storage) | Yes |
| Backblaze B2 | No (rejects with HTTP 501) |

When you run multiple proxy instances against the same storage, CAS makes concurrent writes safe. Each delta-compressed prefix has a shared baseline (`reference.bin`). A single instance protects it with an in-process lock, and that lock does not span processes. With a sync bucket, the instances also take a lock object in the sync bucket, and every write of `reference.bin` is conditional on the version that the writer saw when it took the lock (`If-Match`, or `If-None-Match: *` for a new baseline). A backend that ignores these conditions cannot refuse a write from an instance that lost the lock, so two instances writing the same prefix on a non-CAS backend can silently corrupt the baseline.

For this reason, the proxy validates the capability up front, logs the result, and refuses the configurations that it cannot make safe:

| Role of the bucket | CAS needed? | What the proxy does |
|---|---|---|
| Coordination bucket (`config_sync_bucket`) | Always | Boot-time probe; refuses to start if the backend is non-CAS |
| Client-writable storage, multi-instance | Yes | Boot-time probe of every backend hosting such buckets; refuses to start if one is non-CAS. Config applies that would create this situation are rejected. |
| Client-writable storage, single-instance | No | Nothing to validate, because the in-process lock is sufficient |
| `replication_target_only` bucket (any backend, including B2) | No | Client writes return 403; replication is the only writer, so no cross-instance race exists |

"Multi-instance" means `config_sync_bucket` (or `DGP_CONFIG_SYNC_BUCKET`) is set. Without it, no probes run and no restriction applies.

## How the probe works

At startup the proxy writes a small object under an isolated key (`.deltaglider/_cwprobe/<random>`). Then it tests both conditions that the leases and locks use. It re-writes the object with `If-None-Match: *`, then with `If-Match` and a wrong ETag, and then with `If-Match` and the current ETag. The probe has three outcomes:

- **Verified**: the two wrong conditions are refused with `412 Precondition Failed` (or `409 ConditionalRequestConflict`, which AWS answers for a racing conditional write), and the write with the current ETag succeeds.
- **Definitively non-CAS**: a wrong condition is written (`200`), a condition is rejected with 501/NotImplemented, or the write with the current ETag is refused. A 200 means the condition was *silently ignored* (the dangerous case); a 501 is how Backblaze B2 answers conditional writes.
- **Anything else** (network error, missing bucket, a probe that takes too long): the verdict is *unverified*, which is different from non-CAS. The proxy logs a warning and continues, so that a transient error does not stop it. An unverified backend shows as such in the GUI until a later boot or config apply probes it successfully.

The proxy deletes the probe object afterwards. Because the probe keys are random, a fleet of instances that boot at once can validate concurrently without collisions. The proxy holds the per-backend verdicts in memory for the life of the process, keyed to the exact backend definition. When you redefine a backend (new endpoint, rotated credentials), the proxy probes it again. A plain restart also probes once (5 requests per backend).

The same probe validates the coordination bucket itself. For that bucket, the proxy caches the verdict across restarts in a witness object (`.deltaglider/coordination-witness.json`, 30 days) inside the coordination bucket.

## Using Backblaze B2 (or any non-CAS backend) as a replication target

Non-CAS backends work well as replication destinations. They give low-cost capacity for a mirror that nobody writes to directly. You declare that intent with the `replication_target_only` bucket marker.

The admin UI cannot set or clear this marker, so you set it in `deltaglider_proxy.yaml`. The Buckets page only shows it: a marked bucket carries a **replication target** chip, and its open row explains the marker.

In the example, the backend `aws-dr` points at a Backblaze B2 account (the name comes from the example cast), and the bucket `releases-dr` on it is the replication mirror of `releases`. This configuration goes in the `storage` section:

```yaml
# validate
advanced:
  config_sync_bucket: dgp-iam-sync     # multi-instance mode
storage:
  default_backend: hetzner-fsn1
  backends:
    - name: hetzner-fsn1
      type: s3
      endpoint: https://fsn1.your-objectstorage.com
      region: fsn1
      force_path_style: true
    - name: aws-dr
      type: s3
      endpoint: https://s3.eu-central-003.backblazeb2.com
      region: eu-central-003
      force_path_style: true
  buckets:
    releases-dr:
      backend: aws-dr
      replication_target_only: true
  replication:
    rules:
      - name: releases-to-dr
        source: { bucket: releases }
        destination: { bucket: releases-dr }
```

After you edit the file, restart the proxy, or apply the file to the running proxy with `POST /_/api/admin/config/apply` or with `deltaglider_proxy config apply deltaglider_proxy.yaml --server https://s3.acme.example`. The admin UI and the file hold the same configuration, so a later change in the UI keeps the marker ([two ways to configure DeltaGlider](../explanation/two-ways-to-configure.md)).

With the marker set:

- Client writes (PUT, DELETE, multipart, copy-into, browser uploads, and admin bulk copy, move and delete into the bucket) are refused with **403**.
- Replication rules that target the bucket work normally. The replication engine is the single writer, and that makes a non-CAS backend safe here.
- Reads (GET, HEAD, LIST) are unaffected. You can also publish prefixes read-only with `public_prefixes`, because a published mirror is a coherent setup.
- The bucket is exempt from the multi-instance CAS requirement, so the proxy starts even though B2 is non-CAS.

Keep one replication rule per destination prefix. The single-writer guarantee assumes that replication is the *only* writer. The proxy warns at config validation when it finds destination rules that overlap, a lifecycle rule that also writes into a marked bucket, or an unmarked second virtual bucket that aliases the same real storage. The marker protects a virtual name, so every alias that points at the protected bucket must carry it too.

## The messages, verbatim, and their fixes

### `403 — bucket is replication_target_only`

> Bucket 'X' is configured for replication targets only: client writes are disabled so replication remains the single writer — see https://deltaglider.com/docs/how-to/backend-capability-validation

You (or a client, or an admin bulk operation) tried to write to a bucket marked `replication_target_only`. If the bucket should accept client writes, remove the marker. In multi-instance mode on a non-CAS backend, the proxy rejects that removal (see the next section).

### `FATAL: backend '<name>' does not support conditional writes`

> FATAL: backend 'X' does not support conditional writes, but client-writable bucket(s) [...] route to it and multi-instance mode is active. Concurrent writes from two instances can corrupt delta references. Fix: move these buckets to a backend that supports conditional writes, or mark each as configured for replication targets only. — see https://deltaglider.com/docs/how-to/backend-capability-validation

The proxy refused to start. There are two fixes:

1. Move the affected buckets to a CAS-capable backend (AWS S3, recent MinIO, Ceph RGW).
2. Mark each affected bucket `replication_target_only: true`, if clients never write to it directly. Only YAML can set the marker (see the previous section).

If you run a single instance, unset `config_sync_bucket`, and the restriction disappears.

For fix 1 in the admin UI, while the proxy runs: open **Storage → Buckets** (`/_/admin/storage/buckets`), click the row of the bucket, for example `db-archive`, and select a CAS-capable backend such as `hetzner-fsn1` in **Backend**. Then click **Review & apply**, and then click **Apply and Persist**. A proxy that refused to start has no admin UI, so in that case change `backend:` of the bucket under `storage.buckets` in the file.

![The db-archive bucket is open and its Backend list is expanded; the arrow points at hetzner-fsn1, a backend with conditional writes.](/_/screenshots/backend-cas-reroute.webp)

A new route does not move the objects. The objects that are already on the old backend become invisible through the proxy until you move them with the migrate job: [How to move a bucket to another backend](move-a-bucket-between-backends.md).

### Config apply rejected: `config refused: backend '<name>' does not support conditional writes`

This is the same check at runtime, with the same message after `config refused:`. A `/config/apply` (or an admin GUI apply) tried to route a client-writable bucket to a known-non-CAS backend, or removed a `replication_target_only` marker that was keeping the setup safe. The proxy refuses the apply before anything changes. The fixes are the same as above.

### Warnings from `config lint` / validate

- *"bucket 'X' is configured for replication targets only but no replication rule targets it"*: the marker has no effect, because writes are blocked and nothing replicates into the bucket. Add a rule or remove the marker.
- *"lifecycle rule 'R' writes into bucket 'X' which is configured for replication targets only"*: lifecycle is a second internal writer, and it weakens the single-writer guarantee on non-CAS backends. This is safe on CAS backends. Reconsider it on B2.
- *"replication rules 'A' and 'B' both write into bucket 'X' with overlapping prefixes"*: two writers into one destination prefix break the single-writer guarantee. Give each rule a distinct destination prefix.

### `FATAL: coordination bucket validation failed`

> FATAL: coordination bucket validation failed: Coordination bucket 'X' does NOT enforce atomic conditional writes (If-None-Match). HA coordination (leases, single-writer locks) would be UNSAFE — refusing to start. ...

The bucket named by `config_sync_bucket` is on a non-CAS backend. The coordination bucket hosts leases, reference locks, and the synced config DB, so it must support CAS. Point `config_sync_bucket` at a CAS-capable backend.

## Verify

1. Check the startup log. The validation logs its result even when everything passes:

   ```
   backend capability: 'hetzner-fsn1' conditional writes verified (Probe)
   Backend capability gate skipped: single instance (config_sync_bucket not set)
   ```

   You see one line for each validated backend, or one line that says that the gate does not apply. If you see none of these lines, the proxy predates the gate.

2. Check the verdict in the admin UI. On **Storage → Backends** (`/_/admin/storage/backends`), a probed backend card shows **Conditional writes: verified by live probe — safe for multi-instance client writes**, **Conditional writes NOT supported**, or **Conditional-write support could not be verified**. A card without such a line was not assessed, for example on a single instance.

3. Check that a marked bucket refuses client writes. A PUT request answers `403`, and a read still works:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 cp probe.txt s3://releases-dr/probe.txt   # 403
   aws --endpoint-url https://s3.acme.example s3 ls s3://releases-dr/                     # works
   ```

## Related

- [How to run multiple instances (HA)](run-multiple-instances.md): the coordination bucket and what sync does
- [How to replicate a bucket](replicate-a-bucket.md): setting up the rule that writes into your mirror
- [How to route a bucket to a backend](route-a-bucket-to-a-backend.md): the `buckets:` routing table
- [How to move a bucket to another backend](move-a-bucket-between-backends.md): move the objects after a re-route
- [Multi-backend architecture](../explanation/multi-backend-architecture.md): why the reference baseline needs a single writer
