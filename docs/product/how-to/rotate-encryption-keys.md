# How to rotate or change encryption keys

This guide shows you how to change a backend's encryption key or mode without losing access to historical objects. Every safe path goes through either the `legacy_key` read shim or a data rewrite. When you change `key` and do not set `legacy_key` yourself, the proxy moves the previous key into the `legacy_key` slot, together with the key id that the old objects carry, so that those objects stay readable — background in [encryption at rest](../explanation/encryption-at-rest.md), shim semantics in the [encryption reference](../reference/encryption.md#the-legacy_key-shim).

## Which recipe do you need?

| You want to… | Recipe |
|---|---|
| Rotate a proxy-AES key, minimum disruption | **A** — shim, then re-encrypt job |
| Rotate so the old key never stays in runtime | **B** — new backend + migrate job |
| Move from proxy-AES to SSE-KMS / SSE-S3 | **C** — migrate job to a new SSE backend |
| Stop encrypting a backend | **D** — `mode: none`, keep the shim |

## Recipe A: shim-assisted rotation

1. Generate the new key off-box (`openssl rand -hex 32` into your secrets manager). Keep the old one — you need both for a while.
2. Move the old key into the `legacy_key` slot and put the new key in `key`:

   ```yaml
   encryption:
     mode: aes256-gcm-proxy
     key: "${env:DGP_NEW_KEY}"
     key_id: prod-2026-06
     legacy_key: "${env:DGP_OLD_KEY}"
     legacy_key_id: prod-2025-10      # the id stamped on old objects
   ```

   Apply (hot-reload or restart). The **Rotate key** button on the Backends page does this step for you: it sends only the new key, and the proxy moves the current key and its id into the legacy slot. New writes now use the new key; reads check the new key id first and fall back to the legacy slot. The admin panel shows an info banner while a shim is active.

3. Run a **Re-encrypt job** to rewrite the historical objects under the new key (a rewritten object keeps its original creation time, so its `LastModified` does not change): **Settings → Jobs → + New job → Re-encrypt buckets…**, or:

   ```bash
   curl -b cookies -X POST https://s3.acme.example/_/api/admin/jobs/reencrypt \
     -H 'Content-Type: application/json' \
     -d '{"buckets": ["db-archive", "releases"]}'
   ```

   One durable job per bucket (max 100 per call). While a bucket's job runs, **writes** to it get `503 SlowDown` — SDKs back off and retry — so no racing PUT can land under the old key; **reads pass untouched**. The job survives proxy restarts and resumes from its cursor; watch objects/bytes progress on the Jobs screen.

   ![Jobs screen](/_/screenshots/jobs-screen.jpg)

4. When every job shows `succeeded`, remove `legacy_key` + `legacy_key_id` and apply. The old key can now be destroyed.

   In the admin UI, the shim banner on the backend does this step. The banner shows how many objects and delta references still carry the legacy key id, because the proxy checks the metadata of every object on the backend. The **Clear legacy key** button is enabled only when that check read every object and found none. The button asks for confirmation and then sends `legacy_key: null` and `legacy_key_id: null`. On a backend with more than 10000 objects, the check in the UI stops early and the button stays disabled; call `GET /_/api/admin/backends/:name/legacy-key-usage?limit=N` with a higher limit instead ([admin API](../reference/admin-api.md#backends)).

**Caveat:** the shim holds exactly ONE legacy generation. Don't rotate again while a shim is live — rotate to the final key, not through intermediaries. The proxy refuses a key change that would push a different key out of a live legacy slot, because objects can still need that key. It also refuses a key change that keeps the same `key_id`, because the proxy would then decrypt the old objects with the new key. To drop a legacy key on purpose, send `legacy_key: null` in the same apply.

## Recipe B: rotation via data migration (zero-shim)

Use when the old key must not remain in runtime at all.

1. Declare a NEW backend with the new key (same underlying storage or different). Route no buckets to it yet.
2. Move each bucket with the built-in migrate job — **Settings → Storage → Buckets → (bucket) → Migrate data…** or `POST /_/api/admin/buckets/:bucket/migrate` with the new backend as `target_backend`. The proxy decrypts with the old key on read and re-encrypts with the new key on write; the job is durable, resumable, cancellable pre-flip, and write-gates the bucket. Full procedure: [How to move a bucket to another backend](move-a-bucket-between-backends.md).
3. Once all buckets are flipped, delete the old backend from the config. The old key can be forgotten.

## Recipe C: migrate from proxy-AES to SSE-KMS

The re-encrypt job cannot do this change. It refuses a bucket whose backend uses `sse-kms` or `sse-s3`, because AWS encrypts those objects and leaves no proxy marker that the job can check. So on a backend that you switch to SSE-KMS, the job never rewrites the old proxy-AES objects. Use a migrate job to a new SSE-KMS backend instead.

1. Declare a new backend with SSE-KMS. Route no buckets to it yet:

   ```yaml
   encryption:
     mode: sse-kms
     kms_key_id: arn:aws:kms:eu-west-1:123456789012:key/new-kms
   ```

2. Move each bucket to the new backend with the migrate job: **Settings → Storage → Buckets → (bucket) → Migrate data…**, or `POST /_/api/admin/buckets/:bucket/migrate`. The migrate job reads every object through the old backend, which decrypts it with the proxy key. Then it writes the object through the new backend, where AWS encrypts it with the KMS key. Full procedure: [How to move a bucket to another backend](move-a-bucket-between-backends.md).
3. When every bucket has moved, delete the old backend. The old proxy key is then no longer needed.

You can also switch the existing backend in place, with the old key as the shim:

```yaml
encryption:
  mode: sse-kms
  kms_key_id: arn:aws:kms:eu-west-1:123456789012:key/new-kms
  legacy_key: "${env:DGP_OLD_PROXY_KEY}"
  legacy_key_id: prod-2025-10
```

New writes then go through SSE-KMS, and reads of the old proxy-AES objects decrypt through the shim. But no job rewrites the old objects, so they keep the legacy key id until a client uploads them again or you delete them. The shim banner on the backend counts these objects. Its **Clear legacy key** button stays disabled until the count is zero, because an object under the legacy key id cannot be read after the clear. To reach zero, move the buckets with the migrate job as described above.

The reverse direction (native → proxy-AES) needs no shim. Native objects carry `dg-encrypted-native`, so the proxy decrypt path never runs on them.

## Recipe D: decommission encryption safely

```yaml
encryption:
  mode: none
  legacy_key: "${env:DGP_OLD_KEY}"
  legacy_key_id: prod-2025-10
```

New writes are plaintext; historical encrypted objects stay readable through the shim (`mode: none` with a `legacy_key` is a valid shape). If you want history decrypted on disk too, run a re-encrypt job — it rewrites toward the *current* config, so under `mode: none` it decrypts. Only drop the `legacy_key` once nothing encrypted remains.

## Two questions that come up

**What if I lose ONLY the `legacy_key` after clearing it?** Nothing new — that's the same state as "the shim was never set." If the re-encrypt job already rewrote everything, no object references the old generation and nothing is lost. If some still do, those objects are unrecoverable, like any other key loss.

**How do I audit who's decrypting?** Under SSE-KMS, turn on CloudTrail for the KMS key — every `Decrypt` / `GenerateDataKey` call logs principal, IP, and timestamp. Proxy-AES has no equivalent: the key never moves, so there's no per-decrypt event — only the proxy's own access logs.

## Verify

1. Every bucket's re-encrypt (or migrate) job shows `succeeded` at **Settings → Jobs**, with zero rows in **Failures**.
2. Every object still reads — spot-check old and new objects through the proxy:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 cp s3://db-archive/nightly/2025-01-01.dump - | sha256sum
   ```

3. On the raw backend, a rewritten object's `dg-encryption-key-id` metadata shows the **new** key id.
4. The shim banner in the admin panel is gone after you clear `legacy_key`.
5. A read failing with "object was encrypted with key id X, but this backend is configured with key id Y" means some objects were missed — restore the shim and re-run the job ([troubleshooting](troubleshooting.md)).

## Related

- [How to encrypt data at rest](encrypt-data-at-rest.md) — first-time setup, mode choice, key handling.
- [How to move a bucket to another backend](move-a-bucket-between-backends.md) — the migrate job recipe B rides on.
- [Encryption reference](../reference/encryption.md) — key ids, markers, shim semantics, limits.
- [Jobs reference](../reference/jobs.md) — the write gate and job durability model.
