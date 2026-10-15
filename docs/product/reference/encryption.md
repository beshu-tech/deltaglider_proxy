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
| `plaintext_reads` | `aes256-gcm-proxy` | `refuse` (default) or `serve`. What a read does with an object that is not encrypted. See [Plaintext objects in a bucket that encrypts](#plaintext-objects-in-a-bucket-that-encrypts). |
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

When a backend is in `aes256-gcm-proxy` mode and neither the YAML nor the env var supplies a `key`, the proxy cannot encrypt. It does not fall back to plaintext. Instead, every write to that backend fails with `500 InternalError`, and the log names the backend and the env var to set. Reads keep working: the objects that the `legacy_key` can decrypt stay readable while you fix the key. A plaintext object stays readable only with `plaintext_reads: serve`, because the backend counts as a backend that encrypts. The proxy starts with such a config, so that the stored data stays available. A config apply that would put a backend into this state is refused. `config lint` only warns about it, because lint does not see the env vars of the deployment.

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

When an apply changes the encryption that a bucket writes with, the proxy does not rewrite the objects that the bucket already holds. They keep the form they were written in. An encrypted object stays readable while the keyring holds its key, because each object carries the fingerprint of its key. A plaintext object stays readable only when the new block has `plaintext_reads: serve` (see [Plaintext objects in a bucket that encrypts](#plaintext-objects-in-a-bucket-that-encrypts)). The response of the apply has a warning for each such bucket. The warning names the re-encrypt job (`POST /_/api/admin/jobs/reencrypt` with `{"buckets": ["<bucket>"]}`), which rewrites the objects and the delta references into the new form. The proxy does not start the job by itself.

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
- a digest of the content type and of the user metadata that the backend stores in clear.

The associated data of the seal holds the real bucket name, the folder, the name of the object and its kind (reference, delta or passthrough). The name of the backend is not part of it, so a renamed backend changes nothing. A seal therefore opens only at the location where the proxy wrote it. The facts are about 220 to 280 characters long, and they count against the 2 KB metadata limit of S3, so a client of a bucket that encrypts has less room for its own user metadata.

The proxy keeps the content type and the user metadata of the client in clear, because S3 returns them in its own headers. The seal covers them with the digest, so an edit on the backend is detected.

### Listing tokens (S3 backends)

On an S3 backend, a plain LIST request reads only the backend listing, which holds the stored size and the stored ETag. For an encrypted object, the proxy also keeps a listing token next to the object, under `.dg/facts/`. The token holds the size and the ETag of the object, encrypted with AES-256-GCM-SIV under a listing key of the key in the configuration. Its associated data names the real bucket, the stored key, the stored ETag and the stored size. The proxy lists an encrypted object with the size and the ETag from its token. A token that does not open is ignored, and the proxy then falls back to the metadata of the object. On a backend that holds an encryption key, the proxy ignores the listing facts of the older form (`!!1`), because a writer on the backend can forge them.

SSE-KMS / SSE-S3 objects carry no DG framing. AWS applies its own encryption wrapper, and the proxy passes the bytes through.

## GET behavior

The read path applies one rule to every object in a bucket:

- **An object with sealed metadata.** The proxy finds the key by the fingerprint in the seal, opens the seal at the location of the object, checks the digest of the clear headers, checks the header of the body, and then decrypts and authenticates each frame. Any failure is `500 InternalError`, and the log names the cause.
- **An object without sealed metadata, but with `dg-encrypted`.** The proxy refuses it. The object was written by another release, or its metadata was edited.
- **An object without any marker in a bucket that encrypts.** The proxy refuses it, unless the bucket has `plaintext_reads: serve` (see the next section).
- **An object without any marker in a bucket that stores plaintext.** The proxy serves it as it is. The read path also checks the first bytes of the body: a body that starts with a `DGE` magic and a digit is proxy-encrypted, so its metadata was stripped, and the read fails instead of serving ciphertext as plaintext.
- **A folder marker** (an empty object whose key ends in `/`) always reads, because the proxy never encrypts it.
- `dg-encrypted-native: sse-kms` (or `sse-s3`): no proxy-side decryption; AWS returns plaintext.

A refused or failed read is always `500 InternalError`, never corrupt data and never `404 NoSuchKey`. The proxy never treats a refused delta reference as missing, because a missing reference makes the next upload create a new baseline over it.

The proxy has no reader for the earlier formats `aes-256-gcm-v1` and `aes-256-gcm-chunked-v1` (body magic `DGE1`). An object in such a format fails to read.

### Plaintext objects in a bucket that encrypts

A writer with access to the backend can put a plaintext object into a bucket that encrypts. The proxy cannot tell such an object from the plaintext history of the bucket. By default (`plaintext_reads: refuse`), a read of an object without encryption in a bucket that encrypts therefore fails with `500 InternalError`, and the log says:

> object 'nightly/2026-10-14.dump' in bucket 'db-archive' is not encrypted, but the bucket encrypts: the proxy refuses it, because a writer with access to the backend could have planted it. Set encryption.plaintext_reads: serve while a re-encrypt job converts the bucket's history

`plaintext_reads: serve` makes the proxy serve these objects. Set it in the `encryption` block of the backend or of the bucket when you start to encrypt a bucket that already holds objects. The re-encrypt job reads through the same rule, so it can convert the plaintext objects only under `serve`. Under `refuse`, each plaintext object is a failure of the job. Remove `plaintext_reads: serve` when the job succeeded, because every object that the proxy serves under `serve` is trusted as it is.

The field exists only in `aes256-gcm-proxy` mode. The admin UI sets `plaintext_reads: serve` by itself when it turns on proxy-side AES for a backend or a bucket that did not encrypt before. While the field is `serve`, the backend card and the bucket card show the box **Unencrypted objects are served**, and its button **Refuse unencrypted objects** sets the field back to `refuse`. `GET /_/api/admin/buckets` and `GET /_/api/admin/backends` report the field as `serves_plaintext` ([admin API](admin-api.md#backends)).

```yaml
# validate
storage:
  backends:
    - name: local-disk
      type: filesystem
      path: /var/lib/deltaglider_proxy/data
  buckets:
    db-archive:
      backend: local-disk
      encryption:
        mode: aes256-gcm-proxy
        key: "${env:DB_ARCHIVE_AES_KEY}"
        key_id: db-archive-2026
        plaintext_reads: serve      # remove after the re-encrypt job succeeded
```

#### Starting to encrypt needs `serve` or a confirmation

A config change that makes an existing bucket start to encrypt in `aes256-gcm-proxy` mode, with `plaintext_reads: refuse`, would make every object that the bucket held before unreadable. The proxy therefore refuses such a change, unless the new `encryption` block sets `plaintext_reads: serve`, or the request confirms the subject. To confirm, send the header `X-DGP-Confirm-Plaintext-Refusal: <subject>` (comma-separated for several subjects), or use `config apply --confirm-plaintext-refusal <subject>`. Confirm only when the subject holds no plaintext that anyone needs. The subjects are the same as for a downgrade: the bucket name, `backend:<name>` for every bucket of the backend without its own block, and `route:default`. The refusal names each subject, for example:

> this change makes bucket 'db-archive' encrypt, but the objects stored there before are plaintext, and with `plaintext_reads: refuse` (the default) the proxy refuses to read them. Set encryption.plaintext_reads: serve and run a re-encrypt job (POST /_/api/admin/jobs/reencrypt) to convert them, then set it back. If the subject holds no plaintext that anyone needs, confirm with the header `X-DGP-Confirm-Plaintext-Refusal: db-archive` (`config apply --confirm-plaintext-refusal db-archive`)

A bucket that no backend lists yet needs no confirmation, because it holds no objects: for example a bucket that the **Create bucket** dialog creates with its own key. A key rotation, and a change from one encrypting mode to another, are not affected. Every config write runs this gate, as it runs the downgrade gate, and a proxy that starts from an edited file does not run it.

### Copies outside the proxy

The sealed metadata names the real bucket, the folder and the name of the object. A copy that the backend makes on its own therefore does not open at its new location: an `rclone` copy into another bucket, a restore of a backup under another bucket name, or a key that you move by hand. The proxy refuses such a copy, with a message that the sealed metadata failed authentication. This release has no setting that reads an object at another location. To move encrypted objects, copy them through the proxy (`CopyObject`, replication, the migrate job, or a lifecycle transition): the proxy then writes new sealed metadata for the destination. A backup of the backend restores readable objects only under the same bucket names and keys.

### The `legacy_key` shim

When `legacy_key` is set, the keyring holds it next to `key`. A read picks the key by the fingerprint of the object, so the objects of the old key decrypt with `legacy_key`. `legacy_key_id` is the id under which the proxy shows these objects. The shim does not affect writes. They go through the backend's current mode only (under a native mode, `WriteMode::PassThrough` skips the proxy-AES path entirely). The shim holds exactly one legacy key generation, works under every mode including `none`, and the admin panel shows an info banner while one is active. When you click **Check usage**, the banner counts the objects and delta references that the legacy key still encrypts (`GET /_/api/admin/backends/:name/legacy-key-usage`). The banner does not count by itself, because the count reads the metadata of every object on the backend. Its **Clear legacy key** button is enabled only when a count is complete and zero. The native → proxy-AES direction needs no shim: native objects carry `dg-encrypted-native`, so the proxy decrypt path does not fire.

## Limits

- **Key rotation keeps one previous key.** When an apply changes `key`, sets it to `null`, or moves a backend away from `aes256-gcm-proxy`, and does not set `legacy_key`, the proxy moves the previous key into `legacy_key`, and its effective id into `legacy_key_id`. An apply that sends `legacy_key: null` drops the previous key instead. The shim holds one legacy generation at a time, so the proxy refuses a key change while `legacy_key` holds a different key. It also refuses a key change that keeps the same `key_id`. A Re-encrypt job (`POST /_/api/admin/jobs/reencrypt`, or Jobs → + New job → Re-encrypt buckets…) rewrites objects under the current configuration; it is durable, resumable across restarts, cancellable, and write-gates affected buckets (`503 SlowDown` on writes; reads unaffected).
- **An apply keeps what it does not name.** For `key`, `key_id` and `legacy_key`, the same rule applies to every apply, whether it is a section `PUT` or a full document. When the field is absent, the proxy keeps the current value. When the field is `null`, the proxy clears it. A `key: null` is a key change like any other: the previous key moves into `legacy_key` (or the apply is refused while `legacy_key` holds a different key), so the proxy never drops a key that objects can still need. A backend that is left in `aes256-gcm-proxy` mode with no key is then refused, as described in [A proxy-AES backend with no key](#a-proxy-aes-backend-with-no-key). When the field has a value, that value replaces the current one. An id is kept only together with its own key: when the apply sets a new `key` and no `key_id`, the new key gets the generated id `fp-<hex>`, and the previous id moves to `legacy_key_id` with the previous key.
- **Enabling is not retroactive.** Switching a backend's mode affects new writes only; existing objects keep their stored form until the re-encrypt job rewrites them. In a bucket that encrypts, a plaintext object reads only with `plaintext_reads: serve` (see [Plaintext objects in a bucket that encrypts](#plaintext-objects-in-a-bucket-that-encrypts)).
- **Key loss is data loss** in `aes256-gcm-proxy` mode. The proxy does not escrow keys, and there is no recovery path.
- **Per-bucket native SSE needs an S3 backend.** A bucket `encryption` block takes `aes256-gcm-proxy` or `none` on every backend, and `sse-kms` or `sse-s3` only on an S3 backend. With `config_sync_bucket` set, the encryption witness keeps the instances in agreement. See [Per-bucket encryption](#per-bucket-encryption).
- **Names and some metadata stay readable on the backend.** Under every mode, the backend stores object names, content-type and `x-amz-meta-*` user metadata unencrypted. In `aes256-gcm-proxy` mode, the sealed metadata authenticates the content type and the user metadata, and it hides the hashes and the size of a delta. The stored size of a passthrough object or a reference still gives its exact plaintext size, and the size of a delta stays readable. Under SSE-KMS and SSE-S3, the backend also stores the hashes and the sizes in clear. [About the encryption threat model](../explanation/encryption-threat-model.md) lists what each attacker learns.
- **Copies outside the proxy are unreadable.** An encrypted object that the backend copies to another bucket name or key does not read there (see [Copies outside the proxy](#copies-outside-the-proxy)).
- **Less room for user metadata.** The sealed metadata takes about 220 to 280 bytes of the 2 KB S3 metadata limit.
- **No forward secrecy.** A disclosed proxy-AES key decrypts all past ciphertext written under it.
- **Memory.** Encrypted GET of a passthrough object is streaming: the decoder holds ~130 KiB in flight regardless of object size, and a range GET fetches only the frames that cover the range, in one request. An encrypted PUT in proxy-AES mode of a body that the proxy holds in memory buffers every encrypted frame before handing off to the inner backend: peak write memory ≈ plaintext size + 0.03%, so a 100 MiB `PutObject` peaks around 200 to 300 MiB RSS. A body that the proxy stores from a file (a large upload, or a multipart upload whose parts are relay files) is encrypted into a temporary file in `DGP_SPOOL_DIR` instead. That file counts against `DGP_SPOOL_MAX_BYTES`, and when the budget is in use the upload gets `503 SlowDown`. The proxy rejects passthrough objects above `max_object_size` (default 100 MiB) up front. SSE-KMS / SSE-S3 stream through without this buffering.
- **Latency.** AES-256-GCM throughput is roughly 1 to 3 GB/s per core with AES-NI. A 100 MiB proxy-AES upload adds ~30 to 100 ms of proxy-side crypto work. Native SSE modes move this cost to AWS.
- The pre-v0.9 global `advanced.encryption_key` field no longer exists; encryption is configured per backend.

## Related

- [About encryption at rest](../explanation/encryption-at-rest.md)
- [About the encryption threat model](../explanation/encryption-threat-model.md)
- [How to encrypt data at rest](../how-to/encrypt-data-at-rest.md)
- [How to rotate or change encryption keys](../how-to/rotate-encryption-keys.md)
