# How to rotate or change encryption keys

This guide shows you how to change the encryption key or the encryption mode of a backend without losing access to the objects that it already holds. Every safe path goes through either the `legacy_key` read shim or a rewrite of the data. When you change `key` and do not set `legacy_key` yourself, the proxy moves the previous key into the `legacy_key` slot, together with the key id that the old objects carry, so that those objects stay readable. The background is in [encryption at rest](../explanation/encryption-at-rest.md), and the shim semantics are in the [encryption reference](../reference/encryption.md#the-legacy_key-shim).

The examples use the backend `hetzner-fsn1`, which holds the buckets `releases` and `downloads` and uses proxy-side AES with the key id `hetzner-2026-06`.

## Which recipe do you need?

| You want to… | Recipe |
|---|---|
| Rotate a proxy-side AES key, with minimum disruption | **A**: shim, then re-encrypt job |
| Rotate so that the old key never stays in the running proxy | **B**: new backend and migrate job |
| Move from proxy-side AES to SSE-KMS or SSE-S3 | **C**: migrate job to a new SSE backend |
| Stop encrypting a backend | **D**: mode `none`, keep the shim |

## Recipe A: rotation with the shim

In the admin UI:

1. In the sidebar, open **Storage → Backends** (`/_/admin/storage/backends`).
2. On the card of `hetzner-fsn1`, click **Rotate key**. The button shows only on a backend in **AES-256-GCM (proxy-side)** mode.

   ![The hetzner-fsn1 backend uses AES-256-GCM with the key id hetzner-2026-06; the arrow points at the Rotate key button.](/_/screenshots/rotate-key-button.webp)

3. The browser generates a new key and shows it once. Click **Copy to clipboard**, and store the key in your secrets manager. Keep the old key too, because you need both for a while.
4. Select the checkbox **I have stored this key safely**, and click **Apply**.

   The admin UI sends only the new key. The proxy moves the current key and its id into the legacy slot. New writes now use the new key. Reads check the key id of the new key first, and they fall back to the legacy slot. The admin UI cannot set `key_id`, so the new key gets an id that the proxy derives from the backend name and the key (16 hex characters).

5. The dialog **Re-encrypt existing objects with the new key?** lists the buckets of the backend. Click **Start now**.

   ![After the rotation, the dialog Re-encrypt existing objects with the new key? lists the buckets of hetzner-fsn1; the arrow points at Start now.](/_/screenshots/rotate-reencrypt.webp)

   The proxy creates one durable re-encrypt job for each bucket (at most 100 in one request). A rewritten object keeps its original creation time, so its `LastModified` does not change. While the job of a bucket runs, **writes** to that bucket get `503 SlowDown`, and SDKs back off and retry. So no racing PUT can land under the old key. **Reads pass untouched.** The job survives restarts of the proxy, and it resumes from its cursor. If you click **Later**, start the jobs from **Storage → Jobs** with **New job** → **Re-encrypt buckets… — one-off rewrite**.

6. Open **Storage → Jobs** (`/_/admin/jobs`), and wait until every re-encrypt job shows `succeeded`. The rows show the progress in objects and bytes.
7. Back on **Storage → Backends**, the card of `hetzner-fsn1` shows the banner **Decrypt-only shim active**. Click **Check usage**. The proxy then checks the metadata of every object on the backend, and the banner counts the objects and delta references that still carry the legacy key id. The check reads every object, so it runs only when you click the button. Click **Check again** after the jobs finish.
8. When the check finds no such object, click **Clear legacy key**, and then click **Clear** in the dialog **Clear the legacy key?**.

   ![The hetzner-fsn1 backend shows the legacy key banner, which reports that no object uses the legacy key id hetzner-2026-06; a box marks Clear legacy key.](/_/screenshots/rotate-clear-legacy.webp)

   The button is enabled only when the check read every object and found none under the legacy key id. On a backend with more than 10000 objects, the check stops early, and the button stays disabled. In that case, send `GET /_/api/admin/backends/hetzner-fsn1/legacy-key-usage?limit=N` with a higher limit ([admin API](../reference/admin-api.md#backends)), and clear the key with a section update that sets `legacy_key: null` and `legacy_key_id: null`. After the clear, you can destroy the old key.

While a shim is active, **Rotate key** and the **Encryption mode** list are disabled. The shim holds exactly one legacy generation, so clear it before the next rotation.

### The same change in YAML

The steps above write the keys into the `storage` section of `deltaglider_proxy.yaml` ([why the UI and the file hold the same configuration](../explanation/two-ways-to-configure.md)). The file then holds both keys in plain text: the new key in `key`, and the old key and its id in `legacy_key` and `legacy_key_id`. To keep the keys out of the file, reference them from the environment, and pin both ids yourself. The new key can come from the backend variable `DGP_BACKEND_HETZNER_FSN1_ENCRYPTION_KEY`; the old key needs a `${env:...}` reference, because no variable sets `legacy_key`. This block is a fragment of the backend entry, because it references environment variables that exist only on the proxy host:

```yaml
# fragment
      encryption:
        mode: aes256-gcm-proxy
        key_id: hetzner-2026-09           # the new key comes from DGP_BACKEND_HETZNER_FSN1_ENCRYPTION_KEY
        legacy_key: "${env:HETZNER_OLD_KEY}"
        legacy_key_id: hetzner-2026-06    # the id stamped on the old objects
```

After you edit the file, restart the proxy, or apply the file with `deltaglider_proxy config apply deltaglider_proxy.yaml --server https://s3.acme.example`. The CLI expands the `${env:...}` references from your own environment before it sends the file. When every re-encrypt job shows `succeeded`, remove `legacy_key` and `legacy_key_id`, and apply again.

**Caveat:** the shim holds exactly one legacy generation. Do not rotate again while a shim is live. Rotate to the final key, not through intermediate keys. The proxy refuses a key change that would push a different key out of a live legacy slot, because objects can still need that key. It also refuses a key change that keeps the same `key_id`, because the proxy would then decrypt the old objects with the new key. To drop a legacy key on purpose, send `legacy_key: null` in the same apply.

## Recipe B: rotation through a data migration (no shim)

Use this recipe when the old key must not remain in the running proxy at all.

1. Add a new backend with the new key, on the same underlying storage or on a different one. Route no buckets to it yet. [How to encrypt data at rest](encrypt-data-at-rest.md) shows how to set its encryption.
2. Move each bucket to the new backend with the built-in migrate job: on **Storage → Buckets**, open the bucket, click **Migrate data…**, select the new backend in **Target backend**, and click **Start migration**. The proxy decrypts with the old key on read, and it encrypts with the new key on write. The job is durable, resumable, and cancellable before the switch, and it gates writes to the bucket. The full procedure, with screenshots, is in [How to move a bucket to another backend](move-a-bucket-between-backends.md). The migrate request gets `409 Conflict` while `config_sync_bucket` is set, because the switch changes the routing of one instance only. Run the migration on a single instance.
3. When the migrate jobs have moved all buckets, delete the old backend on **Storage → Backends**. You can then forget the old key.

A YAML change cannot do step 2, because a change of `storage.buckets.<bucket>.backend` only reroutes the bucket and copies nothing.

## Recipe C: migrate from proxy-AES to SSE-KMS

The re-encrypt job cannot make this change. It refuses a bucket whose backend uses `sse-kms` or `sse-s3`, because AWS encrypts those objects and leaves no proxy marker that the job can check. So on a backend that you switch to SSE-KMS, the job never rewrites the old proxy-side AES objects. Use a migrate job to a new SSE-KMS backend instead.

1. Add a new backend on AWS with the encryption mode **SSE-KMS (AWS KMS)**, and route no buckets to it yet. The steps are in [How to encrypt data at rest](encrypt-data-at-rest.md#4-or-use-sse-kms-or-sse-s3-on-aws).
2. Move each bucket to the new backend with the migrate job, as in recipe B. The migrate job reads every object through the old backend, which decrypts it with the proxy key. Then it writes the object through the new backend, where AWS encrypts it with the KMS key. As in recipe B, the migrate request gets `409 Conflict` while `config_sync_bucket` is set.
3. When every bucket has moved, delete the old backend. The old proxy key is then no longer needed.

You can also switch the existing backend in place, with the old key as the shim. The admin UI does this when you select **SSE-KMS (AWS KMS)** in the **Encryption mode** list of a proxy-side AES backend: the proxy moves the current key into the legacy slot. The same state in YAML:

```yaml
# fragment
      encryption:
        mode: sse-kms
        kms_key_id: arn:aws:kms:eu-west-1:123456789012:key/new-kms
        legacy_key: "${env:HETZNER_OLD_KEY}"
        legacy_key_id: hetzner-2026-06
```

New writes then go through SSE-KMS, and reads of the old proxy-side AES objects decrypt through the shim. But no job rewrites the old objects, so they keep the legacy key id until a client uploads them again or you delete them. The shim banner on the backend counts these objects. Its **Clear legacy key** button stays disabled until the count is zero, because an object under the legacy key id cannot be read after the clear. To reach zero, move the buckets with the migrate job as described above.

The reverse direction (SSE to proxy-side AES) needs no shim. SSE objects carry `dg-encrypted-native`, so the proxy decrypt path never runs on them.

## Recipe D: turn off encryption safely

In the admin UI, select **None (plaintext)** in the **Encryption mode** list of the backend, and click **Apply**. The proxy moves the current key into the legacy slot, and the dialog **Decrypt existing objects?** proposes a re-encrypt job. The same state in YAML:

```yaml
# fragment
      encryption:
        mode: none
        legacy_key: "${env:HETZNER_OLD_KEY}"
        legacy_key_id: hetzner-2026-06
```

New writes are plaintext. The encrypted objects stay readable through the shim (`mode: none` with a `legacy_key` is a valid shape). If you want the old objects decrypted on disk too, run the re-encrypt job. It rewrites objects toward the *current* configuration, so under `mode: none` it decrypts them. Clear the legacy key only when no encrypted object remains.

## Two questions that come up

**What if I lose only the `legacy_key` after I clear it?** Nothing changes. This is the same state as "the shim was never set". If the re-encrypt job already rewrote everything, no object references the old generation, and nothing is lost. If some objects still do, those objects are unrecoverable, like after any other key loss.

**How do I audit who decrypts?** Under SSE-KMS, turn on CloudTrail for the KMS key. Every `Decrypt` and `GenerateDataKey` request logs the principal, the IP address and the time. Proxy-side AES has no equivalent. The key never leaves the proxy, so there is no event for each decrypt, and only the access logs of the proxy exist.

## Verify

1. Check that the re-encrypt (or migrate) job of every bucket shows `succeeded` on **Storage → Jobs**, with no rows on its **Failures** tab.
2. Check that every object still reads. Spot-check old and new objects through the proxy:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 cp s3://releases/firmware/widget-3000/fw-1.4.0.tar - | sha256sum
   ```

3. On the raw backend, check that the `dg-encryption-key-id` metadata of a rewritten object shows the **new** key id.
4. Check that the banner **Decrypt-only shim active** is gone after you clear the legacy key.
5. If a read fails with "object was encrypted with key id X, but this backend is configured with key id Y", the job missed some objects. Restore the shim and run the job again ([troubleshooting](troubleshooting.md)).

## Related

- [How to encrypt data at rest](encrypt-data-at-rest.md): first-time setup, the choice of mode, and key handling.
- [How to move a bucket to another backend](move-a-bucket-between-backends.md): the migrate job that recipes B and C use.
- [Encryption reference](../reference/encryption.md): key ids, markers, shim semantics, and limits.
- [Jobs reference](../reference/jobs.md): the write gate and the durability model of jobs.
- [Two ways to configure DeltaGlider](../explanation/two-ways-to-configure.md): how the admin UI and the YAML file relate.
