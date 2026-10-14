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
| `key_id` | `aes256-gcm-proxy` | Stable identifier stamped on written objects. On a backend, optional: derived from backend name + key when absent. On a bucket, required. |
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

When a backend is in `aes256-gcm-proxy` mode and neither the YAML nor the env var supplies a `key`, the proxy cannot encrypt. It does not fall back to plaintext. Instead, every write to that backend fails with `500 InternalError`, and the log names the backend and the env var to set. Reads keep working: plaintext objects, and objects that the `legacy_key` can decrypt, stay readable while you fix the key. The proxy starts with such a config, so that the stored data stays available. A config apply that would put a backend into this state is refused. `config lint` only warns about it, because lint does not see the env vars of the deployment.

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
| `key_id` is set explicitly. | Every object carries the id of its key, and the proxy picks the key by that id when it reads the object. A bucket key id is therefore never derived from a name: a renamed bucket or an alias would otherwise change the id. |
| `legacy_key_id` is set explicitly when `legacy_key` is set. | The same reason. |
| The bucket has `backend:` when `storage.backends` is used. | A bucket without a route can live on any backend, so the proxy cannot tell which storage the block applies to. With a single backend (`storage.backend`), the bucket is always on that backend, and `backend:` is not needed. |
| Every name of one stored bucket has the same effective encryption. | The encryption belongs to the stored bucket (the backend and the real bucket name), not to a name. When two bucket names reach the same real bucket through `alias`, they must carry the same `encryption` block, or none. |
| A key id names one key. | Two keys with the same `key_id` and different key bytes are refused, across all backends and all buckets. The same key with the same id in two places is allowed. |

With `config_sync_bucket` set (several instances), the encryption witness makes an instance that disagrees with the others about a bucket's encryption refuse writes to that bucket. See [How to run multiple instances](../how-to/run-multiple-instances.md#8-keep-the-encryption-of-each-bucket-the-same-on-every-instance).

A bucket key has no `DGP_*` environment variable. Put the key in the YAML file, or reference any environment variable with `${env:NAME}`, as in the example.

A bucket block in `aes256-gcm-proxy` mode with no key makes the proxy refuse every write to that bucket, as for a backend (see [A proxy-AES backend with no key](#a-proxy-aes-backend-with-no-key)). An apply that puts a bucket into this state is refused.

### Changing a bucket's encryption

When an apply changes the encryption that a bucket writes with, the proxy does not rewrite the objects that the bucket already holds. They keep the form they were written in, and they stay readable, because each object carries the id of its key. The response of the apply has a warning for each such bucket. The warning names the re-encrypt job (`POST /_/api/admin/jobs/reencrypt` with `{"buckets": ["<bucket>"]}`), which rewrites the objects and the delta references into the new form. The proxy does not start the job by itself.

The proxy judges every key of a bucket's block on that bucket alone. After an apply, the bucket must still be able to read each key id that its block held: in its own block, or in the encryption of the backend that stores it. A key that another bucket on the same backend holds does not count, because that bucket can change its own block later.

- A key that a bucket writes with cannot leave in one step. An apply that removes it is refused. The objects written under that key would otherwise become unreadable.
- A legacy (decrypt-only) key of a bucket leaves only when the request confirms its id. Send the header `X-DGP-Confirm-Key-Removal: <key id>` with the apply, or `config apply --confirm-key-removal <key id>`. Without the confirmation, the apply is refused.

To stop using a bucket key, do these steps:

1. Set the bucket's `mode` to `none`, or give the bucket a new key and key id. An apply that leaves `legacy_key` out keeps the old key as the bucket's `legacy_key`, with its id as `legacy_key_id`.
2. Run the re-encrypt job for the bucket.
3. Check that no object still uses the old key: `GET /_/api/admin/backends/<backend>/legacy-key-usage?bucket=<bucket>`. This scan counts the objects under that key id in every bucket stored on the backend, because an object can carry the id in another bucket too (for example, when two buckets shared the key, or when an object was moved at the storage level).
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

Each backend has one encrypting layer, which holds every key that the config gives to that backend: the key and legacy key of the backend, and the key and legacy key of each bucket on it. When the proxy reads an object, it picks the key by the `dg-encryption-key-id` of the object. A bucket therefore reads the objects that it wrote under the backend key before it got its own key, and it reads them again after its own key is retired into `legacy_key`. An object without a key id (written before key ids existed) is decrypted with the key of the backend.

When the proxy writes an object, it picks the key of the stored bucket that the object goes to. A server-side copy inside one backend keeps the stored bytes only when the destination bucket writes with the same key id as the source object. Otherwise the proxy decrypts the object and encrypts it again with the key of the destination.

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

## Key check value

With `config_sync_bucket` set, the encryption witness publishes, for each proxy-AES key that a bucket writes with, a key check value (`kcv`) next to the key id: the first 8 bytes, in hex, of HMAC-SHA256 with the key as the HMAC key and the text `dgp-witness-kcv-v1` as the message. Two instances that use the same key id with different key bytes then disagree, and the witness refuses writes on the instance that differs. The value is a one-way function of the key, and only 64 bits of it are published, so it reveals nothing practical about a random 256-bit key. It also changes the audit entry of a change that keeps the key id but changes the key: that change is a `bucket_encryption_key_rotated`.

## Key IDs

Every proxy-AES write stamps a `dg-encryption-key-id` metadata field on the object. The id is either the explicit `key_id` from YAML or derived as the first 8 bytes of `SHA-256(backend_name ‖ 0x00 ‖ key)`, written as 16 hex characters. The backend name is part of the derivation: two backends with identical key material but different names produce different ids, so objects are not portable across backends by default. Pinning the same explicit `key_id` with identical key bytes on two backends is the documented portability escape hatch.

On read, the proxy compares the object's stamped id against the backend's configured `key_id`, then against `legacy_key_id`. A mismatch produces a specific error rather than an opaque GCM authentication failure:

> object was encrypted with key id 'obj-foo', but this backend is configured with key id 'backend-bar' (no legacy-shim match either). This usually means: (a) the key was rotated without `legacy_key` set — restore the old key alongside the new one; (b) this bucket is routed to the wrong backend; (c) two backends share physical storage with different keys.

Objects written before the `dg-encryption-key-id` stamp existed carry no id; they decrypt as long as the key material matches. At startup, two backends declaring the same explicit `key_id` with different key bytes is a fatal error at engine construction.

## Metadata markers

| Marker | Value | Meaning |
|---|---|---|
| `dg-encrypted` | `aes-256-gcm-v1` | Proxy-AES, single-shot (deltas and references) |
| `dg-encrypted` | `aes-256-gcm-chunked-v1` | Proxy-AES, chunked wire format (passthrough bodies) |
| `dg-encrypted-native` | `sse-kms` or `sse-s3` | Native SSE; no proxy-side decryption |
| `dg-encryption-key-id` | key id string | The proxy-AES key generation the object was written under |

On filesystem backends these markers live in the `user.dg.metadata` xattr; on S3 backends, in S3 user metadata.

## Chunked wire format (proxy-AES)

Passthrough bodies use this format. The codec slices plaintext into 64-KiB windows and produces this layout:

```
┌──────────┬───────────┬────────────────────────────────────────────────┐
│ 4 B      │ 12 B      │ repeated N times:                              │
│ magic    │ base_iv   │ ┌─────────┬───────────────────────────┐        │
│ "DGE1"   │ (random)  │ │ 4 B len │ ciphertext (inc 16 B tag) │ …      │
│          │           │ │ u32 LE  │                           │        │
│          │           │ └─────────┴───────────────────────────┘        │
└──────────┴───────────┴────────────────────────────────────────────────┘
```

- **Per-chunk nonce**: `base_iv XOR (chunk_index as big-endian u96)`. Unique up to 2³² chunks (= 256 TiB per object).
- **AAD for chunk `i`**: `"DGE1" || chunk_index_le_u32 || final_flag_u8 || 0x00 0x00 0x00`. Binds the index (foils reorder attacks) and the final flag (foils truncation attacks).
- **Chunk plaintext size**: 64 KiB. Overhead: 20 B/chunk = 0.03%. Range-read trim cost: ≤ 64 KiB at each end.

Deltas and references stay single-shot (`aes-256-gcm-v1`). They are bounded by `max_object_size` (100 MiB default), so chunking would be wasted overhead.

SSE-KMS / SSE-S3 objects carry no DG wire framing. AWS applies its own encryption wrapper, and the proxy passes the bytes through.

## GET behavior

The reader dispatches on the object's metadata markers:

- `dg-encrypted: aes-256-gcm-v1`: decrypt single-shot with the backend's proxy-AES key.
- `dg-encrypted: aes-256-gcm-chunked-v1`: stream-decrypt chunk by chunk. Range requests compute the first and last chunk in O(1) (every non-final frame is exactly 65556 wire bytes), fetch only those chunks, decrypt, and trim to the client's `[start, end]`. The proxy reads the object's metadata from the response of its first ranged `GET`, so it sends no `HEAD` for it. A range that starts in the first chunk (for example `bytes=0-`, the first request of a video player) needs one ranged `GET`, because the 16-byte header and the first chunk are contiguous. A range that starts later needs two: one for the header and one for the chunks.
- `dg-encrypted-native: sse-kms` (or `sse-s3`): no proxy-side decryption; AWS returns plaintext.
- Absent marker: the proxy serves the object as-is. The read path also sniffs the first 4 bytes of the body. If the `DGE1` magic is present but the metadata marker is missing (for example, because a backup/restore round-trip stripped the xattrs), the read fails and does not serve ciphertext as plaintext.

Truncation, reordering, and tampering of proxy-AES objects fail at GCM verification; the client receives `500 InternalError`, never corrupt data.

### The `legacy_key` shim

When `legacy_key` / `legacy_key_id` are set, reads check the object's `dg-encryption-key-id` against `key_id` first, then against `legacy_key_id`. Objects that match the legacy slot decrypt with `legacy_key`. The shim does not affect writes. They go through the backend's current mode only (under a native mode, `WriteMode::PassThrough` skips the proxy-AES path entirely). The shim holds exactly one legacy key generation, works under every mode including `none`, and the admin panel shows an info banner while one is active. When you click **Check usage**, the banner counts the objects and delta references that still carry `legacy_key_id` (`GET /_/api/admin/backends/:name/legacy-key-usage`). The banner does not count by itself, because the count reads the metadata of every object on the backend. Its **Clear legacy key** button is enabled only when a count is complete and zero. The native → proxy-AES direction needs no shim: native objects carry `dg-encrypted-native`, so the proxy decrypt path does not fire.

## Limits

- **Key rotation keeps one previous key.** When an apply changes `key`, sets it to `null`, or moves a backend away from `aes256-gcm-proxy`, and does not set `legacy_key`, the proxy moves the previous key into `legacy_key`, and its effective id into `legacy_key_id`. An apply that sends `legacy_key: null` drops the previous key instead. The shim holds one legacy generation at a time, so the proxy refuses a key change while `legacy_key` holds a different key. It also refuses a key change that keeps the same `key_id`. A Re-encrypt job (`POST /_/api/admin/jobs/reencrypt`, or Jobs → + New job → Re-encrypt buckets…) rewrites objects under the current configuration; it is durable, resumable across restarts, cancellable, and write-gates affected buckets (`503 SlowDown` on writes; reads unaffected).
- **An apply keeps what it does not name.** For `key`, `key_id` and `legacy_key`, the same rule applies to every apply, whether it is a section `PUT` or a full document. When the field is absent, the proxy keeps the current value. When the field is `null`, the proxy clears it. A `key: null` is a key change like any other: the previous key moves into `legacy_key` (or the apply is refused while `legacy_key` holds a different key), so the proxy never drops a key that objects can still need. A backend that is left in `aes256-gcm-proxy` mode with no key is then refused, as described in [A proxy-AES backend with no key](#a-proxy-aes-backend-with-no-key). When the field has a value, that value replaces the current one. An id is kept only together with its own key: when the apply sets a new `key` and no `key_id`, the new key gets a derived id, and the previous id moves to `legacy_key_id` with the previous key.
- **Enabling is not retroactive.** Switching a backend's mode affects new writes only; existing objects keep their stored form and markers until rewritten.
- **Key loss is data loss** in `aes256-gcm-proxy` mode. The proxy does not escrow keys, and there is no recovery path.
- **Per-bucket encryption is proxy-AES only and single-instance only.** A bucket `encryption` block takes `aes256-gcm-proxy` or `none`, and the proxy refuses it while `config_sync_bucket` is set. See [Per-bucket encryption](#per-bucket-encryption).
- **Metadata is plaintext** under every mode, including SSE-KMS: object names, sizes, content-type, and `x-amz-meta-*` user metadata are stored unencrypted.
- **No forward secrecy.** A disclosed proxy-AES key decrypts all past ciphertext written under it.
- **Memory.** Encrypted GET is streaming: the decoder holds ~130 KiB in flight regardless of object size, and range GETs fetch only the target chunks plus the 16-byte header (in the same request when the range starts in the first chunk). An encrypted PUT in proxy-AES mode of a body that the proxy holds in memory buffers every encrypted frame before handing off to the inner backend: peak write memory ≈ plaintext size + 0.03%, so a 100 MiB `PutObject` peaks around 200 to 300 MiB RSS. A body that the proxy stores from a file (a large upload, or a multipart upload whose parts are relay files) is encrypted into a temporary file in `DGP_SPOOL_DIR` instead. That file counts against `DGP_SPOOL_MAX_BYTES`, and when the budget is in use the upload gets `503 SlowDown`. The proxy rejects passthrough objects above `max_object_size` (default 100 MiB) up front. SSE-KMS / SSE-S3 stream through without this buffering.
- **Latency.** AES-256-GCM throughput is roughly 1 to 3 GB/s per core with AES-NI. A 100 MiB proxy-AES upload adds ~30 to 100 ms of proxy-side crypto work. Native SSE modes move this cost to AWS.
- The pre-v0.9 global `advanced.encryption_key` field no longer exists; encryption is configured per backend.

## Related

- [About encryption at rest](../explanation/encryption-at-rest.md)
- [How to encrypt data at rest](../how-to/encrypt-data-at-rest.md)
- [How to rotate or change encryption keys](../how-to/rotate-encryption-keys.md)
