# How to encrypt one bucket on a shared backend

This guide shows you how to encrypt one bucket with its own key while the other buckets on the same backend stay as they are. A bucket can carry its own `encryption` block, and that block replaces the encryption of its backend for that bucket only. For the mechanism (one keyring per backend, one key fingerprint per object), see [encryption at rest](../explanation/encryption-at-rest.md).

The example encrypts `db-archive`, where `backup-bot` writes nightly database dumps, on the backend `hetzner-fsn1`. The bucket `downloads` on the same backend holds public installers, and it stays plaintext. You keep one backend for one storage account, and you do not define a second backend with the same credentials.

**If you lose the bucket key, the objects that it encrypts are unrecoverable.** A full backup (the **Backup** card on the **System** page) includes a key that is in the config file. It does not include a key that an environment variable supplies, and a backup on the same host is lost with the host. Store every key outside the proxy host as well, for example in a secrets manager.

A bucket block takes `aes256-gcm-proxy`, `none`, and on an S3 backend also the native modes `sse-kms` and `sse-s3`. The admin UI offers only the modes that the server accepts for the bucket's backend. With `config_sync_bucket` set (several instances), the encryption witness makes an instance that disagrees about a bucket's encryption refuse writes to it, so apply the same configuration on every instance. The [encryption reference](../reference/encryption.md#per-bucket-encryption) has the full list of rules.

## 1. Give the bucket its own key

In the admin UI:

1. In the sidebar, open **Storage → Buckets** (`/_/admin/storage/buckets`).
2. Click the `db-archive` row to open it. The **Encryption** group sits between **Public access** and **Backend**. It names the current state, for example "Now: Not encrypted.", and the first option names what the backend does: **Inherit — hetzner-fsn1: plaintext**.
3. Select **Encrypt with a bucket key**.

   ![The open db-archive row on the Buckets page; callout 1 marks the db-archive row, and callout 2 marks the Encrypt with a bucket key option in the Encryption group.](/_/screenshots/bucket-encryption-choose.webp)

4. Leave **Generate a key** selected. The browser generates a random 256-bit key and shows it once, under **Generated key (64 hexadecimal characters, shown once)**. To use a key that you already have, select **Paste a key** and paste the 64 hexadecimal characters.
5. Check the **Key id**. The admin UI, the audit log and the jobs show the key under this id. The objects themselves store only a fingerprint of the key, and the proxy picks the key by that fingerprint when it reads an object. The page suggests `db-archive-2026-10` (the bucket name, the year and the month). A bucket key id is always explicit. Give each key its own id: one key cannot have two ids on one backend.
6. Click **Download key file**, and store the file in your secrets manager. The file holds the bucket name, the backend, the key id, the date, the key and a recovery note. **Copy** puts only the key on the clipboard.
7. In **To confirm that you stored the key, type its last 6 characters**, type the last 6 characters of the key. **Review & apply** stays disabled until they match. This check replaces an "I stored it" checkbox: you can only type the characters when you have a copy of the key.
8. Click **Review & apply**.

   ![The key setup of db-archive with the generated key hidden; callout 5 marks the Key id field, a box marks Download key file (step 6), callout 7 marks the field for the last 6 characters, and callout 8 marks Review & apply.](/_/screenshots/bucket-encryption-key.webp)

9. Read the dialog, check the diff, and then click **Apply and Persist**. The dialog says that only new writes change. For a bucket that did not encrypt before, it also says that the proxy keeps serving the objects that are already in the bucket until the re-encrypt job converts them. It also says that the re-encrypt job, which the page offers next, gives the bucket a temporary `503` for uploads and deletes while it runs.

The change applies at once, with its own request. It is not staged with the other fields of the bucket, so the **Encryption** group waits while the page has unsaved changes: apply or discard them first. When the bucket has no explicit backend yet, the page also sets `backend: hetzner-fsn1`, because a bucket's own block belongs to the storage that holds the bucket, and the server refuses a block without a backend when named backends exist.

The objects that `db-archive` already holds are plaintext, because `hetzner-fsn1` does not encrypt. In a bucket that encrypts, the proxy refuses a plaintext object by default, because it cannot tell the history of the bucket from an object that someone planted on the backend ([why](../explanation/encryption-at-rest.md#why-enabling-isnt-retroactive)). For a bucket that did not encrypt before, the page therefore also sets `plaintext_reads: serve` in the bucket's block, so that the existing objects stay readable until section 2 converts them.

When you apply here, the proxy writes the key into the config file, in plain text, in the `key` field of the bucket. The key setup shows the YAML and the environment variable that keep the key out of the file (see the next section).

## 2. Encrypt the objects that the bucket already holds

The new key encrypts new writes only. The objects that `db-archive` already holds stay in the form that they were written in. An object that the backend key encrypted stays readable, because the keyring of the backend holds that key. A plaintext object stays readable while the bucket's block has `plaintext_reads: serve`, and the re-encrypt job encrypts it.

In the admin UI:

1. After the apply, the page asks **Encrypt existing objects?** and lists `db-archive` only. The other buckets on `hetzner-fsn1` are not listed, because their encryption did not change. Check that `db-archive` is selected.
2. Click **Start now**. The proxy starts a re-encrypt job for the bucket.

   ![The dialog Encrypt existing objects? lists only db-archive; the arrow points at Start now.](/_/screenshots/bucket-encryption-reencrypt.webp)

   While the job runs, reads of `db-archive` work, and uploads and deletes get `503 SlowDown`, which S3 clients retry. The other buckets on the backend are not affected. If you click **Later**, start the job when you are ready with **Re-encrypt existing objects** in the open bucket row, or from **Storage → Jobs**.

3. Open **Storage → Jobs** (`/_/admin/jobs`), and wait until the re-encrypt job of `db-archive` shows `succeeded`, with no rows on its **Failures** tab.
4. On **Storage → Buckets**, open the `db-archive` row. The **Encryption** group shows the box **Unencrypted objects are served**. Click **Refuse unencrypted objects**. From now on, the proxy refuses every object in `db-archive` that it did not encrypt.

While `plaintext_reads: serve` is set, the proxy trusts every plaintext object in the bucket as it is, and the job seals it as part of the bucket's history. Keep that window short.

## 3. Create a bucket that is encrypted from its first object

To create a new bucket with its own key, choose the encryption in the create dialog. The dialog sends the bucket's encryption block with the create request, and the proxy writes the route, the block and the bucket in one step. No object is ever written in the backend's mode first. The proxy refuses a name that already exists, and it changes nothing for it.

1. On **Storage → Buckets**, click **Create bucket**.
2. Type the bucket name. In the backend list, each backend names its encryption, for example `s3 · plaintext`.
3. Under **Encryption**, select **Encrypt with a bucket key**. The default is **Same as backend (plaintext)**, which writes no block.
4. Follow steps 4 to 7 of section 1, and then click **Create**.

   ![The Create bucket dialog with the Encryption choice; the arrow points at Encrypt with a bucket key.](/_/screenshots/create-bucket-encryption.webp)

The default option names what the bucket gets without a choice. For a name that already has settings (a bucket that you prepared with **Add settings for a bucket that does not exist yet**), it is **Keep the settings this name already has**. **Off for this bucket** on an encrypting backend asks you to confirm plaintext before the dialog sends the request.

If the proxy refuses the request, or the create fails, the proxy keeps nothing of it, and the dialog shows the reason. Settings with a key for a bucket that does not exist (for example from a release before this one) show as **bucket not found** on the Buckets page. To drop them, open the row, select **Remove these settings…** in the **⋯** menu, and confirm. No object can use the key of a bucket that does not exist, so the proxy removes it without a key check.

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
        plaintext_reads: serve      # step 4 of section 2 removes this line
```

The admin UI writes `plaintext_reads: serve` because the bucket did not encrypt before. **Refuse unencrypted objects** in section 2 removes the line again (the default is `refuse`). Remove it in the file only when the re-encrypt job of the bucket succeeded.

An apply that makes an existing bucket start to encrypt without `plaintext_reads: serve` is refused, because its plaintext objects would become unreadable. When the bucket holds no plaintext that anyone needs, leave the line out and confirm instead: `deltaglider_proxy config apply deltaglider_proxy.yaml --server https://s3.acme.example --confirm-plaintext-refusal db-archive`, or the header `X-DGP-Confirm-Plaintext-Refusal: db-archive` with `POST /_/api/admin/config/apply`. A bucket that the create dialog of section 3 makes needs neither, because it holds no objects.

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

## Several instances

With `config_sync_bucket` set, the instances share the encryption of each bucket through the encryption witness in the sync bucket. Each instance compares the encryption that it uses for new writes to a bucket with the encryption that the witness holds for that bucket. When the two differ, that instance refuses writes to the bucket with `503 SlowDown`. Reads keep working, and so do writes to the other buckets. A config apply that changes the encryption of a bucket publishes the new encryption, so the change in section 1 needs nothing more: apply it on every instance. A restart with a new config publishes nothing, so after such a rollout you publish the encryption of the instances that have the new config.

In the admin UI, on an instance that has the new config:

1. Open **Storage → Buckets** (`/_/admin/storage/buckets`). A bucket that this instance refuses to write carries the warning **Writes refused on this instance: encryption differs from the cluster.** Click **Open the encryption witness** in the warning.

   ![The Buckets page; a box marks the warning Writes refused on this instance on the db-archive card, and callout 1 marks its link Open the encryption witness.](/_/screenshots/bucket-encryption-witness-chip.webp)

2. The link opens the **Encryption witness** card on **System → System** (`/_/admin/system`). The card shows the state of this instance: **Agree**, **Mismatch**, **Unknown** (the sync bucket did not answer) or **Unavailable** (this instance has no client for the sync bucket). For each mismatch, it shows the bucket, the backend, the encryption of this instance, the encryption that the witness holds, and the instance that published it. A row for **All buckets without their own setting** stands for the buckets of the backend that have no entry of their own. Check that this instance has the encryption that you want for the bucket, and click **Publish this instance's setting** in its row. **Publish all** publishes every row at once.

   ![The Encryption witness card on the System page with the db-archive mismatch; callout 2 marks Publish this instance's setting.](/_/screenshots/bucket-encryption-witness-publish.webp)

   When this instance stores the bucket in plaintext, the publish is a downgrade for every instance. The page asks you to confirm with **Publish plaintext**, and the request carries the header `X-DGP-Confirm-Encryption-Downgrade`.

After the publish, the instances that have the same encryption write the bucket again. An instance that still has the old config refuses writes until you give it the new config. The card is not on the page of a single instance, because a single instance has no witness. To publish from the command line (`deltaglider_proxy witness publish`), and for the rules of the witness, see [How to run multiple instances](run-multiple-instances.md#8-keep-the-encryption-of-each-bucket-the-same-on-every-instance).

### Other proxies on the same backend

A proxy with its own configuration, which shares no sync bucket with these instances, has no witness. When it has the credentials of the backend but not the key of the bucket, it refuses to write the bucket with `400 InvalidRequest`, because the bucket holds a key claim and encrypted objects of a key that it does not have. To let its users write the bucket, create IAM users for them on these instances instead of giving the key to the other proxy. See [Buckets that another proxy encrypts](../reference/encryption.md#buckets-that-another-proxy-encrypts).

## Verify

1. On **Storage → Buckets**, check the encryption chip of each row. `db-archive` shows **Encrypted · bucket key db-archive-2026-10**. The `downloads` row has no encryption chip, because a bucket that stores plaintext like its backend shows no chip. A bucket that takes the encryption of an encrypting backend shows **Encrypted · from hetzner-fsn1**.

   ![The Buckets page; a box marks the chip Encrypted · bucket key db-archive-2026-10 of db-archive. The downloads row has no encryption chip, because it stores plaintext as its backend does.](/_/screenshots/bucket-encryption-chip.webp)

2. The admin API reports the same answer for each bucket (see [the admin API reference](../reference/admin-api.md)):

   ```bash
   curl -s -b cookies.txt https://s3.acme.example/_/api/admin/buckets \
     | jq '.buckets[] | {name, effective_encryption}'
   ```

   `db-archive` has `"source": "bucket"` and `"key_id": "db-archive-2026-10"`; `downloads` has `"source": "backend"` and `"mode": "none"`.

3. Write an object through the proxy, and look at the stored copy on the backend, without the proxy. The user metadata carries `dg-encrypted: aes-256-gcm-chunked-v1` and a `dg-sealed` value that starts with `2.`. The proxy does not store the key id: the `dg-sealed` value starts with the fingerprint of the bucket key, and the [encryption reference](../reference/encryption.md#key-ids) shows how to compare it with your key:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 cp nightly.dump s3://db-archive/nightly/2026-10-14.dump
   aws s3api head-object --bucket db-archive --key nightly/2026-10-14.dump --profile hetzner
   ```

   An object in `downloads` has no `dg-encrypted` marker and no `dg-sealed` value.

4. Read the object back through the proxy, and compare its hash with the original file. Clients notice nothing.
5. Check that `db-archive` no longer serves unencrypted objects: the box **Unencrypted objects are served** is gone from its **Encryption** group, and `GET /_/api/admin/buckets` shows `"serves_plaintext": false` in its `effective_encryption`. Then read an old object through the proxy again.

## Related

- [How to encrypt data at rest](encrypt-data-at-rest.md): encryption for every bucket of a backend, and the native SSE modes.
- [How to rotate or change encryption keys](rotate-encryption-keys.md): rotation recipes and the legacy slot.
- [Encryption reference](../reference/encryption.md#per-bucket-encryption): the rules of a bucket's `encryption` block.
- [How to move a bucket to another backend](move-a-bucket-between-backends.md): a bucket's own block moves with it.
