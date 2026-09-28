# How to encrypt data at rest

This guide shows you how to turn on at-rest encryption on a storage backend. Encryption belongs to a backend, so every bucket that routes to the backend inherits it. For the threat model behind each mode, see [encryption at rest](../explanation/encryption-at-rest.md). The example encrypts `hetzner-fsn1`, which holds the buckets `releases` and `downloads`, and it shows SSE-KMS on `aws-dr`.

## 1. Pick a mode

The admin UI names the four modes in the **Encryption mode** list of each backend:

- **AES-256-GCM (proxy-side)** (`aes256-gcm-proxy`): the proxy encrypts the bytes before the backend sees them. Use it for a filesystem backend, or for an S3 provider that you do not fully trust with plaintext.
- **SSE-KMS (AWS KMS)** (`sse-kms`): AWS encrypts the objects with a KMS key. Use it on AWS S3 when you want an audit log for each decrypt and key management in KMS.
- **SSE-S3 (AWS-managed AES256)** (`sse-s3`): AWS encrypts the objects with its own keys. Use it on AWS S3 when that is enough.
- **None (plaintext)** (`none`): no encryption. Keep it for a bucket whose contents are public anyway, because there encryption only adds overhead.

The two SSE modes work only on S3 backends; the proxy refuses them on a filesystem backend. The full mode matrix, the field list and the wire format are in the [encryption reference](../reference/encryption.md).

**If you lose a proxy-side AES key, the encrypted objects on that backend are unrecoverable.** The proxy does not escrow keys, and there is no recovery path. Store every key off the proxy host, for example in a secrets manager or an operator vault.

## 2. Turn on proxy-side AES with a key from the admin UI

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

**Apply** sends a `storage` section update at once; it has no separate **Review & apply** step. New writes to `hetzner-fsn1` are encrypted from this moment. Section 5 covers the objects that the backend already holds.

The proxy stores a key from the admin UI **in the config file, in plain text**, as the `key` field of the backend. The proxy writes the file with no access for other users (mode `0600` for a new file), and it never returns the key: `GET /_/api/admin/config/export` and the section reads leave it out, and a full backup keeps it in `secrets.json`. Anyone who can read the config file can still read the key. To keep the key out of the file, use an environment variable instead (section 3).

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

## 3. Or keep the key in an environment variable

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

`key_id` is the name that the proxy stamps on every object that it encrypts. When you leave it out, the proxy derives an id from the backend name and the key. A readable id helps later, when you rotate the key ([How to rotate or change encryption keys](rotate-encryption-keys.md)). While the variable is set, the encryption editor of the backend in the admin UI is read-only, and a note names the variable.

## 4. Or use SSE-KMS or SSE-S3 on AWS

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

## 5. Encrypt the objects that the backend already holds

Encryption is **not retroactive**. The proxy encrypts only new writes, and the existing objects stay in their stored form. After an **Apply** that changes the proxy-side encryption of a backend, the Backends page proposes a job that rewrites these objects:

1. In the dialog **Encrypt existing objects?**, check the list of buckets on the backend.
2. Click **Start now**. The proxy starts one re-encrypt job for each selected bucket.

   ![After the apply, the dialog Encrypt existing objects? lists the buckets of hetzner-fsn1; the arrow points at Start now.](/_/screenshots/encrypt-reencrypt-proposal.webp)

If you click **Later**, start the job when you are ready from **Storage → Jobs** with **New job** → **Re-encrypt buckets… — one-off rewrite**. While the job of a bucket runs, writes to that bucket get `503 SlowDown`, and reads pass. The job survives restarts. [How to rotate or change encryption keys](rotate-encryption-keys.md) explains the mechanics. The re-encrypt job does not handle the SSE modes: on an SSE-KMS or SSE-S3 backend, use the migrate job ([recipe C](rotate-encryption-keys.md#recipe-c-migrate-from-proxy-aes-to-sse-kms)).

## Verify

1. Write an object and read it back through the proxy. Clients must notice nothing:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 cp release-notes.md s3://releases/reports/release-notes.md
   aws --endpoint-url https://s3.acme.example s3 cp s3://releases/reports/release-notes.md - | sha256sum
   ```

   The hash must match the hash of the original file.

2. Check that the stored object is ciphertext. Look at it on the raw backend, without the proxy. In proxy-side AES mode, the body starts with the `DGE1` magic (chunked) or is opaque GCM ciphertext. The user metadata on the backend carries the `dg-encrypted` marker (an `aes-256-gcm-*` value) and `dg-encryption-key-id`:

   ```bash
   aws s3api head-object --bucket releases --key reports/release-notes.md --profile hetzner
   ```

   On a filesystem backend, the markers live in the `user.dg.metadata` extended attribute of the file, for example `xattr -p user.dg.metadata /var/lib/dgp-local/db-archive/nightly/2026-06-10.dump`.

   The SSE modes stamp `dg-encrypted-native: sse-kms` (or `sse-s3`) instead. That marker is harmless to expose.

3. Check that the old plaintext objects still read correctly. The decrypt path acts on the marker, and an object without a marker is served as it is.
4. If you ran a re-encrypt job, check that its row on **Storage → Jobs** shows `succeeded`, and that an *old* object on the raw backend now carries the marker too.
5. Check that the key is stored somewhere safe **outside** the proxy host.

## Related

- [How to rotate or change encryption keys](rotate-encryption-keys.md): rotation recipes, the `legacy_key` shim, and the re-encrypt job in detail.
- [Encryption reference](../reference/encryption.md): modes, fields, environment variables, markers, wire format, and limits.
- [Encryption at rest](../explanation/encryption-at-rest.md): which mode fits which threat model, and why.
- [How to move a bucket to another backend](move-a-bucket-between-backends.md): migration as a path to re-encrypt.
- [Two ways to configure DeltaGlider](../explanation/two-ways-to-configure.md): how the admin UI and the YAML file relate.
