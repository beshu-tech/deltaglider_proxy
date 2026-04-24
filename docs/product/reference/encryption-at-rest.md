# Encryption at rest

*Per-backend encryption with four modes: none, proxy-side AES-256-GCM, SSE-KMS, SSE-S3.*

This page is operational-depth reference for the encryption-at-rest feature: which mode to pick per backend, what gets encrypted, how reads detect the wrong key, and what the operational boundaries are.

## What this protects

If someone walks off with the disks — or an S3 backend is breached at the storage layer — object bodies are ciphertext. Without the key (or KMS access, for SSE-KMS), they're unrecoverable.

Encryption is configured **per backend**, so a public-CDN bucket can live alongside a compliance-scoped one without paying the same CPU tax or sharing blast radius.

**What's encrypted:**

| Layer | Encrypted? |
|---|---|
| Passthrough object bodies | Depends on backend mode (see below) |
| Delta bodies + reference bodies | Same — encrypted by whatever the backend mode dictates |
| Object names / sizes / user metadata | No — lives in the backend's native metadata; plaintext even under SSE-KMS |
| Transport (network) | No — terminate TLS at a reverse proxy (see [security checklist](../20-production-security-checklist.md)) |

## The four modes

Each backend carries one of four encryption modes. They're mutually exclusive on a given backend — the `BackendEncryptionConfig` enum enforces this by construction.

| Mode | Who encrypts? | When to use |
|---|---|---|
| `none` | nobody | Plaintext-acceptable buckets (public CDN, release artifacts). |
| `aes256-gcm-proxy` | proxy, before the backend sees the bytes | Filesystem backends, or S3 backends where you don't trust the provider to encrypt. |
| `sse-kms` | AWS, via KMS | S3 backends with an AWS-KMS-managed key. Key lifecycle delegated to AWS. |
| `sse-s3` | AWS, using AWS-managed AES256 | S3 backends where you want at-rest encryption without the KMS cost/complexity. |

### Proxy-AES vs native SSE

Proxy-AES and the two native modes encrypt at different layers:

- **Proxy-AES (`aes256-gcm-proxy`)** — the proxy's `EncryptingBackend` wrapper encrypts bytes before they reach the backend. Works with any backend type (filesystem or S3). Key material lives in the proxy's configuration (YAML, env var, or generated via the admin GUI). Delta compression happens **before** encryption so compression gains are preserved.

- **SSE-KMS / SSE-S3** — the proxy delegates encryption to AWS. Every PutObject carries `ServerSideEncryption` + (for SSE-KMS) `SSEKMSKeyId` headers; AWS encrypts the object on write and transparently decrypts on read for callers with KMS permission. The proxy never handles key material. These modes only apply to S3 backends (`Config::check` rejects them on filesystem).

Both styles get the same `dg-encrypted` / `dg-encrypted-native` marker on user metadata so reads dispatch correctly.

## Configuration

Each named backend declares its own `encryption` block. Singleton-backend deployments use the top-level `backend_encryption`.

```yaml
storage:
  backends:
    - name: eu-archive
      s3: { endpoint: https://s3.eu-central-1.amazonaws.com, region: eu-central-1, ... }
      encryption:
        mode: aes256-gcm-proxy
        key:  "${DGP_BACKEND_EU_ARCHIVE_ENCRYPTION_KEY}"
        key_id: eu-2026-04                  # optional — derived from name + key when absent
    - name: us-public
      s3: { endpoint: ..., region: us-east-1, ... }
      # encryption omitted → mode: none
    - name: us-kms
      s3: { ... }
      encryption:
        mode: sse-kms
        kms_key_id: arn:aws:kms:us-east-1:123456789012:key/abc-def
        bucket_key_enabled: true            # reduces KMS cost on bursty traffic
    - name: us-s3
      s3: { ... }
      encryption:
        mode: sse-s3
```

**Via env var** (recommended — keeps the key off-disk):

```bash
DGP_BACKEND_EU_ARCHIVE_ENCRYPTION_KEY=$(openssl rand -hex 32)
DGP_BACKEND_US_KMS_SSE_KMS_KEY_ID=arn:aws:kms:...
# Singleton-backend shortcut:
DGP_ENCRYPTION_KEY=$(openssl rand -hex 32)
```

**Via the admin GUI:** Admin → Storage → Backends. Each backend card has its own encryption subsection: mode dropdown, key-generation widget (proxy-AES), KMS ARN input (SSE-KMS). Keys are generated in-browser via `crypto.getRandomValues` and never round-trip through the server pre-Apply.

> [!WARNING] If you lose a proxy-AES key, encrypted objects on that backend are unrecoverable.
> DeltaGlider does not escrow keys. Store each key somewhere outside the proxy host — a secrets manager, an operator password vault, a sealed envelope. The admin panel displays a red banner on every key-touching action as a reminder. SSE-KMS / SSE-S3 keys are AWS-managed; their lifecycle is your IAM and KMS story.

## Key IDs and mismatch detection

Every proxy-AES write stamps a `dg-encryption-key-id` metadata field on the object. Reads consult this field against the backend's configured `key_id` and emit a **specific** error on mismatch — not the opaque GCM auth failure that would otherwise surface.

The id is either explicit in YAML (`encryption.key_id: "..."`) or derived: `SHA-256(backend_name ‖ 0x00 ‖ key)[..16]`. Name-mixing is load-bearing — two backends with the same key material but different names produce different ids, so objects are NOT accidentally portable across backends.

Mismatch error text:

> object was encrypted with key id 'obj-foo', but this backend is configured with key id 'backend-bar' (no legacy-shim match either). This usually means: (a) the key was rotated without `legacy_key` set — restore the old key alongside the new one; (b) this bucket is routed to the wrong backend; (c) two backends share physical storage with different keys.

Legacy objects (written before `dg-encryption-key-id` landed) have no stamp — they decrypt as long as the key material still matches.

## Mode transitions — the decrypt-only shim

Flipping a backend from `aes256-gcm-proxy` to `sse-kms` (or `sse-s3`) while historical proxy-encrypted objects still live on it would otherwise strand them. The shim lets the operator migrate lazily:

```yaml
# After the transition
encryption:
  mode: sse-kms
  kms_key_id: arn:aws:kms:...
  legacy_key:    "${DGP_OLD_PROXY_KEY}"   # the pre-transition proxy-AES key
  legacy_key_id: "eu-2026-04"             # id that was stamped on old objects
```

In this state:
- **Reads** check the object's `dg-encryption-key-id` against `key_id` first, then against `legacy_key_id`. Old proxy-stamped objects match the legacy slot and decrypt with `legacy_key`.
- **Writes** skip the proxy-AES path entirely (`WriteMode::PassThrough`) and go through SSE-KMS on the inner S3Backend.

The admin panel shows an info banner while a shim is active. Clear `legacy_key` + `legacy_key_id` once all historical objects have been re-written or deleted.

The reverse direction (native → proxy-AES) needs no shim: native-mode objects carry `dg-encrypted-native` (not `dg-encrypted`), so the proxy wrapper's decrypt path doesn't fire — AWS returned plaintext on the way out.

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

Deltas and references stay single-shot (`aes-256-gcm-v1`). They're bounded by `max_object_size` (100 MiB) so chunking would be wasted overhead.

SSE-KMS / SSE-S3 objects carry no DG wire framing — AWS applies its own encryption wrapper and the object body is plaintext to the proxy's serialisation path (it just passes the bytes through).

## What happens on GET

The reader dispatches on the object's metadata markers:

- `dg-encrypted: aes-256-gcm-v1` → decrypt single-shot with the backend's proxy-AES key.
- `dg-encrypted: aes-256-gcm-chunked-v1` → stream-decrypt chunk by chunk. Range requests compute the first/last chunk in O(1) math (every non-final frame is exactly 65556 wire bytes), fetch only those chunks, decrypt, and trim to the client's `[start, end]`.
- `dg-encrypted-native: sse-kms` (or `sse-s3`) → no proxy-side decryption; AWS returned plaintext.
- Absent marker → object is plaintext. The read path sniffs the body's first 4 bytes for `DGE1` — if the magic is there but the metadata marker isn't (e.g. xattrs were stripped by a backup/restore round-trip), the read errors rather than serving ciphertext as plaintext.

Truncation, reorder, and tamper attempts on proxy-AES objects all fail hard at GCM verification. The client gets a `500 InternalError` — never corrupt data.

## Operational caveats

### Rotation on a single backend is not supported

Changing the `key` on a backend makes objects written under the old key **unreadable** until the old key is restored OR the shim is configured.

The shim (§Mode transitions) covers the proxy→native transition cleanly. Within the same mode — rotating a proxy-AES key to a NEW proxy-AES key — DeltaGlider does not ship a background re-encryption worker. Rotation either:
- restore the old key as `legacy_key` + keep the new one as `key` (read shim works, but only for two generations at a time), or
- create a new backend with the new key, copy objects through the proxy, retire the old backend.

### Enabling is not retroactive

Turning on encryption on a backend does not encrypt existing objects. Only new writes go through the encrypting wrapper.

### The global `advanced.encryption_key` is gone

Older guides may reference `advanced.encryption_key`. That field was removed in v0.9 when encryption moved per-backend. Use `storage.backend_encryption` (singleton) or `storage.backends[*].encryption` (list).

### Per-bucket encryption (not supported)

Encryption is a backend-scoped choice. A bucket inherits the encryption of the backend it routes to. Per-bucket encryption within a shared backend would require per-object routing and is out of scope.

Operators who need bucket-level isolation should create additional backends.

### Metadata is plaintext

Under any mode — including SSE-KMS — object names, sizes, content-type, and user metadata (`x-amz-meta-*` headers) are plaintext. This is an AWS constraint for SSE, and mirrored on the filesystem backend (xattrs are unencrypted). DeltaGlider does not treat metadata as secret.

### Peak memory on proxy-AES writes

Encrypted GET (downloads) is streaming: the decoder holds ~130 KiB in-flight regardless of object size. Range GETs fetch only the target chunks plus a 16-byte header probe — reading the last 100 bytes of a large object is O(1) network traffic.

Encrypted PUT (uploads) in proxy-AES mode buffers every encrypted frame in memory before handing to the inner backend. Peak write memory ≈ ciphertext size ≈ plaintext size + 0.03%. Combined with the multipart handler buffering all parts, a 100 MiB encrypted upload peaks around 200–300 MiB of RSS.

The proxy rejects passthrough objects larger than `max_object_size` (default 100 MiB) up front. Raising that ceiling with proxy-AES enabled means budgeting RAM to match.

SSE-KMS / SSE-S3 don't have this overhead — the proxy streams bytes through without buffering.

### Xattr-strip defense

Backup/restore tools that preserve file contents but drop extended attributes (older `rsync`, some S3 sync scripts) would leave proxy-AES ciphertext on disk with no `dg-encrypted` marker. The read path's DGE1-magic sniffer catches this and errors rather than serving ciphertext as plaintext.

## Related

- [Security checklist — Step 6](../20-production-security-checklist.md#step-6-enable-encryption-at-rest-optional) — the operational walkthrough.
- [How delta works](how-delta-works.md) — why compression happens before encryption.
- [Configuration reference](configuration.md) — the `storage.backends[*].encryption` schema.
