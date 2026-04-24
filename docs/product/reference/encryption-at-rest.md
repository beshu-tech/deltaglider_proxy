# Encryption at rest

*Per-backend encryption with four modes: none, proxy-side AES-256-GCM, SSE-KMS, SSE-S3.*

This page is operational-depth reference for the encryption-at-rest feature: which mode to pick per backend, what gets encrypted, how reads detect the wrong key, what the operational boundaries are, and how to recover from common mistakes.

## TL;DR

- Encryption is **per-backend**, not global. Each backend in `storage.backends[]` (or the singleton `storage.backend`) carries its own `encryption` block.
- Four mutually-exclusive modes: `none`, `aes256-gcm-proxy`, `sse-kms`, `sse-s3`. Pick one per backend.
- **Use `aes256-gcm-proxy`** when you want the proxy to hold the key (filesystem backends, or S3 backends where you don't trust the provider). Compression gains are preserved — xdelta3 runs before AES-GCM.
- **Use `sse-kms`** when you want AWS to manage the key via KMS. Your KMS IAM policy is the security boundary; the proxy never sees key material.
- **Use `sse-s3`** when you want AWS-managed at-rest encryption with no KMS cost/complexity.
- **Use `none`** for public-CDN buckets or workloads where plaintext on disk is acceptable.
- Key loss in `aes256-gcm-proxy` mode = data loss. There is no recovery. Back the key up off-box before enabling.
- Rotation within a single mode is not automated — see §[Rotation recipes](#rotation-recipes).

## What this protects

If someone walks off with the disks — or an S3 backend is breached at the storage layer — object bodies are ciphertext. Without the key (or KMS access, for SSE-KMS), they're unrecoverable.

Encryption is configured **per backend**, so a public-CDN bucket can live alongside a compliance-scoped one without paying the same CPU tax or sharing blast radius.

### Threat model — what encryption-at-rest does and doesn't defend

| Threat | `none` | `aes256-gcm-proxy` | `sse-kms` | `sse-s3` |
|---|---|---|---|---|
| Disk theft / backend breach — attacker has ciphertext, no key/KMS access | vulnerable | defended | defended | defended |
| Compromised proxy host (attacker reads proxy memory or YAML) | same | **vulnerable** (key is in proxy) | defended (key in KMS) | defended (key in AWS) |
| Compromised AWS IAM credentials with full S3 read access | vulnerable | defended (AWS can't decrypt) | defended (needs KMS too) | **vulnerable** (AWS decrypts for IAM caller) |
| Compromised KMS principal | N/A | N/A | **vulnerable** | N/A |
| Plaintext traffic on the wire (no TLS) | vulnerable | vulnerable | vulnerable | vulnerable |
| Malicious admin with proxy access rotating the key to lock out data | same | **vulnerable** (rotation ≠ deletion but old key access is gone) | depends on KMS policy | N/A |

TLS termination is a separate concern — see the [security checklist](../20-production-security-checklist.md).

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

### Decision tree: which mode for which backend?

```
Is the backend an S3 bucket?
├── No (filesystem)
│   └── Use `aes256-gcm-proxy` (if encryption is needed) or `none`.
│       Native SSE modes are S3-only — Config::check rejects them
│       on filesystem backends.
└── Yes (S3)
    ├── Is this bucket holding public / CDN-ready artifacts?
    │   └── Use `none`. Encryption is pure overhead here.
    ├── Do you already pay for AWS KMS or want per-key audit logs?
    │   └── Use `sse-kms`. KMS provides separate IAM, key rotation,
    │       grant-based access, and CloudTrail integration for
    │       every decrypt operation.
    ├── Do you want at-rest encryption but no KMS cost?
    │   └── Use `sse-s3`. AWS handles keys entirely. No per-request
    │       KMS API cost; no CloudTrail per decrypt.
    └── Do you distrust the S3 provider (third-party, compliance)?
        └── Use `aes256-gcm-proxy`. The provider never sees plaintext.
            Cost: proxy CPU + memory for the encrypt/decrypt path.
```

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

### Worked examples

#### Single filesystem backend, proxy-AES

The simplest deployment: one filesystem backend, proxy-side encryption, key from env var.

```yaml
# deltaglider_proxy.yaml
storage:
  backend:
    type: filesystem
    path: /var/lib/deltaglider_proxy/data
  backend_encryption:
    mode: aes256-gcm-proxy
    key: "${DGP_ENCRYPTION_KEY}"
    key_id: prod-2026-04   # optional but recommended
```

```bash
# /etc/deltaglider_proxy.env (or systemd Environment=)
DGP_ENCRYPTION_KEY=$(openssl rand -hex 32)
```

After first start, store `DGP_ENCRYPTION_KEY` in your secrets manager. If you lose it AND the env file, every encrypted object on that filesystem is unrecoverable.

#### Multi-region: EU proxy-AES + US SSE-KMS + public CDN plaintext

Three backends, three encryption postures:

```yaml
storage:
  backends:
    # 1. EU archive — we don't trust the third-party provider,
    #    so proxy-AES. Our key, our problem.
    - name: eu-archive
      s3:
        endpoint: https://eu-archive.provider.example
        region: eu-central-1
      access_key_id: "${DGP_BACKEND_EU_ARCHIVE_AWS_KEY}"
      secret_access_key: "${DGP_BACKEND_EU_ARCHIVE_AWS_SECRET}"
      encryption:
        mode: aes256-gcm-proxy
        key: "${DGP_BACKEND_EU_ARCHIVE_ENCRYPTION_KEY}"
        key_id: eu-archive-2026-04

    # 2. US primary — AWS S3 with KMS. AWS handles key lifecycle.
    #    We still rely on DeltaGlider for delta compression + the
    #    transparent S3 API, but the crypto is AWS's job.
    - name: us-primary
      s3:
        region: us-east-1
      encryption:
        mode: sse-kms
        kms_key_id: arn:aws:kms:us-east-1:123456789012:key/abcd
        bucket_key_enabled: true

    # 3. Public CDN — releases are meant to be world-readable.
    #    Encryption is pure overhead here.
    - name: public-releases
      s3:
        region: us-east-1
      # encryption omitted → mode: none

  default_backend: us-primary

  buckets:
    archive:
      backend: eu-archive
    releases:
      backend: public-releases
      public_prefixes: ["builds/"]
```

#### Shared infra with KMS, different keys per team

Two KMS keys, two backends, same AWS account but different IAM grantees:

```yaml
storage:
  backends:
    - name: team-alpha
      s3: { region: us-east-1 }
      encryption:
        mode: sse-kms
        kms_key_id: arn:aws:kms:us-east-1:1:key/alpha-key
    - name: team-beta
      s3: { region: us-east-1 }
      encryption:
        mode: sse-kms
        kms_key_id: arn:aws:kms:us-east-1:1:key/beta-key
```

Team Alpha's objects become unreadable to anyone without `kms:Decrypt` on `alpha-key`, even if they have S3 GetObject on the bucket. Same for Beta. The proxy itself only needs GetObject/PutObject and `kms:GenerateDataKey`/`kms:Decrypt` on both keys.

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

## Rotation recipes

Automated rotation isn't part of this release. All four recipes below are manual; pick the one that matches your constraint.

### (A) Rotate a proxy-AES key with minimum downtime (shim-assisted)

Use when: you want to rotate the key, can tolerate a period during which the old and new keys coexist in runtime, and want every historical read to keep working throughout.

```yaml
# Before:
encryption:
  mode: aes256-gcm-proxy
  key: "${DGP_OLD_KEY}"
  key_id: prod-2025-10
```

Step 1 — Generate the new key off-box. `openssl rand -hex 32` into your secrets manager as `DGP_NEW_KEY`.

Step 2 — Configure the shim. Move the old key into `legacy_key` / `legacy_key_id`; put the new key into `key`.

```yaml
encryption:
  mode: aes256-gcm-proxy
  key: "${DGP_NEW_KEY}"
  key_id: prod-2026-04
  legacy_key: "${DGP_OLD_KEY}"
  legacy_key_id: prod-2025-10
```

Hot-reload the config (PATCH or restart).

Step 3 — All new writes encrypt with the new key. All reads try the new key first, fall back to the legacy shim. Both keys must remain configured until every historical object has been re-written or deleted.

Step 4 — When you're confident the old key is gone from disk (a usage scan across all buckets routed to this backend would prove it), remove `legacy_key` + `legacy_key_id` from the config.

**Caveat:** the shim supports exactly ONE legacy generation. Rotating again while the shim is still live means the "old" key in that shim becomes unreadable — rotate to the final destination, not through intermediaries.

### (B) Rotate a proxy-AES key with a data migration (zero-shim)

Use when: you don't want the old key to stay in runtime at all, and you can tolerate migration time up front.

Step 1 — Create a NEW backend with the new key. Route no buckets to it yet.

```yaml
storage:
  backends:
    - name: old-encrypted
      # existing backend, old key
      encryption: { mode: aes256-gcm-proxy, key: "${DGP_OLD_KEY}", key_id: 2025-10 }
    - name: new-encrypted
      # new backend, new key — same underlying storage or different
      encryption: { mode: aes256-gcm-proxy, key: "${DGP_NEW_KEY}", key_id: 2026-04 }
```

Step 2 — Copy every object from the old-backend buckets to the new-backend buckets via the proxy. The proxy decrypts with `DGP_OLD_KEY` on the way out and re-encrypts with `DGP_NEW_KEY` on the way in.

```bash
aws s3 sync --endpoint http://proxy:9000 \
    s3://archive-old/ s3://archive-new/
```

Step 3 — Re-route the bucket alias to the new backend. Delete the old backend. `DGP_OLD_KEY` can now be forgotten.

### (C) Migrate from proxy-AES to SSE-KMS on an existing backend

Use when: you want AWS to take over encryption but historical objects are still encrypted with your proxy key.

Same shape as recipe (A), just with a mode change:

```yaml
# Before:
encryption:
  mode: aes256-gcm-proxy
  key: "${DGP_OLD_PROXY_KEY}"
  key_id: prod-2025-10
```

```yaml
# After:
encryption:
  mode: sse-kms
  kms_key_id: arn:aws:kms:us-east-1:1:key/new-kms
  legacy_key: "${DGP_OLD_PROXY_KEY}"
  legacy_key_id: prod-2025-10
```

Reads of proxy-stamped historical objects decrypt via the legacy shim; new writes go through SSE-KMS. Clear the legacy fields once all proxy-encrypted objects are gone.

### (D) Disable encryption on a backend

Use when: the bucket's contents should become plaintext going forward.

```yaml
encryption:
  mode: none
```

New writes are plaintext. Historical encrypted objects on that backend become unreadable (no key to decrypt them). If you might still need to read them, keep the key as `legacy_key` under `mode: none`:

```yaml
encryption:
  mode: none
  legacy_key: "${DGP_OLD_KEY}"
  legacy_key_id: prod-2025-10
```

This is a valid shape — the legacy shim works on any mode.

## Troubleshooting

### "object is encrypted but no key is configured"

**Symptom:** GET returns 500; server logs contain the literal error text.

**Cause:** The object's metadata carries `dg-encrypted` (it was encrypted) but the backend's current config has no key (mode is `none`, or proxy-AES with no `key` loaded).

**Fix:** restore the key — either in YAML, via env var, or through the admin GUI. If the key is genuinely lost, the object is unrecoverable.

### "object was encrypted with key id 'X', but this backend is configured with key id 'Y'"

**Symptom:** GET returns 500; error text cites both ids.

**Cause:** rotation without a shim, or a bucket routed to the wrong backend, or two backends accidentally pointing at the same physical storage with different keys.

**Fix:** most commonly, restore the old key as `legacy_key`/`legacy_key_id` (recipe A above). If the mismatch is a routing error, fix `storage.buckets[*].backend` in YAML. If two backends share storage, that's a config bug — pick one.

### "object body begins with chunked-encryption magic but metadata has no dg-encrypted marker"

**Symptom:** GET returns 500; error mentions `xattrs` and backup/restore.

**Cause:** the object on disk starts with `DGE1` (proxy-AES chunked wire format) but the metadata marker that tells us so is missing. Most common after a backup/restore round-trip that preserved file contents but dropped extended attributes.

**Fix:** restore the xattrs. On filesystem backends, DeltaGlider stores per-object metadata in the `user.dg.metadata` xattr. A backup utility that doesn't preserve xattrs (older `rsync` without `-X`, some S3 sync tools that don't understand S3 user metadata) will strip them. Re-run the backup with xattr support, or rebuild the metadata from a known-good source.

### "backend 'X' declares key_id='Y' but a prior backend uses the SAME key_id with DIFFERENT key bytes"

**Symptom:** Server refuses to start; error at engine construction.

**Cause:** two backends in YAML pinned the same `key_id` explicitly but have different `key` values. This is almost certainly a copy-paste mistake — same id + different bytes would mean the read-side mismatch check fires on EVERY cross-backend read.

**Fix:** make the `key_id` values distinct, OR make the `key` bytes identical (the documented "portability" escape hatch for operators who intentionally want the same key across two backends).

### "bucket 'X' routes to unknown backend 'Y' — route will be ignored"

**Symptom:** Startup warning; subsequent requests to bucket X land on the default backend instead.

**Cause:** `storage.buckets[X].backend` references a name that's not in the `backends[]` list (e.g. a backend was renamed or removed).

**Fix:** update the routing or restore the backend. Note that objects already written to the OLD backend stay there — the "route will be ignored" only affects future requests.

### "backend 'X' has encryption mode aes256-gcm-proxy but no key is configured"

**Symptom:** Startup warning; writes to this backend go to disk as plaintext despite `mode: aes256-gcm-proxy`.

**Cause:** YAML declares proxy-AES mode but neither `key` in YAML nor `DGP_[_BACKEND_<NAME>]_ENCRYPTION_KEY` in the environment. The mode declaration wins as far as `Config::check` is concerned, but there's nothing to encrypt with.

**Fix:** set the env var, OR put the hex key into YAML (treated as an infra secret — stripped by canonical exports so it doesn't leak via `/config/export`).

### Reads succeed but return garbage

**Symptom:** GET returns 200 with an object that looks like random bytes; no error.

**Cause:** you've disabled the proxy wrapper somehow AND the object on disk is still ciphertext. This shouldn't happen in v0.9 — every backend is always wrapped, and a missing key produces an explicit error. If you see it, it's a bug; file a report with the config + the first 16 bytes of the object body.

## Interoperability

### Python DeltaGlider CLI

The [original Python CLI](https://github.com/beshu-tech/deltaglider) produces the same delta format as DeltaGlider Proxy but does NOT encrypt. If you upload via the Python CLI to a bucket routed to an encrypted backend, the object arrives PLAINTEXT — it bypasses the proxy. Reads through the proxy fire the xattr-strip defense (no `dg-encrypted` marker on an object that happens to start with `DGE1` is unlikely for Python-CLI-produced deltas, but possible).

Policy: standardize on the proxy as the write path for encrypted backends. Configure the CLI's endpoint to point at the proxy, not at raw S3.

### Raw AWS CLI on an SSE-KMS backend

SSE-KMS objects are readable by any AWS IAM principal with `s3:GetObject` + `kms:Decrypt` on the key. Operators doing raw `aws s3 cp` on the backing bucket will see plaintext — AWS decrypts on the wire for authorized callers.

This is intended: SSE-KMS is about "encrypted on disk at AWS" and "decryption gated by KMS IAM", not about hiding the data from authenticated callers. If you want the plaintext to be unreachable even to AWS-authenticated callers, use `aes256-gcm-proxy` instead.

### Raw filesystem access on a proxy-AES backend

Opening the underlying filesystem directly with no proxy in the loop: you'll see a `DGE1` magic header, 12 bytes of IV, then a sequence of length-prefixed ciphertext chunks. No key, no plaintext. Only the proxy can decrypt.

## Security properties — what AEAD gives you

- **Confidentiality** — AES-256 with a 256-bit key.
- **Integrity + authenticity** — GCM tag per chunk binds the ciphertext to the key + the AAD (which includes the chunk index and the is-final flag). Any bit flip in any chunk is detected.
- **Anti-reorder** — AAD binds the chunk index, so swapping two chunks on disk makes both fail auth.
- **Anti-truncate** — AAD binds the `is_final` flag on the last chunk. Truncating the file (whole-tail-chunk deletion) also fails auth because the previous-last chunk's AAD had `is_final=false`, and the decoder expects `is_final=true` at the end.
- **Per-object nonce isolation** — each object has its own random 12-byte `base_iv`; per-chunk nonces derive from it via XOR with the chunk index. Collision-free up to 2^32 chunks per object (256 TiB at 64-KiB chunks).

What AEAD does NOT give you:
- **Semantic privacy of metadata.** Object size + name + content-type + user metadata are plaintext. An attacker counting ciphertext lengths learns approximate plaintext lengths.
- **Forward secrecy.** If the key is ever disclosed, every past ciphertext becomes readable.
- **Deniability.** The `DGE1` magic is a dead giveaway that the bytes are proxy-encrypted.

## FAQ

### Does enabling encryption break existing reads?

No. Reads of unencrypted objects continue to work — the wrapper dispatches on the `dg-encrypted` metadata marker, and absent marker means "serve as-is." Only NEW writes go through the encrypt path.

### Can I use different keys for different buckets on the same backend?

No. Encryption is backend-scoped; every bucket routed to a backend uses that backend's key. If you need bucket-level isolation, split into multiple backends — buckets route to backends via `storage.buckets[name].backend`.

### Does compression still work with encryption?

Yes. xdelta3 runs FIRST on delta-eligible files (archives, db dumps, versioned binaries), then the encrypting wrapper encrypts the delta bytes. Compression ratios are preserved exactly; the ciphertext is just the encrypted wrapping of what the delta codec already produced.

### How much does proxy-AES cost in CPU?

Rough numbers on a modern server CPU with AES-NI: 1–3 GB/s per core for AES-256-GCM. A 100 MiB upload adds ~30–100 ms of proxy-side crypto work on top of whatever the backend takes. SSE-KMS / SSE-S3 move this cost to AWS.

### Can I enable encryption on a running backend that already has unencrypted objects?

Yes. Flip the mode to `aes256-gcm-proxy` (or native). Existing objects stay plaintext (no marker, no decrypt attempt). New writes encrypt. If you later want to encrypt the historical objects too, copy them through the proxy to themselves — that's a re-write through the encrypt path.

### What if I lose ONLY the `legacy_key` after clearing it?

Nothing. Once `legacy_key` is cleared from the config, reads of objects stamped with the legacy id start failing — but this is the SAME state as "legacy_key was never set." If you still have the key somewhere, restore it to resurrect reads. If you don't, those objects are unrecoverable, same as any other key-loss case.

### Is there a background worker that re-encrypts old objects after rotation?

Not in this release. The `legacy_key` shim keeps the old key accessible for reads while new writes use the new key; operators re-encrypt lazily (by re-writing objects) or explicitly (rotation recipe B — copy-through-migration). A true background worker is a possible future add, not a current feature.

### Why are metadata (`x-amz-meta-*`) headers plaintext?

Under SSE-KMS / SSE-S3, AWS encrypts the object body but not the headers — this is an AWS-imposed constraint. Under proxy-AES, we follow the same policy for consistency and because the DG metadata is needed to DETECT whether an object is encrypted at all (chicken-and-egg if the marker itself were encrypted).

If metadata confidentiality matters for your threat model, put the secret in the object body, not in `x-amz-meta-*` headers.

### How do I audit who's decrypting my SSE-KMS objects?

Turn on CloudTrail for the KMS key. Every `Decrypt` / `GenerateDataKey` call logs the principal, the IP, and the timestamp. Proxy-AES has no equivalent — the proxy's own access logs show who GET'd objects, but there's no per-decrypt event because the key never moves.

## Related

- [Security checklist — Step 6](../20-production-security-checklist.md#step-6-enable-encryption-at-rest-optional) — the operational walkthrough.
- [How delta works](how-delta-works.md) — why compression happens before encryption.
- [Configuration reference](configuration.md) — the `storage.backends[*].encryption` schema.
- [Upgrade guide](../21-upgrade-guide.md) — migrating from pre-v0.9 global `advanced.encryption_key`.
- [FAQ](../42-faq.md) — frequent encryption questions not covered above.
- [Troubleshooting](../41-troubleshooting.md) — encryption-specific symptoms and fixes.

- [Security checklist — Step 6](../20-production-security-checklist.md#step-6-enable-encryption-at-rest-optional) — the operational walkthrough.
- [How delta works](how-delta-works.md) — why compression happens before encryption.
- [Configuration reference](configuration.md) — the `storage.backends[*].encryption` schema.
