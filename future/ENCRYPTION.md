# Transparent Encryption at Rest (SSE-Proxy)

## Value Proposition

Use cheap, untrusted cloud object storage (Hetzner, Backblaze, any S3) with DGP encrypting everything at rest. Only DGP holds the secret. DGP runs on-prem with arbitrary cache. Backend is a dumb encrypted blob store.

## Design

- AES-256-GCM encryption (hardware-accelerated via AES-NI)
- Per-object random 12-byte IV stored in metadata (`dg-iv`)
- 16-byte authentication tag appended to ciphertext
- Master encryption key from config (`DGP_ENCRYPTION_KEY`) or derived from bootstrap password
- SSE-S3 header (`x-amz-server-side-encryption: AES256`) returned to clients
- SSE-C: client provides key in request headers, DGP uses that instead of master key

## Pipeline Order (critical)

```
PUT: client data → SHA-256 verify → delta compress (plaintext) → encrypt → store to backend
GET: fetch from backend → decrypt → delta reconstruct → stream to client
```

Why compress-then-encrypt:
- Encrypted data is pseudorandom — compression ratio drops to ~0%
- Delta requires comparing plaintext of reference and new file
- reference.bin and deltas are BOTH encrypted independently

## Multipart Upload Interaction

- Parts buffered in memory, assembled, then passed to `engine.store()`
- Encryption happens INSIDE `engine.store()` after assembly — no change to multipart flow
- Parts never written to disk as plaintext — memory only
- The assembled blob is compressed → encrypted → stored. Clean.

## Cache Interaction

- Reference cache (`cache.rs`) stores plaintext references (for delta reconstruction)
- Cache entries come from decrypted backend reads — cached in memory as plaintext
- If DGP crashes, the cache (in-memory, moka) is lost. No plaintext on disk.
- Backend stores ONLY encrypted blobs

## Performance

- AES-256-GCM with AES-NI: ~5 GB/s throughput. Not a bottleneck.
- xdelta3 subprocess is the bottleneck. Encryption adds <1% overhead.
- Per-object overhead: 12 bytes IV + 16 bytes tag = 28 bytes. Negligible.
- CPU: <1ms per MB on AES-NI hardware.
- Memory: zero additional (encrypt/decrypt in-place).

## Edge Cases & Threats

| Case | Handling |
|------|----------|
| Key rotation | Background job re-encrypts all objects. NOT blocking. |
| Lost key | Data permanently lost. DOCUMENT: BACKUP YOUR KEY. |
| Mixed encrypted/unencrypted | Migration: `dg-encrypted: true` in metadata. Objects without = plaintext (backward compat). |
| SSE-C (customer key) | Per-object key from headers. Can't delta-compress across different keys → passthrough for SSE-C. |
| HEAD Content-Length | Must reflect original (plaintext) size, not encrypted. Same pattern as delta (metadata stores original size). |
| Range requests on encrypted | AES-GCM not seekable. Must decrypt entire object, then serve range. Same as delta objects (already buffered). |

## Effort

~1 week. AES-256-GCM via `ring` crate. Well-understood cryptography.

## Addresses

Stefano requirements: SSE-S3, SSE-C
