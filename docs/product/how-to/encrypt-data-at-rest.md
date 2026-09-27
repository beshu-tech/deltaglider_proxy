# How to encrypt data at rest

This guide shows you how to enable at-rest encryption on a storage backend. Encryption is backend-scoped: every bucket routed to the backend inherits it. For the threat model behind each mode, see [encryption at rest](../explanation/encryption-at-rest.md).

## 1. Pick a mode

- If the backend is a filesystem, or an S3 provider you don't fully trust with plaintext, use `aes256-gcm-proxy`. The proxy encrypts the bytes before the backend sees them.
- If the backend is AWS S3 and you want per-decrypt audit logs and KMS key management, use `sse-kms`. If AWS-managed AES256 is enough, use `sse-s3`.
- If the bucket's contents are public anyway, keep `none`, because there encryption only adds overhead.

The full mode matrix, field list, and wire format are in the [encryption reference](../reference/encryption.md).

## 2. Generate and place the key (proxy-AES only)

Generate a 32-byte hex key and put it in the proxy's environment, never in the config file. The proxy derives the env-var name from the backend name (uppercase, `-`/`.` → `_`):

```bash
# for backend hetzner-fsn1
export DGP_BACKEND_HETZNER_FSN1_ENCRYPTION_KEY=$(openssl rand -hex 32)
# singleton-backend deployments use:
export DGP_ENCRYPTION_KEY=$(openssl rand -hex 32)
```

Before going further, store the key off-box, for example in a secrets manager, an operator vault, or a sealed envelope. **If you lose a proxy-AES key, the encrypted objects on that backend are unrecoverable.** The proxy does not escrow keys. There is no recovery path.

In the admin UI, open **Settings → Storage → Backends**. Each backend card has an encryption subsection with a mode dropdown and a key-generation widget. The browser generates the keys (`crypto.getRandomValues`), and the keys never go through the server before **Apply**. The panel shows a red key-loss banner, and it enables **Apply** only after you tick the "I have stored this key safely" checkbox.

![Enable encryption on a backend](/_/screenshots/encryption-enable.jpg)

## 3. Configure the backend

This section gives one worked example for each mode.

**Proxy-AES on a named S3 backend.** The key comes from the env var in step 2, so no `key` field appears in the YAML:

```yaml
# validate
storage:
  default_backend: hetzner-fsn1
  backends:
    - name: hetzner-fsn1
      type: s3
      endpoint: https://fsn1.your-objectstorage.com
      region: fsn1
      force_path_style: true
      encryption:
        mode: aes256-gcm-proxy
        key_id: hetzner-2026-06    # optional but recommended — stamps objects with a stable key generation
  buckets:
    db-archive:
      backend: hetzner-fsn1
```

**Proxy-AES on a singleton filesystem backend** (this block has no `# validate`, because the
`${env:…}` reference expands only when `DGP_ENCRYPTION_KEY` is set):

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

**SSE-KMS on an AWS backend.** The proxy never touches key material, and AWS does the cryptography:

```yaml
# validate
storage:
  default_backend: aws-dr
  backends:
    - name: aws-dr
      type: s3
      region: eu-west-1
      encryption:
        mode: sse-kms
        kms_key_id: arn:aws:kms:eu-west-1:123456789012:key/abcd-ef01
        bucket_key_enabled: true   # reduces per-request KMS cost
```

**SSE-S3 on an AWS backend**:

```yaml
      encryption:
        mode: sse-s3
```

The native SSE modes work only on S3. The proxy rejects them on filesystem backends at config check.

## 4. Restart and verify

Restart the proxy (or apply from the UI) so the env var and config load together, then check the round trip:

1. Write an object and read it back through the proxy. Clients must notice nothing:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 cp dump.sql s3://db-archive/nightly/dump.sql
   aws --endpoint-url https://s3.acme.example s3 cp s3://db-archive/nightly/dump.sql - | sha256sum
   ```

   The hash must match the original.

2. Check that the stored object is ciphertext. Look at it on the raw backend, without the proxy. In proxy-AES mode the body starts with the `DGE1` magic (chunked) or is opaque GCM ciphertext, and the backend-side user metadata carries the `dg-encrypted` marker (an `aes-256-gcm-*` value) plus `dg-encryption-key-id`. On a filesystem backend the markers live in the `user.dg.metadata` xattr:

   ```bash
   xattr -p user.dg.metadata /var/lib/deltaglider_proxy/data/db-archive/nightly/dump.sql
   ```

   Native modes stamp `dg-encrypted-native: sse-kms` (or `sse-s3`) instead. That marker is harmless to expose.

3. Old plaintext objects still read correctly. The decrypt path dispatches on the marker, and an absent marker means "serve as-is."

## 5. Encrypt the historical objects

Encryption is **not retroactive**. The proxy encrypts only new writes, and existing objects stay in their stored form. When you change a backend's encryption in the admin UI, the Backends page proposes a **Re-encrypt job** that rewrites every object not matching the new config.

![Re-encrypt proposal](/_/screenshots/reencrypt-proposal.jpg)

Accept it (or start one later from **Settings → Jobs → + New job → Re-encrypt buckets…**). The job write-gates each bucket while it runs, and it survives restarts. [How to rotate or change encryption keys](rotate-encryption-keys.md) explains the mechanics, including what the write gate means for clients.

## Verify

- A fresh PUT through the proxy reads back byte-identical (step 4.1).
- The raw backend stores ciphertext and the `dg-encrypted` / `dg-encrypted-native` marker (step 4.2).
- The key is stored somewhere safe **outside** the proxy host.
- If you ran a re-encrypt job, its row at **Settings → Jobs** shows `succeeded` and a raw-backend spot-check of an *old* object now shows the marker too.

## Related

- [How to rotate or change encryption keys](rotate-encryption-keys.md): rotation recipes, the `legacy_key` shim, and the re-encrypt job in detail.
- [Encryption reference](../reference/encryption.md): modes, fields, env vars, markers, wire format, limits.
- [Encryption at rest](../explanation/encryption-at-rest.md): which mode for which threat model, and why.
- [How to move a bucket to another backend](move-a-bucket-between-backends.md): migration as a re-encryption path.
