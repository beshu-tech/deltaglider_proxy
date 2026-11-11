# Encryption reference

Encryption at rest is configured per backend, and a bucket can replace the encryption of its backend with its own. Each backend in `storage.backends[]` (or the singleton `storage.backend`, via the top-level `storage.backend_encryption`) carries one `encryption` block with exactly one of four modes. The modes are mutually exclusive on a given backend; the `BackendEncryptionConfig` enum enforces this by construction. A bucket in `storage.buckets` can carry an `encryption` block of the same schema. The rules of that block are in [Per-bucket encryption](#per-bucket-encryption).

## Modes

| Mode | Who holds the key | What the backend stores | Supported backend types |
|---|---|---|---|
| `none` | nobody | plaintext object bodies | filesystem, S3 |
| `aes256-gcm-proxy` | the proxy (YAML, env var, or GUI-generated) | AES-256-GCM ciphertext, encrypted before the backend sees the bytes | filesystem, S3 |
| `sse-kms` | AWS KMS (`kms_key_id`) | AWS-encrypted; AWS decrypts transparently for IAM callers with `kms:Decrypt` | S3 only |
| `sse-s3` | AWS (AWS-managed AES256) | AWS-encrypted; AWS decrypts transparently for authorized IAM callers | S3 only |

A backend with no `encryption` block has `mode: none`. `Config::check` rejects `sse-kms` and `sse-s3` on filesystem backends. In `sse-kms` / `sse-s3` modes the proxy never handles key material: every PutObject carries `ServerSideEncryption` (plus `SSEKMSKeyId` for SSE-KMS) headers, and the object body is plaintext to the proxy's serialization path. In `aes256-gcm-proxy` mode the `EncryptingBackend` wrapper encrypts after delta compression, so xdelta3 compression ratios are unaffected.

## Configuration fields

| Field | Applies to | Meaning |
|---|---|---|
| `mode` | all | `none` \| `aes256-gcm-proxy` \| `sse-kms` \| `sse-s3` |
| `key` | `aes256-gcm-proxy` | 32-byte hex AES-256 key. Treated as an infrastructure secret: stripped from canonical exports (`/config/export`). |
| `key_id` | `aes256-gcm-proxy` | The name of the key. The admin UI, the audit log, the encryption witness and the legacy-key scan show the key under this name. On a backend, optional: without it, the id is `fp-` and the 16 hexadecimal characters of the key's fingerprint (see [Key IDs](#key-ids)). On a bucket, required. |
| `kms_key_id` | `sse-kms` | The KMS key: a key ARN or alias ARN (`arn:aws:kms:<region>:<account>:key/<id>`), an alias name (`alias/<name>`), a key id (a UUID) or a multi-Region key id (`mrk-<32 hex digits>`). On MinIO, write a key name as `arn:aws:kms:<key name>`. The proxy refuses to start with an empty or malformed value, on a backend and on a bucket, because every write would fail with an S3 error. |
| `bucket_key_enabled` | `sse-kms` | S3 Bucket Keys; reduces per-request KMS cost. |
| `legacy_key`, `legacy_key_id` | any mode | Decrypt-only shim for objects written under a previous proxy-AES key. Valid under every mode, including `none`. |

### Environment variables

Env vars override only the secret fields (`key`, `kms_key_id`); the mode itself is authoritative in YAML. Backend names map to env-var suffixes by uppercasing and replacing `-` and `.` with `_`.

| Variable | Sets |
|---|---|
| `DGP_BACKEND_<NAME>_ENCRYPTION_KEY` | `key` on the named backend (e.g. `DGP_BACKEND_HETZNER_FSN1_ENCRYPTION_KEY` for backend `hetzner-fsn1`) |
| `DGP_BACKEND_<NAME>_SSE_KMS_KEY_ID` | `kms_key_id` on the named backend |
| `DGP_ENCRYPTION_KEY` | `key` on the singleton backend |
| `DGP_SSE_KMS_KEY_ID` | `kms_key_id` on the singleton backend |

### A proxy-AES backend with no key

When a backend is in `aes256-gcm-proxy` mode and neither the YAML nor the env var supplies a `key`, the proxy cannot encrypt. It does not fall back to plaintext. Instead, every write to that backend fails with `500 InternalError`, and the log names the backend and the env var to set. Reads keep working: the objects that the `legacy_key` can decrypt stay readable while you fix the key. A plaintext object follows the read rule of a bucket that encrypts (see [Plaintext objects in a bucket that encrypts](#plaintext-objects-in-a-bucket-that-encrypts)), and no conversion window opens for a backend without a key. The proxy starts with such a config, so that the stored data stays available. A config apply that would put a backend into this state is refused. `config lint` only warns about it, because lint does not see the env vars of the deployment.

### Examples

Proxy-AES on a named S3 backend, key supplied by env var:

```yaml
# validate
storage:
  backends:
    - name: hetzner-fsn1
      type: s3
      endpoint: https://fsn1.your-objectstorage.com
      region: fsn1
      encryption:
        mode: aes256-gcm-proxy
        key_id: hetzner-2026-06
  buckets:
    db-archive:
      backend: hetzner-fsn1
```

```bash
DGP_BACKEND_HETZNER_FSN1_ENCRYPTION_KEY=$(openssl rand -hex 32)
```

Proxy-AES on a singleton filesystem backend:

```yaml
# validate
storage:
  backend:
    type: filesystem
    path: /var/lib/deltaglider_proxy/data
  backend_encryption:
    mode: aes256-gcm-proxy
    key: "${env:DGP_ENCRYPTION_KEY}"
    key_id: local-2026-06
```

SSE-KMS on an AWS backend:

```yaml
# validate
storage:
  backends:
    - name: aws-dr
      type: s3
      region: eu-west-1
      encryption:
        mode: sse-kms
        kms_key_id: arn:aws:kms:eu-west-1:123456789012:key/abcd-ef01
        bucket_key_enabled: true
```

SSE-S3 on an AWS backend:

```yaml
# validate
storage:
  backends:
    - name: aws-dr
      type: s3
      region: eu-west-1
      encryption:
        mode: sse-s3
```

You can also set keys in the admin GUI (**Storage → Backends**). The browser generates GUI keys with `crypto.getRandomValues`, and the keys do not go through the server before Apply. When you apply, the proxy writes the key in plain text into the config file. A new config file is readable only by its owner (mode 0600), and a rewritten file keeps the owner and group permissions of the old one, and nobody else can read it. To keep the key out of the file, set it in `DGP_BACKEND_<NAME>_ENCRYPTION_KEY` instead.

## Per-bucket encryption

A bucket policy can carry its own `encryption` block. The block uses the same fields as the `encryption` block of a backend. When a bucket has this block, the writes to that bucket use it instead of the encryption of the backend. A bucket without the block uses the encryption of its backend, as before.

The admin UI edits the block in the **Encryption** group of the bucket row on **Storage → Buckets**, and `GET /_/api/admin/buckets` reports the effective encryption of every bucket (`effective_encryption`, see the [admin API reference](admin-api.md)). [How to encrypt one bucket on a shared backend](../how-to/encrypt-one-bucket-on-a-shared-backend.md) shows the steps.

```yaml
# validate
storage:
  backends:
    - name: local-disk
      type: filesystem
      path: /var/lib/deltaglider_proxy/data
      encryption:
        mode: aes256-gcm-proxy
        key: "${env:LOCAL_DISK_AES_KEY}"
        key_id: local-disk-2026
  buckets:
    releases:
      backend: local-disk
    db-archive:
      backend: local-disk
      encryption:
        mode: aes256-gcm-proxy
        key: "${env:DB_ARCHIVE_AES_KEY}"
        key_id: db-archive-2026
    downloads:
      backend: local-disk
      encryption:
        mode: none
```

In this example, `releases` uses the key of `local-disk`, `db-archive` uses its own key, and `downloads` stores plaintext.

### Rules

The proxy refuses to start with a config that breaks one of these rules, and it refuses an apply that breaks one. `config lint` reports them as errors.

| Rule | Reason |
|---|---|
| A bucket in `sse-kms` or `sse-s3` mode is stored on an S3 backend. | Native SSE is done by S3. A filesystem backend would store the bytes as they are, so the proxy refuses the block. |
| `kms_key_id` has the shape of a KMS key. | See [Configuration fields](#configuration-fields). A malformed value would fail every write to the bucket. |
| `key_id` is set explicitly. | The admin UI, the audit log and the legacy-key scan show a bucket's key under this id, so the proxy asks for a readable name. A backend key without an id gets the generated id `fp-<hex>` instead. |
| `legacy_key_id` is set explicitly when `legacy_key` is set. | The same reason. |
| The bucket has `backend:` when `storage.backends` is used. | A bucket without a route can live on any backend, so the proxy cannot tell which storage the block applies to. With a single backend (`storage.backend`), the bucket is always on that backend, and `backend:` is not needed. |
| Every name of one stored bucket has the same effective encryption. | The encryption belongs to the stored bucket (the backend and the real bucket name), not to a name. When two bucket names reach the same real bucket through `alias`, they must carry the same `encryption` block, or none. |
| A key id names one key. | Two keys with the same `key_id` and different key bytes are refused, across all backends and all buckets. The same key with the same id in two places is allowed. |
| A key has one id on a backend. | An object stores only the fingerprint of its key, and the proxy shows the key by the id that this fingerprint maps to. When one key had two ids on one backend (for example the backend key and a bucket key with the same bytes but another `key_id`), the id of an object would depend on the order of a lookup. The proxy refuses to start with such a config: `backend 'X': one key has two ids, 'A' and 'B'`. |

With `config_sync_bucket` set (several instances), the encryption witness makes an instance that disagrees with the others about a bucket's encryption refuse writes to that bucket. See [How to run multiple instances](../how-to/run-multiple-instances.md#8-keep-the-encryption-of-each-bucket-the-same-on-every-instance).

A bucket key has no `DGP_*` environment variable. Put the key in the YAML file, or reference any environment variable with `${env:NAME}`, as in the example.

A bucket block in `aes256-gcm-proxy` mode with no key makes the proxy refuse every write to that bucket, as for a backend (see [A proxy-AES backend with no key](#a-proxy-aes-backend-with-no-key)). An apply that puts a bucket into this state is refused.

### Changing a bucket's encryption

When an apply changes the encryption that a bucket writes with, the proxy does not rewrite the objects that the bucket already holds. They keep the form they were written in. An encrypted object stays readable while the keyring holds its key, because each object carries the fingerprint of its key. When the apply makes a bucket that holds objects start to encrypt with a proxy key, the proxy opens a conversion window for the bucket and starts its re-encrypt job, so that the plaintext objects stay readable while the job converts them (see [How a conversion starts](#how-a-conversion-starts)). For every other change, such as a key rotation, the response of the apply has a warning for each such bucket. The warning names the re-encrypt job (`POST /_/api/admin/jobs/reencrypt` with `{"buckets": ["<bucket>"]}`), which rewrites the objects and the delta references into the new form. The proxy does not start that job by itself.

The proxy judges every key of a bucket's block on that bucket alone. After an apply, the bucket must still be able to read each key id that its block held: in its own block, or in the encryption of the backend that stores it. A key that another bucket on the same backend holds does not count, because that bucket can change its own block later.

- A key that a bucket writes with cannot leave in one step. An apply that removes it is refused. The objects written under that key would otherwise become unreadable.
- A legacy (decrypt-only) key of a bucket leaves only when the request confirms its id. Send the header `X-DGP-Confirm-Key-Removal: <key id>` with the apply, or `config apply --confirm-key-removal <key id>`. Without the confirmation, the apply is refused.

To stop using a bucket key, do these steps:

1. Set the bucket's `mode` to `none`, or give the bucket a new key and key id. An apply that leaves `legacy_key` out keeps the old key as the bucket's `legacy_key`, with its id as `legacy_key_id`.
2. Run the re-encrypt job for the bucket.
3. Check that no object still uses the old key: `GET /_/api/admin/backends/<backend>/legacy-key-usage?bucket=<bucket>`. This scan counts the objects under that key id in every bucket stored on the backend, because an object can carry the key in another bucket too (for example, when two buckets shared the key).
4. When the scan answers `"safe_to_clear": true`, remove the bucket's `legacy_key` and `legacy_key_id`, or the whole `encryption` block when its mode is `none`, and confirm the key id as described above.

#### A downgrade needs a confirmation

A change that makes a bucket store new objects in plaintext, where it encrypted them before, is a downgrade. The proxy refuses a downgrade unless the request confirms it. These changes are downgrades:

- The bucket's block gets `mode: none`.
- The bucket's block is removed, and the backend of the bucket stores plaintext.
- The bucket moves to a backend that stores plaintext (a changed `backend:`, or a migrate).
- The backend's own encryption becomes `mode: none`. This is a downgrade for every bucket on that backend without its own block.
- `default_backend` changes to a backend that stores plaintext, or the config moves from an encrypting `storage.backend` to named backends that store plaintext. This is a downgrade for every bucket that the config does not name. Its subject is `route:default`.

To confirm, send the header `X-DGP-Confirm-Encryption-Downgrade: <subject>` with the request (comma-separated for several subjects), or use `config apply --confirm-encryption-downgrade <subject>`. The subject is the bucket name. For a backend's own downgrade, the subject is `backend:<name>`, and that one confirmation covers every bucket that inherits the backend's encryption. The refusal names the subject that it needs. Every config write runs this gate: the document apply, a section PUT, a field PATCH, the backend endpoints, a backup restore, and `config apply`. The migrate start (`POST /_/api/admin/buckets/<bucket>/migrate`) runs it too, and answers `400` without the confirmation; the flip at the end of the migrate is then not checked again. Objects stored before a downgrade keep their encryption, and they stay readable.

A change from one encrypting mode to another (for example from `aes256-gcm-proxy` to `sse-kms`) and a key rotation are not downgrades.

These gates run on an apply through the admin API. A proxy that starts from an edited file has no previous config to compare with, so the gates do not run at boot: a file edited to `mode: none` takes effect at the next start with no confirmation.

#### Audit entries

Every change of a bucket's write encryption writes one audit entry per subject, whatever the write path: a config apply, a section PUT, a field PATCH, the backend endpoints, a backup restore, or the flip of a migrate. The entry names the mode and the key id before and after the change, never key material. The action is one of these:

| Action | Change |
|---|---|
| `bucket_encryption_set` | From plaintext to an encrypting mode, or from one encrypting mode to another. |
| `bucket_encryption_cleared` | From an encrypting mode to plaintext (a downgrade). |
| `bucket_encryption_key_rotated` | The same mode with another key id (`kms_key_id` for SSE-KMS). |

A change of a backend's own encryption therefore writes one entry for `backend:<name>` and one entry for each bucket that the config names and that inherits that encryption (1 + N entries). A change of `default_backend` writes one entry for `route:default`. With `config_sync_bucket` set, `witness publish` writes `bucket_encryption_witness_publish`.

The target of the entry is `<subject>: <before> -> <after>`, for example `db-archive: aes256-gcm-proxy (key id 'db-archive-2026') -> none`. The subject is the bucket name, or `backend:<name>` for a change of the backend's own encryption. A change that the proxy makes by itself (the flip of a migrate) has the user `background`.

### The keyring

Each backend has one encrypting layer, which holds every key that the config gives to that backend: the key and legacy key of the backend, and the key and legacy key of each bucket on it. Every encrypted object carries the fingerprint of its key. When the proxy reads an object, it picks the key whose fingerprint the object names. It never tries keys one after the other. A bucket therefore reads the objects that it wrote under the backend key before it got its own key, and it reads them again after its own key is retired into `legacy_key`.

When the proxy writes an object, it picks the key of the stored bucket that the object goes to. A server-side copy inside one backend keeps the stored bytes only when the destination bucket writes with the same key as the source object. The proxy then writes new sealed metadata for the destination, because the sealed metadata names the location of the object. Otherwise the proxy decrypts the object and encrypts it again with the key of the destination.

### Native SSE per bucket

A bucket on an S3 backend can use `sse-kms` or `sse-s3` in its own block. The proxy uses one S3 client for each backend. It does not use a second client for these buckets. Instead, it sets the SSE headers of each request from the bucket that the request writes to. These requests are PutObject, CreateMultipartUpload, CopyObject, UploadPartCopy and the empty objects that the proxy writes for folders and listing facts. The headers are `x-amz-server-side-encryption`, and for SSE-KMS also `x-amz-server-side-encryption-aws-kms-key-id` and `x-amz-server-side-encryption-bucket-key-enabled`.

```yaml
# validate
storage:
  backends:
    - name: aws-dr
      type: s3
      region: eu-west-1
      encryption:
        mode: sse-s3
  buckets:
    db-archive:
      backend: aws-dr
      encryption:
        mode: sse-kms
        kms_key_id: arn:aws:kms:eu-west-1:123456789012:alias/db-archive
        bucket_key_enabled: true
    releases:
      backend: aws-dr
      encryption:
        mode: aes256-gcm-proxy
        key: "${env:RELEASES_AES_KEY}"
        key_id: releases-2026
    downloads:
      backend: aws-dr
      encryption:
        mode: none
```

In this example, each bucket on `aws-dr` gets exactly one layer of encryption:

| Bucket | What the proxy sends to S3 |
|---|---|
| `db-archive` | The bytes as they are, with the SSE-KMS headers of its key. |
| `releases` | AES-256-GCM ciphertext, with no SSE headers. The SSE-S3 mode of the backend does not apply, so the bytes are not encrypted twice. |
| `downloads` | The bytes as they are, with no SSE headers. `mode: none` on a bucket means plaintext, also on a backend with native SSE. |
| A bucket with no block | The bytes as they are, with the SSE-S3 headers of the backend. |

A server-side copy between two buckets on one backend sends the SSE headers of the destination bucket. A copy from an SSE-KMS bucket into a plaintext bucket therefore stores plaintext, and a copy from a plaintext bucket into an SSE-KMS bucket stores an SSE-KMS object. A copy from or into a bucket that encrypts in the proxy goes through the proxy, which decrypts and encrypts the bytes as the destination asks.

A multipart upload into an SSE bucket uses native S3 multipart. The SSE headers go on CreateMultipartUpload, and S3 encrypts every part.

#### KMS permissions

S3 calls KMS with the credentials of the request, so the credentials of the backend (`access_key_id` of `aws-dr`, or the role of the proxy) need these KMS permissions on each key that a bucket or backend names:

| Permission | Why |
|---|---|
| `kms:GenerateDataKey` | Every write. S3 asks KMS for a data key to encrypt the object. |
| `kms:Decrypt` | Every read, every server-side copy from the bucket, and every multipart upload into it (S3 reads the encrypted data key to finish the upload). |
| `kms:DescribeKey` | Optional. S3 uses it to resolve an alias. |

Give these permissions in the key policy, or in an IAM policy when the key policy allows IAM. With `bucket_key_enabled: true`, S3 calls KMS less often, but it needs the same permissions. SSE-S3 needs no KMS permission.

### Multipart uploads, migrate and replication

A multipart upload into a bucket with its own key is encrypted with that key. The proxy does not use native S3 multipart for a bucket that encrypts in the proxy, because each part would reach the backend in plaintext.

When a bucket moves to another backend (`POST /_/api/admin/buckets/<bucket>/migrate`), its `encryption` block moves with it. The copies on the target backend are written under the bucket's key, and the verify phase checks the at-rest form of each copy.

A replication rule from a bucket that encrypts into a bucket that stores plaintext is allowed. `Config::check` reports a warning for it, because the copies are not encrypted at rest.

## Encryption needs atomic writes

The proxy writes `aes256-gcm-proxy` objects only on a backend whose conditional writes it has proven. A conditional write is a `PUT` or a `DELETE` with `If-Match` or `If-None-Match`, which the backend refuses with `412` when the condition does not hold. The proxy needs these writes because it rewrites encrypted buckets in place: a conversion and a key rotation read each object and store it again, conditional on the version that they read (see [Conditional rewrites and the write gate](#conditional-rewrites-and-the-write-gate)). The key claim and the conversion marker are also conditional writes. On a backend that ignores the condition, a rewrite can overwrite a newer client write. So the proxy refuses to turn proxy encryption on there, and an encrypted bucket that is already there becomes read-only. This rule applies with one instance too.

The rule applies to `aes256-gcm-proxy` only. In `sse-s3` and `sse-kms` modes the backend encrypts each object itself. The proxy never converts or rotates those objects (the re-encrypt job refuses native SSE modes), so it needs no conditional write for them. A proxy mode without a key refuses every write, so it needs none either.

### The probe

The proxy proves the conditional writes of a backend with a probe:

- A filesystem backend has atomic writes. It has one node, and every write of an object on that node takes the object's in-process lock. The probe sends no request. The verdict is `cas-verified` with `via: local-filesystem`.
- On an S3 backend, the probe writes one small object, `.deltaglider/_cwprobe/<uuid>`, in a bucket that the backend holds. Then it tests `If-None-Match: *`, `If-Match` with a wrong ETag, and `If-Match` with the current ETag. It deletes the object afterwards. On a bucket that keeps versions, it also deletes every version of the key. It does this also when the probe ran out of time, so no noncurrent version of the probe key stays in the bucket. The details are in [How to use a backend without conditional writes](../how-to/backend-capability-validation.md#how-the-probe-works).
- Ceph RGW (Hetzner Object Storage) compares the `If-Match` of a `PUT` with the ETag without its quotes, so it refuses the correct ETag in the quoted form that S3 returns. When the probe's write with the current ETag gets `412`, the probe tries the same ETag without quotes, and it checks that a wrong ETag without quotes is still refused. When both hold, the verdict is `cas-verified` with `if_match: unquoted`, and every conditional request of the proxy to that backend sends the ETag without quotes. Hetzner also ignores `If-Match` on `DELETE`, so the proxy reads an object before each delete that must not remove a newer version. See [The `If-Match` form](../how-to/backend-capability-validation.md#the-if-match-form-and-backends-that-ignore-it-on-delete).
- Some backends close the connection when they receive a conditional `PUT`, and send no HTTP answer at all. Backblaze B2 (`eu-central-003`) did this on 2026-10-31. When this happens, the probe sends a plain `PUT` of the same key. When the plain `PUT` works, the dropped connection was the backend's answer to the condition, so the verdict is `non-cas`. When the plain `PUT` fails too, the probe cannot reach the backend, so the verdict is `unknown`.
- One probe has one deadline of 15 seconds in total. The listing of the backend's buckets, which picks the bucket that the probe writes in, may use 5 seconds of it. The SDK retries each request a limited number of times inside the deadline, so a probe never retries without end.

A verdict has one of three values:

| Verdict | Meaning | Proxy encryption | Writes to encrypted buckets |
|---|---|---|---|
| `cas-verified` | The backend supports atomic writes (`via: probe` or `via: local-filesystem`). `if_match` is `quoted` or `unquoted`: the form of the ETag that the backend accepts in `If-Match`. | Allowed | Allowed |
| `non-cas` | The backend does not support atomic writes: it ignored or refused a conditional write, as Backblaze B2 does. | Refused | Refused with `403`: the buckets are read-only |
| `unknown` | The proxy could not verify the backend's atomic writes, because the probe did not reach it. | Refused | Refused with `503 SlowDown`, which clients retry |

A backend without a verdict yet is handled like `unknown`: the writes to its encrypted buckets get `503 SlowDown` until the probe ends.

### Actions that run a fresh probe

Each action below runs a new probe of the backend first. It does not use a verdict that is older than the action:

- A config change that turns proxy encryption on, changes its key, or moves an encrypted bucket to another backend. This applies to every surface: `POST /_/api/admin/config/apply` and `/config/validate`, `PUT /_/api/admin/config/section/:name` and its `/validate`, and `deltaglider_proxy config apply` (which sends the document apply). The proxy checks a backend whose own encryption becomes proxy-AES or gets another key, a new default backend that encrypts with a proxy key, and a configured bucket that starts or changes proxy encryption or moves to another backend with it.
- `POST /_/api/admin/buckets` for a bucket that will encrypt with the proxy (its own block, or its backend's).
- `POST /_/api/admin/jobs/reencrypt` for a bucket whose target is a proxy key, also with `"open_window": true` (the Resume button). The probe runs before the request opens a window, because the conversion marker is a write.
- The conversion that a restart starts (see [A conversion that a restart starts](#a-conversion-that-a-restart-starts)). A refused boot conversion writes the audit entry `encryption_conversion_refused`.
- `POST /_/api/admin/buckets/:bucket/migrate`, when the bucket will encrypt with a proxy key on the target backend.

These actions refuse a backend whose fresh verdict is `non-cas` or `unknown`. A bucket that the configuration does not name can live on any backend, so the proxy asks the storage router which backend holds it before it probes, and it probes that backend.

When a re-encrypt run starts, it uses a `cas-verified` verdict of the current definition of the backend that holds the bucket. Without one, it runs the probe. It fails the run when the backend does not support atomic writes, and it stops the run for a later retry when the proxy could not verify them.

A refused action changes nothing and answers `409 Conflict`. The body has the usual error text and an `atomic_writes` object:

```json
{
  "error": "Failed to apply section 'storage' (no state changed): encryption of bucket 'releases' refused: encryption needs atomic writes (conditional PUT and DELETE), and backend 'aws-dr' does not support them: it ignored or rejected a conditional write, as Backblaze B2 does. Keep encrypted buckets on a backend with atomic writes (move a bucket there with a migrate job) — see https://deltaglider.com/docs/how-to/backend-capability-validation",
  "atomic_writes": {
    "code": "atomic_writes_required",
    "backend": "aws-dr",
    "probe": "non-cas",
    "probe_result": "the backend does not support atomic writes: it ignored or rejected a conditional write, as Backblaze B2 does",
    "doc_url": "https://deltaglider.com/docs/how-to/backend-capability-validation"
  }
}
```

When the probe could not reach the backend, the text says so instead, and `probe` is `unknown`:

> encryption of bucket 'releases' refused: encryption needs atomic writes (conditional PUT and DELETE), and the proxy could not verify them on backend 'aws-dr': the proxy could not verify the backend's atomic writes (…). Retry when the backend is reachable (Check again on the backend card) — see https://deltaglider.com/docs/how-to/backend-capability-validation

The endpoints of the admin API that create a bucket (`POST /_/api/admin/buckets`) or start a migrate send the same `atomic_writes` object with their `409`.

A change that turns proxy encryption off is never refused for this reason. A decrypt job (target `none`) needs no atomic writes. On a backend without them, the decrypt job keeps the bucket's write gate, so uploads and deletes on the instance that runs it get a temporary `503 SlowDown`.

### Read-only encrypted buckets

A bucket that encrypts with the proxy, on a backend whose verdict is `non-cas`, is read-only. GET, HEAD, LIST and a copy out of the bucket keep working. Every write is refused:

| Write | Answer |
|---|---|
| S3 `PutObject`, `DeleteObject`, `DeleteObjects`, `CreateMultipartUpload`, `CompleteMultipartUpload`, `CopyObject` into the bucket, `DeleteBucket` | `403 AccessDenied` |
| Browser form POST (PostObject), after the signature check | `403 AccessDenied` |
| Admin bulk copy into the bucket, move out of it, delete, and delete jobs (`POST /_/api/admin/objects/delete-jobs`) | `403` |
| Metadata backfill (`POST /_/api/admin/jobs/backfill-metadata`) | `403` |
| Re-encrypt to a proxy key (`POST /_/api/admin/jobs/reencrypt`) | `409` with `atomic_writes` |
| Replication into the bucket | The rule shows `deferred` with the reason. Its events are skipped, not held, and the rule catches up with a full run when the bucket is writable again (see [Replication reference](replication.md)). A replication run-now answers `409` with the reason. |
| Lifecycle writes into the bucket, delete jobs that already run | They wait. A lifecycle rule shows `deferred` with the reason. |
| S3 `CreateBucket` of a bucket that would encrypt with the proxy on such a backend (a bucket without a policy inherits the encryption of its backend) | `403 AccessDenied`; the proxy does not create the bucket |

The `403` is `AccessDenied` because the refusal comes from the configuration, not from a temporary state. An S3 client retries a `503`, and it must not retry this refusal. S3 has no error code for a read-only bucket. A refused `CreateBucket` also gets `403`, not `409`: clients read a `409` on `CreateBucket` as "the bucket already exists" and go on as if it were there.

An S3 client gets a fixed sentence. It does not name the backend, and it never carries the error of the probe, because that error can name the address of the backend:

> Bucket 'releases' is read-only: it is encrypted, and its backend does not support atomic writes, which encryption needs. Reads keep working. To write to it again, move the bucket to a backend with atomic writes with a migrate job — see https://deltaglider.com/docs/how-to/backend-capability-validation

A refused `CreateBucket` gets:

> Bucket 'releases' cannot be created: it would be encrypted by the proxy, and its backend does not support atomic writes (conditional PUT and DELETE), which encryption needs. The proxy does not create a bucket that would be read-only. Create it on a backend with atomic writes — see https://deltaglider.com/docs/how-to/backend-capability-validation

The admin API (admin bulk operations, backfill, the `read_only` field of a bucket) and the server log use a longer message that names the backend and the probe result.

A migrate job out of a read-only bucket is allowed. It is the way to make the bucket writable again: [How to move a bucket to another backend](../how-to/move-a-bucket-between-backends.md). The job reads the source as the job, so it also copies plaintext objects that the bucket holds from before its encryption: it encrypts them on the target and reports each one as adopted.

The proxy judges each write against the backend that really holds the bucket. A bucket that the configuration does not name can live on any backend, so the storage router finds it there first: from its cache, or with the same `HeadBucket` round that the write itself sends. When the router cannot tell where the bucket is (a backend did not answer), the write gets `503 SlowDown` unless every backend whose own storage encrypts with the proxy has proven atomic writes, because the bucket can be on any of them.

### Pending writes: before a verdict, and when the probe cannot reach the backend

While a backend has no verdict for its current definition, or its verdict is `unknown`, the writes to its encrypted buckets fail closed with `503 SlowDown`. S3 clients retry a `503`. Reads are never blocked. An S3 client gets this fixed sentence:

> Bucket 'releases' is encrypted, and the proxy has not yet verified that its backend supports atomic writes, which encryption needs. Writes wait until the check succeeds; retry in a few seconds. Reads keep working — see https://deltaglider.com/docs/how-to/backend-capability-validation

A filesystem backend gets its verdict at once, without a request, so its buckets never wait. For an S3 backend, the proxy gets the verdicts in the background:

- At boot, the proxy starts the probe of every backend that holds a bucket that encrypts with the proxy as one of its first steps, all backends at the same time, in parallel with the boot health check. The boot waits up to 3 seconds for them, so a backend with atomic writes is usually verified before the proxy serves. A slower probe does not hold the start: a probe ends within its 15-second deadline, so the writes to the encrypted buckets of an S3 backend answer `503` for at most about 15 seconds after the start.
- After a config change, the proxy probes a backend that has no verdict yet for its current definition.
- When a probe could not reach the backend (`unknown`), the proxy probes it again after 5 seconds, then after 10, 20 and 40 seconds, and then every 60 seconds. When the health check sees the backend become reachable again, the proxy probes it at once.
- A `cas-verified` or `non-cas` verdict is never probed again on a timer. A probe writes a key into a bucket of the backend, and on a versioned bucket a probe every few minutes would leave versions there for nothing. The proxy probes such a backend again only on **Check again**, after a config change that redefines it, and at a restart.
- A failed probe never replaces a verdict that an earlier probe of the same backend definition proved. A network error therefore does not make the encrypted buckets of a backend with atomic writes wait, and it does not lift the read-only state of a backend without them.
- The proxy keeps the verdicts per backend name and per backend definition, for up to 4 definitions of one name. A validate or an apply that probes a changed definition (new credentials, a new endpoint) does not remove the verdict of the definition that runs, so the running buckets are not affected by the check of a change that you have not applied.
- The proxy logs each change of the read-only set (a warning that names the read-only buckets, and an info line when they are writable again), and a warning when it could not verify a backend.

`GET /_/ready` shows the state in its body, as `atomic_writes` with one entry per backend that holds encrypted buckets: `checking` (no verdict yet), `unverified`, `supported` or `unsupported`. The state never makes the proxy not ready: reads and the other buckets work, and a pending check ends on its own, so a load balancer that removed the instance would only lose service.

**Check again** on the backend card (`POST /_/api/admin/backends/:name/capability-probe`) runs a fresh probe at once. When the backend now proves atomic writes, its encrypted buckets become writable at once, without a restart. The admin views show the state:

- `GET /_/api/admin/backends` lists `read_only_buckets` for each backend, `capability_words` (the verdict in plain words), `proxy_unavailable` (why proxy AES is not offered there) and `atomic_writes_checking` (`true` while a backend with encrypted buckets has no verdict yet). The backend card shows **Checking atomic writes…** in that case.
- `GET /_/api/admin/buckets` has a `read_only` reason on each read-only bucket, and `capability_words` in the encryption view of each bucket.

Both lists show every read-only bucket that the storage listing shows, also a bucket without a policy that inherits the encryption of its backend.

The proxy's own writes follow the same rule. In an encrypted bucket on a backend without proven atomic writes, the proxy does not store listing tokens (see [Listing tokens](#listing-tokens-s3-backends)) and does not write a key claim. Reads keep working without them, but a listing of such a bucket can send more `HEAD` requests, because it finds no stored token.

## Key check value

With `config_sync_bucket` set, the encryption witness publishes, for each proxy-AES key that a bucket writes with, a key check value (`kcv`) next to the key id. The key check value is the fingerprint of the key in hexadecimal (16 characters, see [Key IDs](#key-ids)). Two instances that use the same key id with different key bytes then disagree, and the witness refuses writes on the instance that differs. The value is a one-way function of the key, and only 64 bits of it are published, so it reveals nothing practical about a random 256-bit key. Every object that the key encrypts carries the same fingerprint, so the witness and the objects name a key in the same way. A change that keeps the key id but changes the key changes the key check value, so the audit entry of that change is a `bucket_encryption_key_rotated`.

## Key IDs

Every proxy-AES key has a fingerprint: the first 8 bytes of an HKDF-SHA256 expansion of the key (salt `DGE2 site`, info `DGE2 fp`). The fingerprint depends on the key alone. Every object that the key encrypts stores this fingerprint, and no object stores a key id.

The key id is the name of a key in the configuration. It is the explicit `key_id` from the YAML, or `fp-` followed by the fingerprint in hexadecimal when a backend key has no `key_id` (for example `fp-bf647b30a881e29c`). The backend name is not part of the generated id. The same key therefore has the same generated id on every backend, and a renamed backend keeps its ids. A bucket key always needs an explicit `key_id`.

When the proxy reads an encrypted object, it looks up the fingerprint of the object in the keyring of the backend, and it shows the object with the id of the key that it finds. One key has one id on a backend, so this answer is always the same. When no key of the backend has the fingerprint, the read fails with an error that names the fingerprint:

> object in bucket 'db-archive' was encrypted with a key that this backend does not have (fingerprint bf647b30a881e29c). Add that key as legacy_key, or restore it

To find the fingerprint of a key that you hold, run this command with OpenSSL 3 (replace `<64 hex characters>` with the key):

```bash
openssl kdf -keylen 8 -kdfopt digest:SHA256 -kdfopt hexkey:<64 hex characters> \
  -kdfopt 'salt:DGE2 site' -kdfopt 'info:DGE2 fp' HKDF | tr -d ':' | tr 'A-F' 'a-f'
```

To read the fingerprint of a stored object, take the value of its `dg-sealed` field (for example from `aws s3api head-object` on the raw backend). The 12 characters after `2.` encode the fingerprint in their first 8 bytes:

```bash
printf '%s' "$SEALED" | cut -c3-14 | basenc --base64url -d | head -c 8 | xxd -p
```

At startup, two backends that declare the same explicit `key_id` with different key bytes are a fatal error.

## Metadata markers

The proxy stores an encrypted object with these user-metadata fields. On filesystem backends they live in the `user.dg.metadata` xattr; on S3 backends, in S3 user metadata (`x-amz-meta-dg-*`).

| Stored field | Value | Meaning |
|---|---|---|
| `dg-encrypted` | `aes-256-gcm-chunked-v1` | The object is proxy-encrypted. The value is the name of an older format on purpose: a release before format v2 refuses to read the object, instead of serving its ciphertext as plaintext. |
| `dg-sealed` | `2.` followed by base64url text | The sealed metadata (see [Sealed metadata](#sealed-metadata)). |
| `dg-file-sha256`, and `dg-ref-sha256` of a delta | `sealed:` followed by 32 hexadecimal characters | An identity token. It is different for every write, and it is not a hash of the content. The real hashes are in the sealed metadata. |
| `dg-encrypted-native` | `sse-kms` or `sse-s3` | Native SSE; no proxy-side decryption. |

An encrypted object stores no `dg-md5`, no multipart ETag, no `dg-encryption-key-id`, and, for a delta, no size of the object (`dg-file-size` is `0`). The proxy reads these facts from the sealed metadata.

Inside the proxy, the read path shows an encrypted object with the marker `dg-encrypted: v2` and with `dg-encryption-key-id`, the id of the key that the fingerprint names. The jobs and the legacy-key scan use these two values. An S3 client never sees them, because a `HeadObject` or `GetObject` response does not carry the `dg-*` user-metadata keys of the proxy.

## Format v2 (proxy-AES)

Every encrypted object uses one format: a delta reference, a delta, and an object stored as it is (passthrough).

**Keys.** Each object has its own keys. When the proxy writes an object, it draws a random 32-byte salt. It derives three keys of the object from the key in the configuration and the salt with HKDF-SHA256: a content key, a metadata key, and a key commitment. The key in the configuration never encrypts data directly.

**Body.** The body is a 76-byte header followed by frames:

```
header  = "DGE2" (4 B) | key fingerprint (8 B) | salt (32 B) | key commitment (32 B)
frame i = AES-256-GCM(content key, nonce = frame index i, plaintext of frame i)
          AAD = "DGE2" | i | final flag
```

- Every frame but the last holds 64 KiB of plaintext. The last frame holds the rest (an empty object has one empty frame).
- The nonce of a frame is its index. This is safe, because every object has its own content key.
- The AAD binds the index of the frame (a reordered frame fails) and the flag of the last frame (a truncated body fails, also at a frame boundary).
- Frames have no length field. The stored size of a body is exactly `76 + plaintext size + 16 × number of frames`, and the sealed metadata holds the plaintext size. For a delta, a delta reference and a range request, the proxy checks the stored size before it sends the first byte, so a body that was cut, extended or replaced fails at once. A full read of a passthrough object streams: a body that was cut or extended fails at the frame where it differs, and the response then breaks off.
- The header carries the key commitment. A body under another key fails before the proxy decrypts any frame.

**Range requests.** The proxy computes the frames that cover the requested range, reads only those frames in one ranged `GET` request, decrypts them, and trims the result to the range. A range read wastes at most one frame at each end.

### Sealed metadata

The value of `dg-sealed` holds the facts of the object, encrypted and authenticated with AES-256-GCM under the metadata key of the object:

- the size of the object, its MD5 or its multipart ETag, and the SHA-256 of its content;
- for a delta: the SHA-256 of its reference and the size of the delta;
- the creation time;
- a digest of the content type and of the client's user metadata, and of nothing else.

The associated data of the seal holds the real bucket name, the folder, the name of the object and its kind (reference, delta or passthrough). The name of the backend is not part of it, so a renamed backend changes nothing. A seal therefore opens only at the location where the proxy wrote it. The facts are about 220 to 280 characters long, and they count against the 2 KB metadata limit of S3, so a client of a bucket that encrypts has less room for its own user metadata.

The proxy keeps the content type and the user metadata of the client in clear, because S3 returns them in its own headers. The seal covers them with the digest, so an edit on the backend is detected. The proxy also keeps some of its own fields in clear: the original name, the tool, the note and the reference path. The digest does not cover these fields.

### Listing tokens (S3 backends)

On an S3 backend, a plain LIST request reads only the backend listing, which holds the stored size and the stored ETag. For an encrypted object, the proxy also keeps a listing token next to the object, under `.dg/facts/`. The token holds the size and the ETag of the object, encrypted with AES-256-GCM-SIV under a listing key of the key in the configuration. Its associated data names the real bucket, the stored key, the stored ETag and the stored size. The proxy lists an encrypted object with the size and the ETag from its token. A token that does not open is ignored, and the proxy then falls back to the metadata of the object. On a backend that holds an encryption key, the proxy ignores the listing facts of the older form (`!!1`), because a writer on the backend can forge them.

An object can have no listing token. This happens when the write of its token failed, when the object was stored before listing tokens existed, or when its key is longer than about 900 bytes, because the name of the token object would then pass the S3 limit of 1024 bytes. A LIST request does not send a `HEAD` request for every such object, because a large listing would become slow. The proxy lists these objects as follows:

- **An object stored as it is, in a bucket that serves no plaintext.** Every object that this bucket can serve is encrypted, and the stored size of an encrypted body is exactly `76 + plaintext size + 16 × number of frames`. The proxy therefore computes the exact plaintext size from the stored size, with no extra request. The ETag in this first listing is still the ETag of the stored ciphertext, so it differs from the ETag of a `HEAD` request. In the background, the proxy sends one `HEAD` request for the object and writes its listing token again. After that, every LIST request shows the size and the ETag of the object.
- **A delta.** The stored size of a delta does not tell the size of the object, so the proxy sends a `HEAD` request for the delta while it answers the LIST request. It sends at most 8 such requests for one listing page, all at the same time. The `HEAD` request gives the size and the ETag of the object, and the proxy writes the listing token again, so the next LIST request needs no `HEAD` request. A delta beyond those 8 is listed with the size of the delta once, and the proxy writes its token in the background.
- **An object stored as it is, in a bucket that serves plaintext** (the bucket writes plaintext, or its conversion window is open). The object can be a plaintext object, whose stored size is its size. The proxy therefore lists the stored size and the stored ETag, and it sends no request for the object.

When the key of an object is too long for a token object, the proxy keeps the token only in its own memory, so the other proxies and a restarted proxy send the `HEAD` request for a delta again. The counter `deltaglider_listing_token_misses_total` counts these objects (see [Metrics](metrics.md)).

SSE-KMS / SSE-S3 objects carry no DG framing. AWS applies its own encryption wrapper, and the proxy passes the bytes through.

## GET behavior

The read path applies one rule to every object in a bucket:

- **An object with sealed metadata.** The proxy finds the key by the fingerprint in the seal, opens the seal at the location of the object, checks the digest of the clear headers, checks the header of the body, and then decrypts and authenticates each frame. When the proxy does not have the key that the fingerprint names, the read fails with `400 InvalidRequest`, and the message names the fingerprint (for example `key fp-8748e64455ba2257`). S3 answers in the same way when a client reads an SSE-C object without its key: a retry cannot succeed. Every other failure is `500 InternalError`, and the log names the cause.
- **An object without sealed metadata, but with `dg-encrypted`.** The proxy refuses it. The object was written by another release, or its metadata was edited.
- **An object without any marker in a bucket that encrypts.** The proxy refuses it, unless the conversion window of the bucket serves it (see the next section).
- **An object without any marker in a bucket that stores plaintext.** The proxy serves it as it is. The read path also checks the first bytes of the body: a body that starts with a `DGE` magic and a digit is proxy-encrypted, so its metadata was stripped, and the read fails instead of serving ciphertext as plaintext.
- **A folder marker** (an empty object whose key ends in `/`) always reads, because the proxy never encrypts it.
- `dg-encrypted-native: sse-kms` (or `sse-s3`): no proxy-side decryption; AWS returns plaintext.

A refused or failed read is always an error (`400 InvalidRequest` for a key that the proxy does not have, else `500 InternalError`), never corrupt data and never `404 NoSuchKey`. The proxy never treats a refused delta reference as missing, because a missing reference makes the next upload create a new baseline over it.

The rule applies to the requests that return the facts or the bytes of one object: GET, HEAD, a copy and a range read. A listing does not apply it. A listing shows a refused object with the size and the name that the backend stores in clear, and logs a warning, so that one refused object does not fail the listing of its whole folder, nor the re-encrypt and migrate jobs that page through the folder. A DELETE request removes a refused object, because a DELETE needs only the kind of the stored object, which the stored key shows. A conditional DELETE (`If-Match`) of a refused object fails, because the proxy cannot compare the ETag of an object whose facts it refuses.

A seal names the stored key exactly as the backend stores it. `releases/v1/app.zip` and `releases/v1//app.zip` are two keys on S3, so the seal of one does not open under the other.

The proxy has no reader for the earlier formats `aes-256-gcm-v1` and `aes-256-gcm-chunked-v1` (body magic `DGE1`). An object in such a format fails to read.

### Plaintext objects in a bucket that encrypts

A writer with access to the backend can put a plaintext object into a bucket that encrypts. Its bytes look the same as the plaintext history of the bucket. The proxy therefore serves an object without encryption in a bucket that encrypts only as the history of the bucket, and it judges that by time: a *conversion window* names a cutoff, and only objects that the backend stored before the cutoff are history. Outside an open window, and for every object stored later, a read fails with `500 InternalError`, and the log says:

> object 'nightly/2026-10-14.dump' in bucket 'db-archive' is not encrypted, but the bucket encrypts: the proxy refuses it, because a writer with access to the backend could have planted it (for example another proxy that does not have the bucket's key). Plaintext is served only while a conversion of the bucket is open, and only for objects that the backend stored before the conversion's cutoff: this object is newer, or no conversion is open. A re-encrypt job converts it.

The threat model of the window is in [About the encryption threat model](../explanation/encryption-threat-model.md#the-conversion-window).

#### The conversion marker

The window is one internal object in the bucket, the conversion marker:

| Backend | Where the marker is |
|---|---|
| S3 | The object `.dg/facts/!!conversion` in the real bucket. Listings do not show it, and the proxy refuses a client request for that key. |
| Filesystem | The file `.dg-conversion` in the bucket directory, next to `.dg-key-claim`. Object keys map under the `deltaspaces/` directory of the bucket, so no client key reaches the file. |

The marker holds three sealed fields:

| Field | Meaning |
|---|---|
| cutoff | The time when the window opened, in Unix seconds, from the backend's clock: the `Date` header of an S3 response, or the local clock on a filesystem backend. |
| deadline | The cutoff plus 7 days. A resume moves it to 7 days after the resume, and never makes it earlier. |
| run | A random 128-bit id of this conversion. Every instance sees the same run id, unlike a job id, which is local to one instance. The admin API shows it in hexadecimal. |

The body is `dgc1.`, the hexadecimal fingerprint of a key of the bucket, a dot, and the base64url encoding of a nonce and an AES-256-GCM-SIV seal of the fields. The proxy derives the seal key from the bucket's encryption key. The associated data of the seal is `dg-conversion`, the format version, and the storage identity of the bucket: the backend name and the real bucket name. The marker opens with any key of the bucket's keyring, so a key rotation keeps the window. A marker that does not open, that names another storage identity, or whose deadline is past counts as a closed window.

The proxy creates the marker with `If-None-Match: *`. It changes the deadline and removes the marker *by identity*: it reads the marker, opens it, checks the run id, and then writes or deletes with `If-Match` on the version that it read. When the condition fails, it reads again, up to 3 times. It never takes a failed condition for a removed marker. The proxy encrypts only on a backend with proven atomic writes (see [Encryption needs atomic writes](#encryption-needs-atomic-writes)), so the marker is always a conditional write.

Each instance keeps the state of the window in memory for 60 seconds per bucket, and it checks the deadline against the backend's clock on every use. When the proxy is about to refuse a client read while the remembered window is closed, it reads the marker again first (at most once per 5 seconds per bucket). A window that another instance opened therefore takes effect at once. A window that another instance closed can stay open on this instance for up to 60 seconds. In that time it serves only objects stored before the cutoff, which the job converted already.

#### The read rule

The rule has two readers.

- **A client** reads an object without encryption when the window is open and the backend's own stored time of the object is before the cutoff plus 300 seconds. A client read is a GET, a HEAD, a range read, the source of a copy, and every engine path that is not the re-encrypt job. The stored time is the `Last-Modified` time on S3, and the modification time of the file on a filesystem backend. It is never the proxy's `dg-created-at` field, and never a value from the metadata cache. The proxy refuses an object without a stored time. A LIST request is not affected: it shows every entry, as before.
- **The re-encrypt job** reads every object without encryption in the bucket, whatever its stored time. Only the job can read in this way, because its reads run in a scope that only the maintenance worker can open. The job converts an object that a client could not read too, and it reports the object as *adopted after the cutoff* (see [The re-encrypt job and the window](#the-re-encrypt-job-and-the-window)).

For both readers, an object that carries `dg-encrypted` but no valid sealed metadata is refused, and a folder marker (an empty object whose key ends in `/`) always reads.

#### How a conversion starts

A config write starts a conversion for a bucket when all of these are true:

- The write makes the bucket encrypt with a usable proxy key: `aes256-gcm-proxy` with a key.
- Before the write, clients could read plaintext in the bucket: it stored plaintext, used SSE-KMS or SSE-S3, or used a proxy mode without a key.
- The bucket holds at least one object. An empty bucket has no history.

Every config write does this: a section PUT, a field PATCH (`PUT /_/api/admin/config`), `POST /_/api/admin/config/apply`, `config apply`, the backend and bucket endpoints, and a backup restore. A key rotation starts no conversion. The steps are:

1. Before the config commits, the proxy refuses the write with `409` when a job other than a delete is active on the bucket. The message names the job. Wait for the job, or cancel it, and then apply again.
2. The proxy opens the window, for up to 8 buckets at once. When the bucket already holds a marker that opens with its keys (from an earlier write, from another instance, a marker whose deadline passed, or one that a stopped conversion left behind), the proxy keeps the run id of that marker, but it writes a new cutoff and a new deadline into it, by identity. The cutoff must be new, because the bucket could store plaintext since the old cutoff, and an old cutoff would refuse those objects. When the bucket holds a marker that does not open, the write fails. When the proxy cannot write a marker, it removes the markers that this write created, and the write fails.
3. The config commits.
4. The proxy queues one re-encrypt job per bucket. The job carries the run id of the marker. When the proxy cannot queue the job, because another job became active in the meantime, the write still succeeds, with a warning. The window is then open with no job, and **Resume** starts the job.

The response of the write lists each conversion in `conversions` (see [the admin API reference](admin-api.md#starting-a-conversion)). A validate request lists each bucket that the write would convert, with its object count. The admin UI shows that count in its confirmation dialog. The response does not add the general note "start the re-encrypt job" for a bucket that it lists in `conversions`, because the write queued that job already.

A config write that makes a bucket *stop* encrypting with a proxy key closes the marker of the bucket, whatever its run. The proxy removes the marker by identity, with the keys of the config that encrypted the bucket. A later write that encrypts the bucket again therefore starts a new window, with a new cutoff.

#### A conversion that a restart starts

An operator, a Helm chart or a GitOps pipeline often changes the config file and restarts the proxy, so no config write runs. The proxy therefore checks the buckets after it starts. It lists the first 100 objects of each bucket that encrypts with a proxy key and has no open window, and it sends a HEAD request for each of them. The check runs in the background, so the start does not wait for it. `DGP_BOOT_PLAINTEXT_CHECK=false` turns it off.

When the read rule refuses one of these objects because it is not encrypted, the proxy decides between two cases:

- **The sample holds no encrypted object, the bucket has no key claim and no marker, and no job is active on it.** No proxy ever wrote encrypted objects to the bucket, because the first encrypted write creates the key claim. Its plaintext objects are therefore its history. The proxy checks the sampled objects as well as the claim, because a writer with access to the backend can delete the claim, whose body is not sealed. The proxy opens the window, as a config write does, and queues the re-encrypt job of its run. The audit log has `encryption_conversion_opened` with the user `boot`, and the log names the run and the job. Clients read the history while the job runs. When the job ends, the bucket holds a key claim and no marker, so the next restart starts nothing.
- **Any other bucket.** The sample holds an encrypted object, or the bucket holds a key claim, a marker that is closed (past its deadline, or one that does not open), or an active job. Its plaintext objects can be plants, so the proxy does not decide for you. It logs a warning that names the bucket, the object, the reason, and the request that opens the window and starts the job: `POST /_/api/admin/jobs/reencrypt` with `{"buckets": ["<bucket>"], "open_window": true}`.

When several instances start at the same moment, the first one creates the marker. An instance that finds the marker after its own check keeps the run id of the marker and writes a new cutoff into it, so both instances queue a job of one run.

#### The re-encrypt job and the window

A job owns a marker only through its run id. A config write, a restart and `POST /_/api/admin/jobs/reencrypt` with `"open_window": true` store the run id of the marker in the job. A plain re-encrypt job (for example after a key rotation) has no run id: it never extends and never removes a marker.

At each start and each resume, a job with a run id reads the marker of its bucket. When the marker carries the same run id, the job moves the deadline to 7 days after the backend's current time, by identity. The cutoff and the run id stay. This also applies to a marker whose deadline passed while no job ran, so a resume opens that window again. A marker with another run id, or one that does not open, stays as it is.

The job then converts the objects and the delta references, 8 objects of a page at once, and it saves its cursor after each page. In a bucket that encrypts, an object or a delta reference that the job converts although a client could not read it is *adopted after the cutoff*. A client could not read it when the bucket has no open window of the job's run (no marker, a marker of another run, a marker that does not open, a job without a run id), or when the backend stored the object at or after the cutoff plus 300 seconds, or names no stored time. Only a bucket that stores plaintext converts such objects without a report. For each adopted object:

- The job counts it in its job row. The admin API shows the count of the latest job of the bucket as `adopted_after_cutoff`.
- The **Failures** tab of the run lists it with the prefix `adopted_after_cutoff`, for the first 100 adopted objects of the job. The row is information, not a failure. These rows never push a `rewrite_failed` row out of the 200 rows that the tab keeps.
- The job writes the audit entry `encryption_conversion_adopted_after_cutoff` for it.
- The job increments `deltaglider_conversion_adopted_after_cutoff_total`.
- The run summary names the count, for example "3 object(s) adopted after the conversion's cutoff".

An object that the job could not convert is a `rewrite_failed` row. After the last phase, a job with a run id closes the window when no object failed: it removes the marker of its run by identity. When objects failed, the window stays open, the run settles with errors, and **Resume** (`POST /_/api/admin/jobs/reencrypt` with `"open_window": true`) runs the job again with the same run id. When the marker is not the one of this run, or does not open, the job does not remove it. The run then settles with an error, and the audit log has `encryption_conversion_close_blocked`.

`POST /_/api/admin/jobs/reencrypt` with `"open_window": true` checks for an active job before it opens a window. When the bucket has an active job, the request opens nothing and returns the error. When the request created a marker and then cannot queue the job, it removes that marker by identity.

While a bucket holds a conversion marker, the proxy refuses a metadata backfill of the bucket (`POST /_/api/admin/jobs/backfill-metadata`) with `409`, because the backfill would move the stored time of the plaintext objects past the cutoff. The proxy reads the marker on the backend, whatever the bucket's current config says, and it holds the config lock from this check until it creates the backfill job, so that a config write cannot open a window in between. The backfill job checks the marker again before it starts.

#### Conditional rewrites and the write gate

The job reads each object, and then it stores the same object again through the encryption. Every such write is conditional on the version of the object that the job read. On an S3 backend, the job sends the `PUT` with `If-Match` and the ETag of its read. On a filesystem backend, the job checks the version of the file (its inode, its modification time and its size) under the write lock of the object, and every write of that object on the instance takes the same lock. A filesystem backend has one node, so that lock is enough.

When a client writes or deletes the object after the job read it, the condition does not hold, and the backend stores nothing for the job. The object stays as the client left it. A client write through an instance that encrypts the bucket is already encrypted, so the object needs no rewrite. The job counts the object as skipped, and also in `objects_skipped_changed` of the run (`GET /_/api/admin/jobs/maintenance:<id>/runs`). The run summary names the count, for example "1 object(s) skipped as changed". Such an object is never a failure. The job rewrites the delta references in the same way, under the lock of their deltaspace.

Because no write of the job can overwrite a client's write, the job takes no write gate when the proxy can prove that the backend enforces conditional writes:

- A filesystem backend always qualifies.
- An S3 backend qualifies when its capability verdict says that it enforces conditional writes. Without a verdict, the proxy runs the capability probe when it queues the job (one conditional `PUT` pair on a temporary key, which it then deletes). See [How to validate backend capability](../how-to/backend-capability-validation.md).

On such a backend, clients keep writing to the bucket while the job runs, on every instance, and get no `503`. The job decides this when it is queued, and again each time a run starts. A job that encrypts runs only on such a backend (see [Encryption needs atomic writes](#encryption-needs-atomic-writes)). A decrypt job (target `none`) can also run on a backend that does not enforce conditional writes, or one that the probe could not check. Such a job keeps the write gate: writes to the bucket on the instance that runs the job get `503 SlowDown` until the job ends. The `writes_blocked` field of `GET /_/api/admin/jobs/bucket/:bucket` says which case applies.

#### A conversion on several instances

On a backend that enforces conditional writes, a conversion needs no extra step with several instances. A client write through any instance either lands before the job reads the object (and the job then encrypts that version), or after the read (and the job then skips the object, which the writing instance encrypted already). Apply the encryption on every instance first, so that every instance encrypts its writes, and then let the job finish.

On a backend that keeps the write gate, the gate stops client writes to the bucket only on the instance that runs the job. The job still writes conditionally where the backend allows it, but a backend that refuses a condition with `501 NotImplemented` gets an unconditional write. Only a decrypt job (target `none`) runs on such a backend, because an encrypting job needs atomic writes. While such a conversion runs in a deployment with several instances, do one of these two things until the job ends:

- Stop client writes to the bucket on every instance (for example with a request rule that rejects writes to the bucket).
- Send every write to the bucket to the instance that runs the job (for example with the directory-hash router of the Kubernetes operator, or a sticky route for the bucket).

Each instance whose config write starts a conversion queues its own job, with the run id of the one shared marker. The jobs are idempotent: a job skips an object that is already encrypted with the right key, so a second job only reads the bucket again. The first job that converts every object closes the window. A job that ends later finds no marker and ends normally.
#### Old versions on a versioned backend

On an S3 backend whose bucket keeps old versions (versioning enabled or suspended), the job rewrites the current object of each key. The plaintext object becomes a noncurrent version, and it stays on the backend. The validate and the apply mark such a bucket with `versioned: true`, and the apply response has a warning for it.

The job can delete these versions in an optional last phase, `purge-versions`. The phase runs after the delta references and before the job closes the window. The default is off. You ask for it in one of three ways:

- In the admin UI, select the box **Also delete the old unencrypted versions** in the confirmation dialog of a versioned bucket.
- On a config write, send the header `X-DGP-Purge-Plaintext-Versions: true`. It applies to the jobs that the write queues for versioned buckets.
- In `POST /_/api/admin/jobs/reencrypt`, send `"purge_plaintext_versions": true`.

**A deleted version cannot be restored.** The phase deletes a version only when all of these are true:

- The version is noncurrent. The phase never deletes a current version.
- The version carries no encryption marker (`dg-encrypted`), so it is plaintext. The phase never deletes an encrypted version.
- The object of its key exists now, and it is encrypted in every stored form that exists. An object has two stored forms: the object as it is (`app.zip`) and its delta (`app.zip.delta`), because a rewrite can change the form.
- The key is not an internal key under `.dg/facts/`.

The phase therefore keeps the versions of an object that a client deleted before the conversion, because that key has no current object. A delta reference (`reference.bin`) is different, because no client can read it: the phase deletes its plaintext versions also when the proxy removed the reference, as long as no current plaintext reference exists. The phase runs only when the run converted every object. When objects failed, the run summary says that the versions were not deleted, and a later run that converts every object deletes them. The phase reads the metadata of up to 8 versions at once, deletes the versions of one page of the version listing with one `DeleteObjects` request, saves its cursor after each page of versions, and resumes from that cursor after a restart. The run summary names the count, for example "12 noncurrent plaintext version(s) deleted", and each page with deletions writes the audit entry `encryption_conversion_versions_purged`. A bucket without versioning, and a bucket on a filesystem backend, have nothing to delete.

#### Metrics and audit entries

| Name | Kind | Meaning |
|---|---|---|
| `deltaglider_plaintext_reads_total{outcome}` | Counter | Client reads of objects without encryption in buckets that encrypt. `outcome` is `served_in_window` (history that an open window serves) or `refused`. |
| `deltaglider_conversion_adopted_after_cutoff_total` | Counter | Objects that the re-encrypt job converted although clients could not read them. |
| `encryption_conversion_opened` | Audit | A config write, a restart (user `boot`), or `POST /jobs/reencrypt` with `open_window`, opened a window or wrote a new cutoff into one. The target names the run id. |
| `encryption_conversion_extended` | Audit | A job start or a resume moved the deadline. The target names the run, the kept cutoff, and the old and the new deadline. |
| `encryption_conversion_closed` | Audit | The job closed the window (the target names the count of objects adopted after the cutoff), or a config write removed a marker: the write that opened it failed, or the bucket stopped encrypting. |
| `encryption_conversion_close_blocked` | Audit | The job did not close the window, because the marker is not the one of its run, or does not open. |
| `encryption_conversion_adopted_after_cutoff` | Audit | The job converted an object that clients could not read. |
| `encryption_conversion_versions_purged` | Audit | The `purge-versions` phase deleted noncurrent plaintext versions. The target names the job and the count. |
| `encryption_marker_ignored` | Audit | A marker of the bucket does not open on this instance (at most one entry per bucket every 10 minutes). |

### Copies outside the proxy

The sealed metadata names the real bucket, the folder and the name of the object. A copy that the backend makes on its own therefore does not open at its new location: an `rclone` copy into another bucket, a restore of a backup under another bucket name, or a key that you move by hand. The proxy refuses such a copy, with a message that the sealed metadata failed authentication. This release has no setting that reads an object at another location. To move encrypted objects, copy them through the proxy (`CopyObject`, replication, the migrate job, or a lifecycle transition): the proxy then writes new sealed metadata for the destination. A backup of the backend restores readable objects only under the same bucket names and keys.

### Buckets that another proxy encrypts

Two proxies with separate configurations can store objects in one bucket of a backend. For example, a second proxy has the credentials of `hetzner-fsn1` and writes to `db-archive`, which the first proxy encrypts. When the second proxy does not have the key of `db-archive`, it cannot read the encrypted objects, and the first proxy refuses every object that the second proxy writes (see [Plaintext objects in a bucket that encrypts](#plaintext-objects-in-a-bucket-that-encrypts)). The encryption witness does not help here, because it works only between instances that share a `config_sync_bucket`.

A proxy therefore checks the bucket itself before it writes. It uses three kinds of evidence:

- **The key claim.** A proxy that encrypts a bucket claims the bucket for its key. The claim is one small internal object (`.dg/facts/!!key-claim` on S3, `.dg-key-claim` in the bucket directory on a filesystem backend) that holds the fingerprint of the key. The fingerprint identifies the key but does not reveal it. The proxy creates the claim with a conditional write (`If-None-Match: *`), so when two proxies with different keys claim one bucket at the same moment, only one of them succeeds. The claim does not depend on the size of the bucket, because the proxy reads one object. A proxy that encrypts reads the claim every 10 minutes, and a proxy that writes plaintext reads it every minute. Listings do not show the claim, and DeleteBucket removes it.
- **Reads.** The sealed metadata of each object, and the listing token of each object on an S3 backend, start with the fingerprint of the key. Every GET, HEAD, listing with metadata and delta upload reads the sealed metadata of an object, and a LIST request of a proxy that holds a key reads the listing tokens. When the proxy does not have the key that it finds, it records the fingerprint for the bucket. This costs no extra request.
- **A sample.** Before the first write to a bucket, and then every 10 minutes, the proxy lists the first 200 keys of the bucket. It reads the listing tokens of those keys (on S3, one more LIST request of `.dg/facts/`), and sends a HEAD request for up to 8 objects: the delta references first, and then objects from the whole list. The sample stops at the first key that the proxy does not have. The sample finds the buckets that an earlier release encrypted, because such buckets have no claim. A proxy takes the sample before it claims a bucket, so it never claims a bucket that holds objects of another key.

What a proxy does with the claim depends on its own key:

| The claim | The proxy writes with | Result |
|---|---|---|
| none | a key | The proxy creates the claim for its key. |
| none | no key | The write goes ahead (when no other evidence refuses it). |
| a key that the proxy does not have | anything | The write is refused. |
| the proxy's write key | that key | The write goes ahead. |
| an older key of the proxy (`legacy_key` or the keyring) | a new key | The proxy moves the claim to the new key. This is a key rotation. Other proxies that have only the older key are then refused. |
| a key of the proxy | no key | The proxy removes the claim, because the bucket stops encrypting. |

The proxy encrypts only on a backend with proven atomic writes (see [Encryption needs atomic writes](#encryption-needs-atomic-writes)), so the claim is always a conditional write. In a bucket that encrypts on a backend without proven atomic writes (a read-only or pending bucket), the proxy writes no claim at all.

When the claim or an object names a key that the proxy does not have, every write to the bucket fails with `400 InvalidRequest`, and nothing is written. The writes are PUT, multipart uploads, copies into the bucket, metadata rewrites and folder markers. The client gets only the label of the key, because the other facts (the real bucket name behind an alias, the names of other objects, the id of this proxy's key) are not the client's to know:

> This bucket holds objects that are encrypted with key fp-8748e64455ba2257, which this proxy does not have, so the proxy refuses to write to it. Reads and deletes still work.

The log of the proxy has the whole cause, for example:

> bucket 'db-archive' holds objects that were encrypted with a key that this proxy does not have (key fp-8748e64455ba2257, for example 'nightly/2026-10-14.dump'). This proxy would write without proxy encryption, and the proxy that holds that key could not read the result, so it refuses the write. Add that key to this proxy's encryption config of the bucket, or send the writes through the proxy that holds it. Reads and deletes still work

The rule is the same for a proxy that encrypts with another key, because the first proxy cannot read those objects either. A DELETE request is not refused, so that the second proxy can remove the objects that it wrote. A replication or lifecycle run that writes into such a bucket stops as `deferred` and tries again at its next tick, as it does for a bucket that the encryption witness refuses, instead of failing object after object. The proxy compares the fingerprints with its current keys at every write. When you add the key to the configuration, the writes work at once. A key in `legacy_key` or in the keyring counts, so a key rotation or a change to native SSE does not refuse writes. A bucket in `aes256-gcm-proxy` mode without a key is not checked, because the proxy refuses its writes anyway.

Every piece of evidence expires, so a bucket becomes writable again when the evidence is gone. A fingerprint that the proxy does not see again for one hour stops counting. A claim of another key is read again every minute, so a removed claim stops refusing writes within a minute. When the sample or the claim fails (for example, the credentials cannot list the bucket, or a bucket policy refuses the claim), the proxy logs one warning, lets the writes go ahead on the evidence that it has, and tries again after one minute. No client request can read, write or delete the claim: keys under `.dg/facts/` are reserved.

A proxy that writes to a bucket reads the claim of that bucket once a minute (a proxy that writes plaintext) or once every 10 minutes (a proxy that encrypts), and takes the sample every 10 minutes. On S3, the sample is two LIST requests and up to 8 HEAD requests.

##### Rotate a key on proxies that share a bucket

When more than one proxy writes a bucket with the same key, change the key in this order, so that no proxy is refused during the change:

1. On every proxy, add the new key as a read-only key (in the keyring of the backend, or as `legacy_key`). Every proxy can then read objects of both keys.
2. On every proxy, make the new key the write key, and keep the old key as `legacy_key`. The first proxy that writes moves the claim to the new key. The other proxies hold the new key, so they are not refused.
3. Run a re-encrypt job, and remove the old key when the job succeeds.

#### What the claim does not stop

The claim stops two proxies with different keys from both writing one bucket, also when both start on an empty bucket at the same moment. These cases stay open:

- **A proxy of an earlier release on a backend without conditional writes.** A release up to v2.0.23 encrypts also on a backend without atomic writes, such as Backblaze B2, and writes its claim there without a condition. Two such proxies that start on an empty bucket at the same moment can both write their claim. This release makes such a bucket read-only, so it writes no claim there.
- **A key rotation out of order.** A proxy that rotates its key (and keeps the old key as `legacy_key`) moves the claim to the new key. A second proxy that has only the old key reads its cached claim, which still names its own key, for up to 10 minutes. Until then, it keeps writing with the old key, and the second proxy cannot read the objects of the new key. The order in [Rotate a key on proxies that share a bucket](#rotate-a-key-on-proxies-that-share-a-bucket) avoids this.
- **A write in flight.** A write that the proxy checked before the claim changed still completes.
- **An earlier release.** A proxy of a release before this check ignores the claim, the sample and the sightings. Upgrade every proxy that writes the bucket.
- **The holder of the backend credentials.** A writer with the credentials of the backend can delete or replace the claim, like any other object (see [About the encryption threat model](../explanation/encryption-threat-model.md)).

There are two ways to let a second proxy write to the bucket:

- **Send the writes through the proxy that holds the key.** Create an IAM user on the first proxy, with permissions on the folders that the other team needs, and let their clients send requests for the bucket to the first proxy. The key then stays on one proxy, and the IAM permissions of the first proxy limit what the other team can read and write.
- **Give the second proxy the same key** for the bucket. The sealed metadata names the real bucket, the folder and the name of the object, but not the name of the backend or of a bucket alias, so both proxies read each other's objects. A proxy with the key can read every object of the bucket, so do this only for a proxy that you trust with all of it. The two proxies also do not share their locks, so do not let both of them store delta-compressed objects in the same folder.

### The `legacy_key` shim

When `legacy_key` is set, the keyring holds it next to `key`. A read picks the key by the fingerprint of the object, so the objects of the old key decrypt with `legacy_key`. `legacy_key_id` is the id under which the proxy shows these objects. The shim does not affect writes. They go through the backend's current mode only (under a native mode, `WriteMode::PassThrough` skips the proxy-AES path entirely). The shim holds exactly one legacy key generation, works under every mode including `none`, and the admin panel shows an info banner while one is active. When you click **Check usage**, the banner counts the objects and delta references that the legacy key still encrypts (`GET /_/api/admin/backends/:name/legacy-key-usage`). The banner does not count by itself, because the count reads the metadata of every object on the backend. Its **Clear legacy key** button is enabled only when a count is complete and zero. The native → proxy-AES direction needs no shim: native objects carry `dg-encrypted-native`, so the proxy decrypt path does not fire.

## Limits

- **Key rotation keeps one previous key.** When an apply changes `key`, sets it to `null`, or moves a backend away from `aes256-gcm-proxy`, and does not set `legacy_key`, the proxy moves the previous key into `legacy_key`, and its effective id into `legacy_key_id`. An apply that sends `legacy_key: null` drops the previous key instead. The shim holds one legacy generation at a time, so the proxy refuses a key change while `legacy_key` holds a different key. It also refuses a key change that keeps the same `key_id`. A Re-encrypt job (`POST /_/api/admin/jobs/reencrypt`, or **Storage → Jobs** → **New job** → **Re-encrypt buckets…**) rewrites objects under the current configuration; it is durable, resumable across restarts, and cancellable. Each rewrite is conditional on the object that the job read, so the job gates writes (`503 SlowDown`; reads unaffected) only on a backend without proven conditional writes ([details](#conditional-rewrites-and-the-write-gate)).
- **An apply keeps what it does not name.** For `key`, `key_id` and `legacy_key`, the same rule applies to every apply, whether it is a section `PUT` or a full document. When the field is absent, the proxy keeps the current value. When the field is `null`, the proxy clears it. A `key: null` is a key change like any other: the previous key moves into `legacy_key` (or the apply is refused while `legacy_key` holds a different key), so the proxy never drops a key that objects can still need. A backend that is left in `aes256-gcm-proxy` mode with no key is then refused, as described in [A proxy-AES backend with no key](#a-proxy-aes-backend-with-no-key). When the field has a value, that value replaces the current one. An id is kept only together with its own key: when the apply sets a new `key` and no `key_id`, the new key gets the generated id `fp-<hex>`, and the previous id moves to `legacy_key_id` with the previous key.
- **Enabling is not retroactive.** Switching a backend's mode affects new writes only; existing objects keep their stored form until the re-encrypt job rewrites them. In a bucket that encrypts, a plaintext object reads only while the conversion window of the bucket is open, and only when the backend stored it before the cutoff (see [Plaintext objects in a bucket that encrypts](#plaintext-objects-in-a-bucket-that-encrypts)). A config write that makes a bucket with objects start to encrypt opens the window and starts the job.
- **Key loss is data loss** in `aes256-gcm-proxy` mode. The proxy does not escrow keys, and there is no recovery path.
- **Per-bucket native SSE needs an S3 backend.** A bucket `encryption` block takes `none` on every backend, `aes256-gcm-proxy` only on a backend with proven atomic writes (see [Encryption needs atomic writes](#encryption-needs-atomic-writes)), and `sse-kms` or `sse-s3` only on an S3 backend. With `config_sync_bucket` set, the encryption witness keeps the instances in agreement. See [Per-bucket encryption](#per-bucket-encryption).
- **Names and some metadata stay readable on the backend.** Under every mode, the backend stores object names, content-type and `x-amz-meta-*` user metadata unencrypted. In `aes256-gcm-proxy` mode, the sealed metadata authenticates the content type and the client's user metadata, and it hides the hashes and the size of a delta. The stored size of a passthrough object or a reference still gives its exact plaintext size, and the size of a delta stays readable. Under SSE-KMS and SSE-S3, the backend also stores the hashes and the sizes in clear. [About the encryption threat model](../explanation/encryption-threat-model.md) lists what each attacker learns.
- **One key per bucket across proxies, with short windows.** A proxy does not write a bucket that another proxy encrypts with a key that it does not have. For up to 10 minutes after a key rotation that does not follow the documented order, two keys can still reach one bucket. See [What the claim does not stop](#what-the-claim-does-not-stop).
- **Copies outside the proxy are unreadable.** An encrypted object that the backend copies to another bucket name or key does not read there (see [Copies outside the proxy](#copies-outside-the-proxy)).
- **Less room for user metadata.** The sealed metadata takes about 220 to 280 bytes of the 2 KB S3 metadata limit.
- **No forward secrecy.** A disclosed proxy-AES key decrypts all past ciphertext written under it.
- **Memory.** Encrypted GET of a passthrough object is streaming: the decoder holds ~130 KiB in flight regardless of object size, and a range GET fetches only the frames that cover the range, in one request. The reference of a large delta object is decrypted frame by frame into the spool directory, so its size does not change the memory that a delta request needs. The spool files hold plaintext: see [Plaintext in the spool directory](../explanation/encryption-threat-model.md#plaintext-in-the-spool-directory). An encrypted PUT in proxy-AES mode of a body that the proxy holds in memory buffers every encrypted frame before handing off to the inner backend: peak write memory ≈ twice the plaintext size (the plaintext and its ciphertext, which is 0.03% larger). The proxy holds a body in memory only when it is at most `DGP_SPOOL_THRESHOLD_BYTES` (16 MiB by default) and the upload memory budget (`DGP_INGEST_MEMORY_MAX_BYTES`) has room for it, so one such upload uses at most about 32 MiB, and all of them together about twice `DGP_INGEST_MEMORY_MAX_BYTES`. A body that the proxy stores from a file (a larger upload, or a multipart upload whose parts are relay files) is encrypted into a temporary file in `DGP_SPOOL_DIR` instead. That file counts against `DGP_SPOOL_MAX_BYTES`, and the spool reservation that the upload takes before its body arrives already covers it. A large upload that finds the budget in use waits for space, up to `DGP_SPOOL_ACQUIRE_TIMEOUT_SECS` (default 120 seconds), and only then gets `503 SlowDown`. A small body that finds no room in memory takes free spool space without waiting, and gets `503 SlowDown` only when no spool space is free. The proxy rejects passthrough objects above `max_object_size` (default 100 MiB) up front. SSE-KMS / SSE-S3 stream through without this buffering.
- **Latency.** AES-256-GCM throughput is roughly 1 to 3 GB/s per core with AES-NI. A 100 MiB proxy-AES upload adds ~30 to 100 ms of proxy-side crypto work. Native SSE modes move this cost to AWS.
- The pre-v0.9 global `advanced.encryption_key` field no longer exists; encryption is configured per backend.

## Related

- [About encryption at rest](../explanation/encryption-at-rest.md)
- [About the encryption threat model](../explanation/encryption-threat-model.md)
- [How to encrypt data at rest](../how-to/encrypt-data-at-rest.md)
- [How to rotate or change encryption keys](../how-to/rotate-encryption-keys.md)
