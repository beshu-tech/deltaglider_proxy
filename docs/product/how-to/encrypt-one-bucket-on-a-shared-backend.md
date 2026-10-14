# How to encrypt one bucket on a shared backend

This guide shows you how to encrypt one bucket with its own key while the other buckets on the same backend stay as they are. A bucket can carry its own `encryption` block, and that block replaces the encryption of its backend for that bucket only. For the mechanism (one keyring per backend, one key id per object), see [encryption at rest](../explanation/encryption-at-rest.md).

The example encrypts `db-archive`, where `backup-bot` writes nightly database dumps, on the backend `hetzner-fsn1`. The bucket `downloads` on the same backend holds public installers, and it stays plaintext. You keep one backend for one storage account, and you do not define a second backend with the same credentials.

**If you lose the bucket key, the objects that it encrypts are unrecoverable.** A full backup (the **Backup** card on the **System** page) includes a key that is in the config file. It does not include a key that an environment variable supplies, and a backup on the same host is lost with the host. Store every key outside the proxy host as well, for example in a secrets manager.

Per-bucket encryption has two limits in this release. A bucket block takes the modes that the server accepts on a bucket (`aes256-gcm-proxy` or `none` today; the admin UI shows only the modes that the server accepts). And the proxy refuses a bucket block while `config_sync_bucket` is set, because the YAML config is per node. The [encryption reference](../reference/encryption.md#per-bucket-encryption) has the full list of rules.

## 1. Give the bucket its own key

In the admin UI:

1. In the sidebar, open **Storage → Buckets** (`/_/admin/storage/buckets`).
2. Click the `db-archive` row to open it. The **Encryption** group sits between **Public access** and **Backend**. It names the current state, for example "Now: Not encrypted.", and the first option names what the backend does: **Inherit — hetzner-fsn1: plaintext**.
3. Select **Encrypt with a bucket key**.

   ![The open db-archive row on the Buckets page; callout 1 marks the db-archive row, and callout 2 marks the Encrypt with a bucket key option in the Encryption group.](/_/screenshots/bucket-encryption-choose.webp)

4. Leave **Generate a key** selected. The browser generates a random 256-bit key and shows it once, under **Generated key (64 hexadecimal characters, shown once)**. To use a key that you already have, select **Paste a key** and paste the 64 hexadecimal characters.
5. Check the **Key id**. The proxy stamps this id on every object that the key encrypts, and it picks the key by this id when it reads the object. The page suggests `db-archive-2026-10` (the bucket name, the year and the month). A bucket key id is always explicit, because an id that depends on a name would change when the bucket is renamed.
6. Click **Download key file**, and store the file in your secrets manager. The file holds the bucket name, the backend, the key id, the date, the key and a recovery note. **Copy** puts only the key on the clipboard.
7. In **To confirm that you stored the key, type its last 6 characters**, type the last 6 characters of the key. **Review & apply** stays disabled until they match. This check replaces an "I stored it" checkbox: you can only type the characters when you have a copy of the key.
8. Click **Review & apply**.

   ![The key setup of db-archive with the generated key hidden; callout 5 marks the Key id field, a box marks Download key file (step 6), callout 7 marks the field for the last 6 characters, and callout 8 marks Review & apply.](/_/screenshots/bucket-encryption-key.webp)

9. Read the dialog, check the diff, and then click **Apply and Persist**. The dialog says that only new writes change. It also says that the re-encrypt job, which the page offers next, gives the bucket a temporary `503` for uploads and deletes while it runs.

The change applies at once, with its own request. It is not staged with the other fields of the bucket, so the **Encryption** group waits while the page has unsaved changes: apply or discard them first. When the bucket has no explicit backend yet, the page also sets `backend: hetzner-fsn1`, because a bucket's own block belongs to the storage that holds the bucket, and the server refuses a block without a backend when named backends exist.

When you apply here, the proxy writes the key into the config file, in plain text, in the `key` field of the bucket. The key setup shows the YAML and the environment variable that keep the key out of the file (see the next section).

## 2. Encrypt the objects that the bucket already holds

The new key encrypts new writes only. The objects that `db-archive` already holds stay in the form that they were written in, and they stay readable, because each object names the key that wrote it. After the apply, the page asks **Encrypt existing objects?** and lists `db-archive` only. The other buckets on `hetzner-fsn1` are not listed, because their encryption did not change.

1. Check that `db-archive` is selected.
2. Click **Start now**. The proxy starts a re-encrypt job for the bucket.

   ![The dialog Encrypt existing objects? lists only db-archive; the arrow points at Start now.](/_/screenshots/bucket-encryption-reencrypt.webp)

While the job runs, reads of `db-archive` work, and uploads and deletes get `503 SlowDown`, which S3 clients retry. The other buckets on the backend are not affected. If you click **Later**, start the job when you are ready with **Re-encrypt existing objects** in the open bucket row, or from **Storage → Jobs**.

## 3. Create a bucket that is encrypted from its first object

To create a new bucket with its own key, choose the encryption in the create dialog. The proxy writes the bucket's encryption block before it creates the bucket, so no object is ever written in the backend's mode first.

1. On **Storage → Buckets**, click **Create bucket**.
2. Type the bucket name. In the backend list, each backend names its encryption, for example `s3 · plaintext`.
3. Under **Encryption**, select **Encrypt with a bucket key**. The default is **Same as backend (plaintext)**, which writes no block.
4. Follow steps 4 to 7 of section 1, and then click **Create**.

   ![The Create bucket dialog with the Encryption choice; the arrow points at Encrypt with a bucket key.](/_/screenshots/create-bucket-encryption.webp)

If the proxy refuses the encryption block, it does not create the bucket, and the dialog shows the reason. If the block is saved but the create fails, the dialog says so. The Buckets page then shows the settings as **bucket not found**, and **Create** creates the bucket under them.

## The same change in YAML

The steps above write this configuration into the `storage` section of `deltaglider_proxy.yaml` ([why the UI and the file hold the same configuration](../explanation/two-ways-to-configure.md)). This example keeps the key out of the file with a `${env:...}` reference. Any variable name works: the proxy expands the reference when it loads the file, and it never writes the key value back into the file.

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
  buckets:
    downloads:
      public_prefixes: ["public/"]
    db-archive:
      backend: hetzner-fsn1
      encryption:
        mode: aes256-gcm-proxy
        key_id: db-archive-2026-10
        key: ${env:DB_ARCHIVE_ENCRYPTION_KEY}
```

Set the variable in the environment of the proxy, for example `DB_ARCHIVE_ENCRYPTION_KEY=<64 hexadecimal characters>`. While the key comes from a variable, the **Encryption** group of the bucket is read-only and shows a **from env DB_ARCHIVE_ENCRYPTION_KEY** badge: change the key in the deployment, not in the admin UI.

After you edit the file, restart the proxy, or apply the file to the running proxy with `POST /_/api/admin/config/apply` or with `deltaglider_proxy config apply deltaglider_proxy.yaml --server https://s3.acme.example`. The apply response has a warning for `db-archive` that names the re-encrypt job.

## Rotate the bucket key, turn it off, and clear the old key

In the open bucket row:

- **Rotate key** gives the bucket a new key and a new key id, with the same key setup as section 1. The old key becomes a decrypt-only legacy key, so the objects under it stay readable. The page offers the re-encrypt job for the bucket afterwards.
- **Off for this bucket** stores new writes as plaintext, even when the backend encrypts. It is not the same as **Inherit**. The bucket key stays as a decrypt-only legacy key. Because new writes lose their encryption, the page asks you to confirm first, and the request carries the header `X-DGP-Confirm-Encryption-Downgrade: db-archive`.
- **Inherit** removes the bucket's block, so the bucket follows its backend again. The page offers it only when the bucket holds no key of its own. A key that objects use never leaves in one step.

One legacy slot exists per bucket. While it holds a key, **Rotate key** and **Off for this bucket** are disabled, and the bucket row shows a **Decrypt-only shim active** banner. To clear the legacy key:

1. Run the re-encrypt job for the bucket.
2. In the banner, click **Check usage**. The proxy scans every bucket on the backend for objects and delta references that still carry the legacy key id (`GET /_/api/admin/backends/hetzner-fsn1/legacy-key-usage?bucket=db-archive`).
3. When the check finds no such object, click **Clear legacy key**, and confirm. The request carries the header `X-DGP-Confirm-Key-Removal: <legacy key id>`.

[How to rotate or change encryption keys](rotate-encryption-keys.md) explains the legacy slot in detail.

## Verify

1. On **Storage → Buckets**, check the encryption chip of each row. `db-archive` shows **Encrypted · bucket key db-archive-2026-10**, and `downloads` shows **Not encrypted**. A bucket that takes the encryption of an encrypting backend shows **Encrypted · from hetzner-fsn1**.

   ![The Buckets page; a box marks the chip Encrypted · bucket key db-archive-2026-10 of db-archive, and the downloads row shows Not encrypted.](/_/screenshots/bucket-encryption-chip.webp)

2. The admin API reports the same answer for each bucket (see [the admin API reference](../reference/admin-api.md)):

   ```bash
   curl -s -b cookies.txt https://s3.acme.example/_/api/admin/buckets \
     | jq '.buckets[] | {name, effective_encryption}'
   ```

   `db-archive` has `"source": "bucket"` and `"key_id": "db-archive-2026-10"`; `downloads` has `"source": "backend"` and `"mode": "none"`.

3. Write an object through the proxy, and look at the stored copy on the backend, without the proxy. The user metadata carries the `dg-encrypted` marker and `dg-encryption-key-id: db-archive-2026-10`:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 cp nightly.dump s3://db-archive/nightly/2026-10-14.dump
   aws s3api head-object --bucket db-archive --key nightly/2026-10-14.dump --profile hetzner
   ```

   An object in `downloads` has no `dg-encrypted` marker.

4. Read the object back through the proxy, and compare its hash with the original file. Clients notice nothing.

## Related

- [How to encrypt data at rest](encrypt-data-at-rest.md): encryption for every bucket of a backend, and the native SSE modes.
- [How to rotate or change encryption keys](rotate-encryption-keys.md): rotation recipes and the legacy slot.
- [Encryption reference](../reference/encryption.md#per-bucket-encryption): the rules of a bucket's `encryption` block.
- [How to move a bucket to another backend](move-a-bucket-between-backends.md): a bucket's own block moves with it.
