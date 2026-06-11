# Encryption reference

Encryption at rest is configured per backend. Each backend in `storage.backends[]` (or the singleton `storage.backend`, via the top-level `storage.backend_encryption`) carries one `encryption` block with exactly one of four modes. The modes are mutually exclusive on a given backend; the `BackendEncryptionConfig` enum enforces this by construction.

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
| `key_id` | `aes256-gcm-proxy` | Optional stable identifier stamped on written objects. Derived from backend name + key when absent. |
| `kms_key_id` | `sse-kms` | KMS key ARN. |
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

### Examples

Proxy-AES on a named S3 backend, key supplied by env var:

```yaml
storage:
  backends:
    - name: hetzner-fsn1
      s3:
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
storage:
  backends:
    - name: aws-dr
      s3:
        region: eu-west-1
      encryption:
        mode: sse-kms
        kms_key_id: arn:aws:kms:eu-west-1:123456789012:key/abcd-ef01
        bucket_key_enabled: true
```

SSE-S3 on an AWS backend:

```yaml
storage:
  backends:
    - name: aws-dr
      s3:
        region: eu-west-1
      encryption:
        mode: sse-s3
```

Keys can also be set in the admin GUI (Admin → Storage → Backends); GUI-generated keys are produced in-browser via `crypto.getRandomValues` and do not round-trip through the server before Apply.

## Key IDs

Every proxy-AES write stamps a `dg-encryption-key-id` metadata field on the object. The id is either the explicit `key_id` from YAML or derived as `SHA-256(backend_name ‖ 0x00 ‖ key)[..16]`. The backend name is part of the derivation: two backends with identical key material but different names produce different ids, so objects are not portable across backends by default. Pinning the same explicit `key_id` with identical key bytes on two backends is the documented portability escape hatch.

On read, the object's stamped id is compared against the backend's configured `key_id`, then against `legacy_key_id`. A mismatch produces a specific error rather than an opaque GCM authentication failure:

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

Large passthrough uploads stream end-to-end without buffering the whole object. The codec slices plaintext into 64-KiB windows and produces this layout:

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

SSE-KMS / SSE-S3 objects carry no DG wire framing — AWS applies its own encryption wrapper and the proxy passes the bytes through.

## GET behavior

The reader dispatches on the object's metadata markers:

- `dg-encrypted: aes-256-gcm-v1` — decrypt single-shot with the backend's proxy-AES key.
- `dg-encrypted: aes-256-gcm-chunked-v1` — stream-decrypt chunk by chunk. Range requests compute the first and last chunk in O(1) (every non-final frame is exactly 65556 wire bytes), fetch only those chunks, decrypt, and trim to the client's `[start, end]`.
- `dg-encrypted-native: sse-kms` (or `sse-s3`) — no proxy-side decryption; AWS returns plaintext.
- Absent marker — the object is served as-is. The read path additionally sniffs the body's first 4 bytes: if the `DGE1` magic is present but the metadata marker is missing (for example, xattrs stripped by a backup/restore round-trip), the read errors rather than serving ciphertext as plaintext.

Truncation, reordering, and tampering of proxy-AES objects fail at GCM verification; the client receives `500 InternalError`, never corrupt data.

### The `legacy_key` shim

When `legacy_key` / `legacy_key_id` are set, reads check the object's `dg-encryption-key-id` against `key_id` first, then against `legacy_key_id`; objects matching the legacy slot decrypt with `legacy_key`. Writes are unaffected — they go through the backend's current mode only (under a native mode, the proxy-AES path is skipped entirely via `WriteMode::PassThrough`). The shim holds exactly one legacy key generation, works under every mode including `none`, and the admin panel shows an info banner while one is active. The native → proxy-AES direction needs no shim: native objects carry `dg-encrypted-native`, so the proxy decrypt path does not fire.

## Limits

- **No in-place key rotation.** Changing `key` on a backend makes objects written under the old key unreadable unless the old key is configured as `legacy_key`. The shim holds one legacy generation at a time. A Re-encrypt job (`POST /_/api/admin/jobs/reencrypt`, or Jobs → + New job → Re-encrypt buckets…) rewrites objects under the current configuration; it is durable, resumable across restarts, cancellable, and write-gates affected buckets (`503 SlowDown` on writes; reads unaffected).
- **Enabling is not retroactive.** Switching a backend's mode affects new writes only; existing objects keep their stored form and markers until rewritten.
- **Key loss is data loss** in `aes256-gcm-proxy` mode. Keys are not escrowed; there is no recovery path.
- **No per-bucket encryption.** Encryption is backend-scoped; a bucket inherits the encryption of the backend it routes to.
- **Metadata is plaintext** under every mode, including SSE-KMS: object names, sizes, content-type, and `x-amz-meta-*` user metadata are stored unencrypted.
- **No forward secrecy.** A disclosed proxy-AES key decrypts all past ciphertext written under it.
- **Memory.** Encrypted GET is streaming: the decoder holds ~130 KiB in flight regardless of object size, and range GETs fetch only the target chunks plus a 16-byte header probe. Encrypted PUT in proxy-AES mode buffers every encrypted frame before handing off to the inner backend: peak write memory ≈ plaintext size + 0.03%; combined with multipart part buffering, a 100 MiB encrypted upload peaks around 200–300 MiB RSS. Passthrough objects above `max_object_size` (default 100 MiB) are rejected up front. SSE-KMS / SSE-S3 stream through without this buffering.
- **Latency.** AES-256-GCM throughput is roughly 1–3 GB/s per core with AES-NI; a 100 MiB proxy-AES upload adds ~30–100 ms of proxy-side crypto work. Native SSE modes move this cost to AWS.
- The pre-v0.9 global `advanced.encryption_key` field no longer exists; encryption is configured per backend.

## Related

- [About encryption at rest](../explanation/encryption-at-rest.md)
- [How to encrypt data at rest](../how-to/encrypt-data-at-rest.md)
- [How to rotate or change encryption keys](../how-to/rotate-encryption-keys.md)
