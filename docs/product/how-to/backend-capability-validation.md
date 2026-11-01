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

Before it deletes the probe object, the proxy also sends a `DELETE` with `If-Match` and a wrong ETag. A backend that enforces the condition refuses this request with `412 Precondition Failed` and keeps the object. This step does not change the CAS verdict. It decides how an instance releases a lock or a lease in the coordination bucket. When the backend enforces the condition, the instance deletes the lock with `If-Match` and the ETag of its own last write, so a lock that a peer took over in the meantime stays in place. When the backend does not enforce it, the instance reads the lock first and deletes it only if the lock is still its own and has not expired. An expired lock is free to every instance, and a peer can take it over between the read and the delete, so the instance leaves an expired lock in place. That costs one more request per release.

The proxy deletes the probe object afterwards, and on a bucket that keeps versions it also deletes every version of the probe key, so no noncurrent version or delete marker stays. Because the probe keys are random, a fleet of instances that boot at once can validate concurrently without collisions. The proxy holds the per-backend verdicts in memory for the life of the process, keyed to the exact backend definition. When you redefine a backend (new endpoint, rotated credentials), the proxy probes it again. A plain restart also probes once (7 requests per backend, plus one for each version of the probe key on a versioned bucket). A re-encrypt job on a backend without a verdict runs the same probe in the bucket it converts, when it is queued.

The same probe validates the coordination bucket itself. For that bucket, the proxy caches the verdict across restarts in a witness object (`.deltaglider/coordination-witness.json`, 30 days) inside the coordination bucket. The witness also records the result of the conditional `DELETE` step. A witness that an older release wrote does not have this result, so the next boot probes the bucket again and rewrites the witness.

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

## Encryption needs a backend with atomic writes

Proxy encryption (`aes256-gcm-proxy`) works only on a backend whose conditional writes the probe proves. This rule applies with one instance too. The proxy rewrites encrypted objects in place when it converts a bucket or rotates a key, and each rewrite is conditional on the version that it read. A backend that ignores the condition could let a rewrite overwrite a newer client write. The full rule is in the [encryption reference](../reference/encryption.md#encryption-needs-atomic-writes). Native SSE modes (`sse-s3`, `sse-kms`) are outside the rule, because the backend encrypts those objects itself.

The rule has two effects:

- The proxy refuses every action that turns proxy encryption on (or changes its key) on such a backend. It runs a fresh probe first, and it answers `409` with an `atomic_writes` object that names the backend and the probe result.
- A bucket that already encrypts with the proxy on such a backend becomes read-only. Reads keep working, and every write gets `403 AccessDenied` with a message that names the bucket, the backend and the reason.

A Backblaze B2 backend is such a backend. B2 drops the connection on a conditional `PUT`. The probe sends a plain `PUT` next, and because that one works, the probe reads the dropped connection as a refusal of the condition (verdict `non-cas`).

### Read the verdict

In the admin UI:

1. In the sidebar, open **Storage → Backends** (`/_/admin/storage/backends`).
2. Find the card of the backend, for example `aws-dr`. The card shows the verdict of the last probe:
   - **Conditional writes: verified by live probe** for an S3 backend with verdict `cas-verified` (`via: probe`). A filesystem backend has atomic writes without a probe (`via: local-filesystem`).
   - **Conditional writes NOT supported** for verdict `non-cas`. Proxy encryption is not offered on this backend.
   - **Conditional-write support could not be verified** for verdict `unknown`. Proxy encryption is not offered on this backend either.
3. When the backend holds encrypted buckets that are read-only, the card shows **Read-only: aws-dr does not support atomic writes** and lists those buckets. The Buckets page shows the same text on the card of each such bucket.
4. Click **Check again** to run a fresh probe now. When the backend now proves atomic writes, its encrypted buckets become writable at once, without a restart.

On the Buckets page and in the backend encryption editor, the proxy-AES options are disabled on a backend whose verdict is `non-cas` or `unknown`. The reason and a link to this page show under the options. Turning encryption off stays possible.

The same check with the admin API:

```bash
curl -b cookies https://s3.acme.example/_/api/admin/backends
curl -b cookies -X POST https://s3.acme.example/_/api/admin/backends/aws-dr/capability-probe
```

The first request lists `capability` and `read_only_buckets` for each backend. The second request runs the probe and answers `{backend, capability, probe_result, read_only_buckets}`.

### Make a read-only bucket writable again

The example bucket `releases` encrypts with the proxy on `aws-dr`, a Backblaze B2 backend, so it is read-only. Move it to `hetzner-fsn1`, which enforces conditional writes. A migrate job out of a read-only bucket is allowed: it only reads the source, and its last phase deletes the old copies only when you ask for that.

In the admin UI:

1. In the sidebar, open **Storage → Buckets** (`/_/admin/storage/buckets`).
2. Click the `releases` row to open it.
3. Click **Migrate data…** next to **Backend**.
4. In the dialog, select `hetzner-fsn1` in **Target backend**, and then click **Start migration**.

The proxy probes `hetzner-fsn1` before it starts the job. When `hetzner-fsn1` has no atomic writes either, the proxy refuses the job with `409 atomic_writes_required`. The full steps and the phases of the job are in [How to move a bucket to another backend](move-a-bucket-between-backends.md).

A migrate job has no YAML form, because it is a one-off job and not configuration. The same job with the admin API:

```bash
curl -b cookies -X POST \
  https://s3.acme.example/_/api/admin/buckets/releases/migrate \
  -H 'Content-Type: application/json' \
  -d '{"target_backend": "hetzner-fsn1", "delete_source": false}'
```

The other way out is to turn proxy encryption off for the bucket. That change is a downgrade, so it needs the confirmation header `X-DGP-Confirm-Encryption-Downgrade: releases` (see [A downgrade needs a confirmation](../reference/encryption.md#a-downgrade-needs-a-confirmation)). The new objects are then plaintext. A decrypt job converts the old objects. On a backend without atomic writes, that job keeps the bucket's write gate, so uploads and deletes on the instance that runs it get a temporary `503 SlowDown`.

### Verify

1. Open **Storage → Buckets**. The card of `releases` shows no **Read-only** text, and its backend is `hetzner-fsn1`.
2. Write an object:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 cp probe.txt s3://releases/probe.txt   # works
   ```

3. `GET /_/api/admin/buckets` shows no `read_only` field on `releases`.

## The messages, verbatim, and their fixes

### `409 — atomic_writes_required`

> encryption of bucket 'releases' refused: encryption needs atomic writes (conditional PUT and DELETE), and backend 'aws-dr' does not provide them: the backend ignored or rejected a conditional write, as Backblaze B2 does. Keep encrypted buckets on a backend with atomic writes (move a bucket there with a migrate job) — see https://deltaglider.com/docs/how-to/backend-capability-validation

A config change, a bucket create, a re-encrypt job (or Resume), a migrate, or the boot conversion tried to make the proxy encrypt on a backend without proven atomic writes. Nothing changed. Put the encrypted bucket on a backend with atomic writes. When the result says `could not reach the backend to verify (...)`, fix the connection to the backend first, and then click **Check again** on its card.

### `403 — Bucket '<name>' is read-only`

> Bucket 'releases' is read-only: it is encrypted, and encryption needs atomic writes, which backend 'aws-dr' does not provide (the backend ignored or rejected a conditional write, as Backblaze B2 does). Reads keep working. To write to it again, move the bucket to a backend with atomic writes with a migrate job — see https://deltaglider.com/docs/how-to/backend-capability-validation

Follow [Make a read-only bucket writable again](#make-a-read-only-bucket-writable-again).

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
- *"replication rules 'A' and 'B' both write into the storage S (buckets 'X' and 'Y') with overlapping prefixes"*: two writers into one destination prefix break the single-writer guarantee. The check compares the real storage, so two bucket names that alias one real bucket count as one destination. Give each rule a distinct destination prefix.

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
