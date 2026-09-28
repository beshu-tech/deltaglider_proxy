# How to migrate an existing S3 bucket into the proxy

This guide shows you how to put an existing S3 bucket, with years of objects already in it, behind the proxy. There are two routes. Pick one by how much you care about compressing the historical objects.

- **If you want zero data movement**, point the proxy at the bucket in place. The proxy serves the existing objects untouched, and only new uploads get delta compression.
- **If you want the history compressed too**, copy the data through the proxy once, and then cut over.

The proxy compresses an object only when it writes it, so it cannot compress old objects afterwards. For the reason, see [delta compression](../explanation/delta-compression.md).

## Route 1: point at the bucket in place

Use this route when the existing data can stay where it is. In the example, the legacy AWS bucket `acme-firmware` becomes the proxy bucket `releases`, on the backend `aws-dr`.

### Declare the backend

In the admin UI:

1. In the sidebar, open **Storage → Backends** (`/_/admin/storage/backends`).
2. Click **Add Backend** below the list of backends.

   ![The Backends page lists the configured backends; callout 1 marks Backends in the sidebar and callout 2 marks the Add Backend button.](/_/screenshots/route-bucket-add-backend.webp)

3. In the **New Backend** form, type `aws-dr` in **Name** and select **S3** in **Type**. Leave **Endpoint** empty, type `eu-west-1` in **Region**, and leave **Force path-style URLs** off, because AWS needs `false`. Fill in **Access Key ID** and **Secret Access Key** with a key that can read and write `acme-firmware`.
4. Click **Create Backend**. The page tests the connection first, and it does not create a backend that fails the test.

[How to route a bucket to a different backend](route-a-bucket-to-a-backend.md#1-declare-the-backends) shows the same form for another backend.

### Add the bucket and its alias

The proxy does not list `releases` yet, because the real bucket on AWS has another name. So you add the settings for a bucket that the proxy does not see yet. In the admin UI:

1. In the sidebar, open **Storage → Buckets** (`/_/admin/storage/buckets`).
2. Click **More ways to add a bucket**, the arrow next to **Create bucket**.
3. Click **Add settings for a bucket that does not exist yet**. A new, open settings row appears at the end of the list.

   ![The Buckets page with the menu next to Create bucket open; callout 1 marks Buckets in the sidebar, callout 2 marks the menu button, and callout 3 marks Add settings for a bucket that does not exist yet.](/_/screenshots/adopt-bucket-draft.webp)

4. Type `releases` in **Bucket name**.
5. In **Backend**, select `aws-dr`.
6. Click **Advanced** inside the row.
7. Type `acme-firmware` in **Real name on backend**.

   ![A new settings row is open; callout 4 marks the Bucket name field, callout 5 marks the Backend list, set to aws-dr, callout 6 marks Advanced, and callout 7 marks Real name on backend, set to acme-firmware.](/_/screenshots/adopt-bucket-alias.webp)

8. Click **Review & apply** in the bar at the bottom of the page. Check the diff in the dialog, and then click **Apply and Persist**.

If you type the name of a bucket that the proxy already lists, the row does not take the name. The page opens the settings of that bucket in the list instead.

### Serve the existing objects

1. List the bucket through the proxy. Every existing object is already visible:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 ls s3://releases/firmware/widget-3000/
   ```

2. The proxy reads the existing objects as they are and serves them unchanged (passthrough). New uploads go through the delta router and start to save space at once, for example when `ci-uploader` uploads `firmware/widget-3000/fw-2.4.1.tar`.

3. Optional: give the existing objects the metadata of the proxy (a content hash and the creation time), so that the admin UI shows their checksum and the proxy can verify them. In the sidebar, open **Storage → Jobs** (`/_/admin/jobs`), click **New job**, and select **Backfill metadata… — for objects written without the proxy**. In the dialog **Backfill object metadata**, select `releases`, and then click **Start now (1 bucket)**. The job reads each object once and rewrites only its metadata. It keeps the Last-Modified time of each object, unless you select **Show backfilled objects as modified now**. While the job runs, uploads and deletes in the bucket get `503 SlowDown`, and S3 clients retry them. The details are in the [jobs reference](../reference/jobs.md#metadata-backfill).

**The limit of this route:** objects that entered the bucket before the proxy never get compressed later. The proxy delta-encodes an object only when it writes it. An object stored as-is stays as-is, unless something writes it again through the proxy. If historical savings matter, use route 2.

### The same change in YAML

The steps above write this configuration into the `storage` section of `deltaglider_proxy.yaml` ([why the UI and the file hold the same configuration](../explanation/two-ways-to-configure.md)):

```yaml
# validate
storage:
  default_backend: aws-dr
  backends:
    - name: aws-dr
      type: s3
      region: eu-west-1
      # access_key_id and secret_access_key: see below
  buckets:
    releases:
      backend: aws-dr
      alias: acme-firmware    # the pre-existing bucket, unchanged
```

The UI writes the keys that you typed into `access_key_id` and `secret_access_key` of the backend. In YAML you can keep them out of the file with `${env:...}` references (for example `secret_access_key: "${env:AWS_DR_SECRET}"`), or you can omit both keys on AWS and let the SDK pick up the instance credentials, as in the block above. The admin UI cannot do either. The first backend that you add becomes `default_backend`, unless you select **Set as default backend** on another one.

After you edit the file, restart the proxy, or apply the file to the running proxy with `POST /_/api/admin/config/apply` or with `deltaglider_proxy config apply deltaglider_proxy.yaml --server https://s3.acme.example`. The metadata backfill is a one-off job, not configuration, so it has no YAML. `POST /_/api/admin/jobs/backfill-metadata` starts it from a script.

## Route 2: copy through the proxy

Use this route when you want the version history itself stored as deltas. The proxy computes a delta for each object when it writes it, so a one-time copy through the proxy stores everything again, compressed.

1. Set up the destination bucket behind the proxy, and route it to the backend that holds the compressed copy. Do not set an alias, because this bucket is a new namespace. In the admin UI, open **Storage → Buckets**, open the `releases` row (or add its settings as in route 1), and select `hetzner-fsn1` in **Backend**. The steps with screenshots are in [How to route a bucket to a different backend](route-a-bucket-to-a-backend.md#2-route-the-bucket). In YAML:

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
       releases:
         backend: hetzner-fsn1
   ```

2. Copy the old bucket into the proxy. The read from the source uses your normal AWS credentials. The write goes to the proxy endpoint:

   ```bash
   aws s3 sync s3://acme-firmware /tmp/acme-firmware          # pull from AWS
   aws --endpoint-url https://s3.acme.example \
       s3 sync /tmp/acme-firmware s3://releases               # push through the proxy
   ```

   If one host can reach both sides and has enough disk, you can copy from bucket to bucket with any S3 tool (`rclone copy` works too). What matters is that the **writes go to the proxy endpoint**, so that each object passes through the delta router.

3. The upload order matters for the ratios. The first object in each prefix becomes the reference baseline, and the later versions are stored as deltas against it. `aws s3 sync` copies in key order, which for versioned names (`fw-2.3.0.tar`, `fw-2.4.0.tar`, and so on) is usually also the version order. This is good enough in practice.

4. Check the savings on the stats endpoint before you cut over. The endpoint shows the size of every bucket, so it answers only a request with an admin session, and any other request gets `401`. Sign in first to store the session cookie:

   ```bash
   curl -s -c /tmp/dgp.cookies -X POST https://s3.acme.example/_/api/admin/login \
     -H 'Content-Type: application/json' -d '{"password": "<bootstrap-password>"}'
   curl -s -b /tmp/dgp.cookies https://s3.acme.example/_/stats
   # per bucket, with the running counter:
   curl -s -b /tmp/dgp.cookies 'https://s3.acme.example/_/stats?bucket=releases'
   ```

## Cut clients over

Both routes end the same way: you change the endpoint.

1. Point the clients at the proxy: change `--endpoint-url` (or the `endpoint_url` of the SDK) from the URL of the provider to the URL of the proxy. Bucket names and key paths do not change: route 1 keeps them through the alias, and route 2 copies them unchanged.
2. Give the clients proxy credentials. Clients now sign with the SigV4 credentials of the proxy, not with those of the provider. Create an IAM user for each client, for example `ci-uploader` with write access to `releases/*`, in **Access → Users** (`/_/admin/access/users`). The steps are in [How to create IAM users and groups](create-iam-users.md).
3. If you used route 2, freeze or retire the old bucket after the traffic moved, so that nothing writes around the proxy.

Do not keep writing to the backend bucket directly, for example with the Python DeltaGlider CLI or with raw AWS credentials. A write with a plain S3 client is not compressed. A tool that writes to the backend directly also does not encrypt with the key of the proxy, so on a backend that the proxy encrypts, such a write is stored in plaintext. Make the proxy the only write path.

## Verify

1. Read an old object through the proxy:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 cp s3://releases/firmware/widget-3000/fw-2.3.0.tar - | sha256sum
   ```

   The hash must match the original, because the proxy returns the exact bytes.

2. Upload a new version, and check that the proxy stored it as a delta. Check the `x-amz-storage-type` header of a GET or HEAD response: `delta` means compressed, and `passthrough` means stored as-is. The proxy sends this header only when it runs with `DGP_DEBUG_HEADERS=true`. Without it, the `x-amz-meta-dg-note` header carries the same value.

3. Watch the savings grow on the dashboard at **Observability → Dashboard** (`/_/admin/dashboard`). Its **Analytics** view shows the savings for each bucket. The same counters are also available to Prometheus at `/_/metrics`.

## Related

- [How to route a bucket to a different backend](route-a-bucket-to-a-backend.md): aliases and routing in full.
- [Configuration reference](../reference/configuration.md): the `storage.backends` and `storage.buckets` (alias) fields used here.
- [How to set per-bucket compression and quotas](set-bucket-compression-and-quotas.md): tune what gets compressed.
- [Your first delta savings](../tutorials/first-delta-savings.md): see the compression pipeline end to end.
- [Delta compression](../explanation/delta-compression.md): why compression happens only at write time.
- [Two ways to configure DeltaGlider](../explanation/two-ways-to-configure.md): how the admin UI and the YAML file relate.
