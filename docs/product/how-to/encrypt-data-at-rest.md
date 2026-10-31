# How to encrypt data at rest

This guide shows you how to turn on at-rest encryption on a storage backend. The encryption of a backend is the default for its buckets: every bucket on the backend uses it, unless the bucket has its own `encryption` block. To encrypt only some buckets of a backend, give those buckets their own key instead, as [How to encrypt one bucket on a shared backend](encrypt-one-bucket-on-a-shared-backend.md) shows. For the threat model behind each mode, see [encryption at rest](../explanation/encryption-at-rest.md). The example encrypts `hetzner-fsn1`, which holds the buckets `releases` and `downloads`, and it shows SSE-KMS on `aws-dr`.

## 1. Choose where the encryption goes

- **On the backend** (this guide): every bucket on the backend that has no block of its own is encrypted. The Backends page shows, on each backend card, how many buckets take its setting and how many set their own, for example "Applies to 2 buckets · 1 bucket has its own setting".
- **On one bucket** ([How to encrypt one bucket on a shared backend](encrypt-one-bucket-on-a-shared-backend.md)): only that bucket is encrypted, with a key that only it uses. Choose this when one storage account holds both data that must be encrypted and data that must stay plaintext, for example a replication target that an external reader expects in plaintext.

The bucket list on **Storage → Buckets** shows where each bucket's encryption comes from: **Encrypted · from hetzner-fsn1** for a bucket that takes the backend's encryption, and **Encrypted · bucket key db-archive-2026-10** for a bucket with its own key.

## 2. Pick a mode

The admin UI names the four modes in the **Encryption mode** list of each backend:

- **AES-256-GCM (proxy-side)** (`aes256-gcm-proxy`): the proxy encrypts the bytes before the backend sees them. Use it for a filesystem backend, or for an S3 provider that you do not fully trust with plaintext.
- **SSE-KMS (AWS KMS)** (`sse-kms`): AWS encrypts the objects with a KMS key. Use it on AWS S3 when you want an audit log for each decrypt and key management in KMS.
- **SSE-S3 (AWS-managed AES256)** (`sse-s3`): AWS encrypts the objects with its own keys. Use it on AWS S3 when that is enough.
- **None (plaintext)** (`none`): no encryption. Keep it for a bucket whose contents are public anyway, because there encryption only adds overhead.

The two SSE modes work only on S3 backends; the proxy refuses them on a filesystem backend. The full mode matrix, the field list and the wire format are in the [encryption reference](../reference/encryption.md).

**If you lose a proxy-side AES key, the encrypted objects on that backend are unrecoverable.** A full backup (the **Backup** card on the **System** page) includes a key that is in the config file, but it does not include a key that an environment variable supplies, and a backup on the same host is lost with the host. Store every key off the proxy host as well, for example in a secrets manager or an operator vault.

## 3. Turn on proxy-side AES with a key from the admin UI

In the admin UI:

1. In the sidebar, open **Storage → Backends** (`/_/admin/storage/backends`).
2. On the card of `hetzner-fsn1`, open the **Encryption mode** list.
3. Select **AES-256-GCM (proxy-side)**.

   ![The Backends page with the Encryption mode list of hetzner-fsn1 open; callout 1 marks Backends in the sidebar, callout 2 marks the Encryption mode list, and callout 3 marks the AES-256-GCM (proxy-side) option.](/_/screenshots/encrypt-mode-select.webp)

   The browser generates a random 256-bit key (`crypto.getRandomValues`) and shows it once, under **Generated key (64 hex chars, shown ONCE)**. The key does not go to the server before you click **Apply**.

4. Click **Copy to clipboard**, and store the key in your secrets manager.
5. Select the checkbox **I have stored this key safely**. **Apply** stays disabled until you select it.
6. Click **Apply**.

   ![The encryption editor of hetzner-fsn1 holds a generated key, which is hidden here; callout 4 marks Copy to clipboard, callout 5 marks the I have stored this key safely checkbox, and callout 6 marks Apply.](/_/screenshots/encrypt-key.webp)

7. When the buckets of `hetzner-fsn1` already hold objects, the dialog **Encrypt backend hetzner-fsn1?** opens. It says how many objects the buckets hold that are not encrypted yet, and that a re-encrypt job encrypts them in the background while they stay readable. For a bucket whose backend keeps old versions, the dialog also has the box **Also delete the old unencrypted versions**. Leave it clear, unless the old plaintext versions must go: the job then deletes them for good, and they cannot be restored ([details](../reference/encryption.md#old-versions-on-a-versioned-backend)). Click **Encrypt**.

   ![The dialog Encrypt backend hetzner-fsn1? says that 1,241 objects in releases and downloads are not encrypted yet and stay readable while a re-encrypt job encrypts them, and warns that the backend keeps old versions of releases; a box marks the unchecked Also delete the old unencrypted versions, and the arrow points at Encrypt.](/_/screenshots/encrypt-conversion-dialog.webp)

**Apply** (or **Encrypt** in the dialog) sends a `storage` section update at once; it has no separate **Review & apply** step. New writes to the buckets of `hetzner-fsn1` that have no encryption block of their own are encrypted from this moment. A bucket with its own block keeps its own setting.

The objects that the buckets already hold are still plaintext. In a bucket that encrypts, the proxy refuses a plaintext object, because it cannot tell the history of the bucket from an object that someone planted on the backend ([why](../explanation/encryption-at-rest.md#why-enabling-isnt-retroactive)). The apply therefore opens a *conversion window* for each bucket that holds objects, and it starts a re-encrypt job for each of these buckets. While the window is open, clients read the objects that the backend stored before the apply, and the job encrypts them. When the job converted every object, it closes the window. [Section 6](#6-encrypt-the-objects-that-the-backend-already-holds) shows how to follow the jobs.

The proxy stores a key from the admin UI **in the config file, in plain text**, as the `key` field of the backend. The proxy writes the file with no access for other users (mode `0600` for a new file), and it never returns the key: `GET /_/api/admin/config/export` and the section reads leave it out, and a full backup keeps it in `secrets.json`. Anyone who can read the config file can still read the key. To keep the key out of the file, use an environment variable instead (section 4).

## The same change in YAML

The steps above write this configuration into the `storage` section of `deltaglider_proxy.yaml` ([why the UI and the file hold the same configuration](../explanation/two-ways-to-configure.md)). The key here is an example value; the file holds the key that the browser generated:

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
        key: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef   # 64 hex characters
```

After you edit the file, restart the proxy, or apply the file to the running proxy with `POST /_/api/admin/config/apply` or with `deltaglider_proxy config apply deltaglider_proxy.yaml --server https://s3.acme.example`.

An apply of this file through `POST /_/api/admin/config/apply` or `config apply` does what the admin UI does: it opens the conversion window of each bucket that holds objects and starts its re-encrypt job. The response lists them in `conversions` ([admin API](../reference/admin-api.md#starting-a-conversion)). To make the jobs delete the old plaintext versions on a versioned backend, send the header `X-DGP-Purge-Plaintext-Versions: true` with `POST /_/api/admin/config/apply`. The apply is refused with `409` while another job, for example a migration, is active on one of the buckets.

A restart from an edited file starts the conversion too, when no proxy encrypted the bucket before. After it starts, the proxy checks the first 100 objects of each bucket that encrypts. When it finds an object without encryption in a bucket that holds no key claim and no conversion marker, it opens the window of the bucket and queues the re-encrypt job, and the log names both ([A conversion that a restart starts](../reference/encryption.md#a-conversion-that-a-restart-starts)). The check runs in the background, so reads of old objects can fail for the first seconds after the start. For any other bucket, the proxy logs a warning instead, and every read of an old object fails until you open the conversion yourself:

```bash
curl -b cookies.txt -X POST https://s3.acme.example/_/api/admin/jobs/reencrypt \
  -H 'Content-Type: application/json' \
  -d '{"buckets": ["releases", "downloads"], "open_window": true}'
```

The warning names the bucket, the object, the reason, and this request. Apply the file to the running proxy instead of a restart when you can, so that the old objects stay readable without a gap.

## 4. Or keep the key in an environment variable

The admin UI cannot set an environment variable, so you set this up outside the proxy and in `deltaglider_proxy.yaml`. The proxy derives the name of the variable from the name of the backend: `DGP_BACKEND_<NAME>_ENCRYPTION_KEY`, where `<NAME>` is the backend name in upper case with `-` and `.` replaced by `_`. For the singleton `storage.backend`, the variable is `DGP_ENCRYPTION_KEY`.

1. Generate a key, and store it in your secrets manager:

   ```bash
   openssl rand -hex 32
   ```

2. Put the key in the environment of the proxy:

   ```bash
   export DGP_BACKEND_HETZNER_FSN1_ENCRYPTION_KEY=<the key from step 1>
   ```

3. Set the mode in the config file, with no `key` field (the block below).
4. Restart the proxy, so that it reads the variable and the file together.

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
        key_id: hetzner-2026-06    # optional: a stable name for this key generation
```

After the restart, the proxy starts the conversion of each bucket that holds plaintext objects and was never encrypted, as the section above describes. For a bucket that it does not convert, the log has a warning: open its conversion with the `open_window` request of the section above, because until then every read of an old object fails. `key_id` is the name under which the admin UI, the audit log and the jobs show the key. The objects store only a fingerprint of the key, not the id. When you leave `key_id` out, the id is `fp-` and the 16 hexadecimal characters of that fingerprint. A readable id helps later, when you rotate the key ([How to rotate or change encryption keys](rotate-encryption-keys.md)). While the variable is set, the encryption editor of the backend in the admin UI is read-only, and a note names the variable.

## 5. Or use SSE-KMS or SSE-S3 on AWS

In SSE mode, the proxy never handles key material: AWS does the cryptography. In the admin UI:

1. On **Storage → Backends**, open the **Encryption mode** list on the card of `aws-dr`, and select **SSE-KMS (AWS KMS)**.
2. In **KMS key ARN or alias**, type the ARN of the key, for example `arn:aws:kms:eu-west-1:123456789012:key/abcd-ef01`.
3. Leave **Enable S3 bucket keys (reduces KMS cost on bursty traffic)** selected.

   ![The encryption editor of aws-dr in SSE-KMS mode; callout 1 marks the KMS key ARN or alias field and callout 2 marks the S3 bucket keys checkbox.](/_/screenshots/encrypt-kms.webp)

4. Click **Apply**.

For SSE-S3, select **SSE-S3 (AWS-managed AES256)** in step 1, and click **Apply**. It needs no other setting.

These steps write this configuration into the `storage` section:

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
        bucket_key_enabled: true   # reduces the KMS cost per request
```

For SSE-S3, the `encryption` block has only the mode:

```yaml
# validate
storage:
  default_backend: aws-dr
  backends:
    - name: aws-dr
      type: s3
      region: eu-west-1
      encryption:
        mode: sse-s3
```

The environment variable `DGP_BACKEND_<NAME>_SSE_KMS_KEY_ID` overrides `kms_key_id`.

## 6. Encrypt the objects that the backend already holds

Encryption is **not retroactive**. The proxy encrypts only new writes, and the existing objects stay plaintext until a re-encrypt job rewrites them. When the apply of section 3 found objects in a bucket, it already started that job, and the conversion window keeps the old objects readable while the job runs. You follow the jobs, and you act only when a job stops.

In the admin UI:

1. In the sidebar, open **Storage → Backends** (`/_/admin/storage/backends`). Under the counts of `hetzner-fsn1`, the card shows one line for each bucket that the job converts, for example "Encrypting the existing objects, 42%." The line of a bucket goes away when its job converted every object and closed the window.
2. To see the job of a bucket, click **View run** on its line. The job opens on **Storage → Jobs**. While the job runs, writes to the bucket get `503 SlowDown` on this instance, and reads pass. The job survives restarts. With several instances, the other instances still accept writes to the bucket, and the job can overwrite such a write: stop client writes to the bucket on every instance, or send them to this instance, until the job ends ([A conversion on several instances](../reference/encryption.md#a-conversion-on-several-instances)).
3. When a line says that the encryption of the existing objects stopped with failures, open the **Failures** tab of the run, fix the cause, and click **Resume** on the line. The window stays open until the job converted every object, and **Resume** gives it a new deadline of 7 days.
4. When a line names objects stored after the cutoff, click it. The run's **Failures** tab lists these objects. The job converted them, but clients could not read them before, because the backend stored them after the conversion started. Check that each of them is an object that you expect, for example an upload from an instance that did not encrypt yet. An object that nobody wrote through the proxy can be an object that someone planted on the backend.

The same lines show on **Storage → Buckets**, in the state line of each bucket. When a line says that the conversion stopped before it finished, the window passed its deadline: reads of the objects that are not encrypted yet fail. Click **Resume** to open the window again and to continue the job. A resume keeps the original cutoff, so it never serves an object that the backend stored after the first apply.

The re-encrypt job does not handle the SSE modes: on an SSE-KMS or SSE-S3 backend, use the migrate job ([recipe C](rotate-encryption-keys.md#recipe-c-migrate-from-proxy-aes-to-sse-kms)). [How to rotate or change encryption keys](rotate-encryption-keys.md) explains the mechanics of the job.

### The same steps with the admin API

The conversion is not configuration, so it has no YAML. The bucket list shows its state in `effective_encryption.conversion` ([admin API](../reference/admin-api.md#backends)):

```bash
curl -b cookies.txt https://s3.acme.example/_/api/admin/buckets
```

**Resume** is a new job for the bucket with `"open_window": true`. The request keeps the window of the bucket, with its run id and its cutoff, and gives the run id to the job. The job of that run then extends the deadline, and it closes the window when it converted every object. A job without `open_window` has no run id, so it neither extends nor closes a window:

```bash
curl -b cookies.txt -X POST https://s3.acme.example/_/api/admin/jobs/reencrypt \
  -H 'Content-Type: application/json' -d '{"buckets": ["releases"], "open_window": true}'
```

## Verify

1. On **Storage → Buckets**, check that the buckets of the backend show **Encrypted · from hetzner-fsn1**. The same answer is in the `effective_encryption` field of `GET /_/api/admin/buckets` (see [the admin API reference](../reference/admin-api.md)).
2. Write an object and read it back through the proxy. Clients must notice nothing:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 cp release-notes.md s3://releases/reports/release-notes.md
   aws --endpoint-url https://s3.acme.example s3 cp s3://releases/reports/release-notes.md - | sha256sum
   ```

   The hash must match the hash of the original file.

3. Check that the stored object is ciphertext. Look at it on the raw backend, without the proxy. In proxy-side AES mode, the body starts with the 4 characters `DGE2`. The user metadata on the backend carries `dg-encrypted: aes-256-gcm-chunked-v1` and a `dg-sealed` value that starts with `2.`. It holds no `dg-md5`, and its `dg-file-sha256` starts with `sealed:` instead of a hash:

   ```bash
   aws s3api head-object --bucket releases --key reports/release-notes.md --profile hetzner
   ```

   On a filesystem backend, the markers live in the `user.dg.metadata` extended attribute of the file, for example `xattr -p user.dg.metadata /var/lib/dgp-local/db-archive/nightly/2026-06-10.dump`.

   The value `aes-256-gcm-chunked-v1` is the name of an older format on purpose, so that an older release refuses the object. The proxy does not store the key id. The `dg-sealed` value starts with the fingerprint of the key, and the [encryption reference](../reference/encryption.md#key-ids) shows how to read it and compare it with the fingerprint of your key.

   The SSE modes stamp `dg-encrypted-native: sse-kms` (or `sse-s3`) instead. That marker is harmless to expose.

4. If the buckets held objects (section 6), check that each re-encrypt job on **Storage → Jobs** shows `succeeded`, and that an *old* object on the raw backend now carries `dg-sealed` too.
5. Check that the conversions are closed: the card of `hetzner-fsn1` shows no conversion line, and `GET /_/api/admin/buckets` shows no `conversion` in the `effective_encryption` of `releases` and `downloads`. Then read an old object through the proxy. It must read without an error. An error that says that the object "is not encrypted, but the bucket encrypts" means that the job did not convert it ([troubleshooting](troubleshooting.md#encryption-at-rest-symptoms-and-fixes)).
6. Check that the key is stored somewhere safe **outside** the proxy host.

## Related

- [How to encrypt one bucket on a shared backend](encrypt-one-bucket-on-a-shared-backend.md): a key for one bucket, while the other buckets on the backend stay as they are.
- [How to rotate or change encryption keys](rotate-encryption-keys.md): rotation recipes, the `legacy_key` shim, and the re-encrypt job in detail.
- [Encryption reference](../reference/encryption.md): modes, fields, environment variables, markers, wire format, and limits.
- [Encryption at rest](../explanation/encryption-at-rest.md): which mode fits which threat model, and why.
- [How to move a bucket to another backend](move-a-bucket-between-backends.md): migration as a path to re-encrypt.
- [Two ways to configure DeltaGlider](../explanation/two-ways-to-configure.md): how the admin UI and the YAML file relate.
