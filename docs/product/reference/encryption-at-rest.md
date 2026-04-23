# Encryption at rest

*AES-256-GCM for stored objects, with chunked streaming for large uploads.*

This page is operational-depth reference for the encryption-at-rest feature: what gets encrypted, what the on-disk layout looks like, and what the operational boundaries are.

## What this protects

If someone walks off with the disks — or your S3 backend is breached at the storage layer — object bodies are AES-256-GCM ciphertext. Without the 32-byte key, they're unrecoverable.

**What's encrypted:**

| Layer | Encrypted? |
|---|---|
| Passthrough object bodies | Yes — chunked AES-256-GCM (`aes-256-gcm-chunked-v1`) |
| Delta bodies + reference bodies | Yes — single-shot AES-256-GCM (`aes-256-gcm-v1`) |
| Object names / sizes / user metadata | No — lives in the backend's native metadata |
| Transport (network) | No — terminate TLS at a reverse proxy (see [security checklist](../20-production-security-checklist.md)) |

## Order of operations

Delta compression happens **before** encryption. The engine produces the xdelta3 delta against the reference baseline first, then the encrypting wrapper encrypts the delta bytes. Compression gains are preserved — encrypted random-looking output would have no compression headroom.

```
client bytes ──xdelta3──→ delta ──AES-256-GCM──→ on-disk
             (compression)       (confidentiality)
```

## Enabling

**Via env var** (recommended — keeps the key out of the YAML artifact):

```bash
DGP_ENCRYPTION_KEY=$(openssl rand -hex 32)
```

**Via the admin GUI:** Admin Settings → Storage → Encryption → Generate New Key. The key is generated in the browser via `crypto.getRandomValues` and never round-trips through the server before you copy it.

**Via YAML** (a startup `WARN` reminds you to back up the key):

```yaml
advanced:
  encryption_key: "4f1b…64-hex-chars…"
```

> [!WARNING] If you lose the key, encrypted objects are unrecoverable.
> DeltaGlider does not escrow keys. Store the key somewhere outside the proxy host — a secrets manager, an operator password vault, a sealed envelope. The admin panel displays a red banner on every key-touching action as a reminder.

## Chunked wire format

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

## What happens on GET

The reader dispatches on the `dg-encrypted` metadata marker:

- `aes-256-gcm-v1` → decrypt single-shot, return plaintext bytes.
- `aes-256-gcm-chunked-v1` → stream-decrypt chunk by chunk. Range requests compute the first/last chunk in O(1) math (every non-final frame is exactly 65556 wire bytes), fetch only those chunks, decrypt, and trim to the client's `[start, end]`.
- Absent marker → object was written before encryption was enabled; returned as plaintext.

Truncation, reorder, and tamper attempts all fail hard at GCM verification. The client gets a `500 InternalError` — never corrupt data.

## Operational caveats

### Rotation is not supported in this release

Changing the key makes objects written under the old key **unreadable** until the old key is restored. DeltaGlider does not track which key encrypted which object. The admin panel shows an amber banner on every key-rotation action stating this verbatim.

A future release will ship `dg-encryption-key-id` metadata + a key-ring config + a background re-encryption worker. Until then, treat the key as permanent.

### Enabling is not retroactive

Enabling encryption does not encrypt existing objects. Only new writes go through the encrypting wrapper. Disabling encryption leaves previously-encrypted objects readable — as long as the key is still configured. Once the key is removed, those objects are unrecoverable.

### "Disable → write plaintext → re-enable with different key" produces 3 classes

If you toggle encryption off, write some plaintext objects, then re-enable with a **different** key, the bucket now contains:
1. Plaintext objects (no `dg-encrypted` marker).
2. Objects encrypted with the original key (unreadable under the new key).
3. Objects encrypted with the new key.

Reads succeed for classes 1 + 3. Class 2 fails opaquely. There is no built-in way to enumerate which-is-which. Avoid this pattern.

### The key is global

One key encrypts all buckets, all prefixes, all object classes. Per-bucket or per-prefix encryption policies are out of scope for this release (they depend on the multi-key tracking story above).

### Passthrough memory bounds

Chunked encryption keeps the encode/decode path streaming. Peak in-flight memory is ~130 KiB (one plaintext + one ciphertext chunk). This is independent of object size — a 10 GiB passthrough upload does not buffer 10 GiB.

Deltas and references still buffer (xdelta3 is a batch algorithm, bounded by `DGP_MAX_OBJECT_SIZE`, default 100 MiB).

## Related

- [Security checklist — Step 6](../20-production-security-checklist.md#step-6-enable-encryption-at-rest-optional) — the operational walkthrough.
- [How delta works](how-delta-works.md) — why compression happens before encryption.
- [Configuration reference](configuration.md) — the `advanced.encryption_key` field.
