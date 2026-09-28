# How to route a bucket to a different backend

This guide shows you how to serve a bucket from a specific storage backend, and how to map the bucket name that clients use onto a different real bucket upstream. Routing is pure configuration, so you can do it in the admin UI or in the YAML file. For how multi-backend routing works internally, see [the multi-backend architecture](../explanation/multi-backend-architecture.md).

The example registers three backends: `hetzner-fsn1` (S3-compatible storage at Hetzner, the default), `local-disk` (a directory on the proxy host) and `aws-dr` (AWS). It then routes `downloads` to `local-disk`, and it keeps `releases` on `hetzner-fsn1` under the real bucket name `acme-prod-releases-fsn1`.

## 1. Declare the backends

In the admin UI:

1. In the sidebar, open **Storage → Backends** (`/_/admin/storage/backends`).
2. Click **Add Backend** below the list of backends.

   ![The Backends page with callout 1 at the Backends entry of the sidebar and callout 2 at the Add Backend button below the backend list.](/_/screenshots/route-bucket-add-backend.webp)

3. In the **New Backend** form, type `hetzner-fsn1` in **Name** and select **S3** in **Type**. Fill in **Endpoint** (`https://fsn1.your-objectstorage.com`), **Region** (`fsn1`), **Access Key ID** and **Secret Access Key**. Select **Force path-style URLs**, because Hetzner is not AWS, and select **Set as default backend**.
4. Click **Create Backend**.

   ![The New Backend form filled in for hetzner-fsn1, with callout 3 on a box around the S3 fields and callout 4 at the Create Backend button.](/_/screenshots/route-bucket-backend-form.webp)

Before it saves an S3 backend, the page tests the connection with the keys that you typed. If the test fails, the page does not create the backend and shows the error. The server then probes the new backend again, and it refuses a backend that fails its probe. A backend that passes is saved at once and written to the config file, so the Backends page has no **Review & apply** step.

Repeat steps 2 to 4 for the other two backends:

- For `local-disk`, select **Filesystem** in **Type** and type `/var/lib/dgp-local` in **Data Directory**. The admin UI and `POST /_/api/admin/backends` refuse a filesystem backend whose path is not absolute. A relative path resolves against the working directory of the proxy process, and that directory is different under systemd, Docker and a shell.
- For `aws-dr`, select **S3**, leave **Endpoint** empty, type `eu-west-1` in **Region**, and leave **Force path-style URLs** off, because AWS itself needs `false`. Every non-AWS provider (Hetzner, MinIO, Backblaze, Wasabi) needs it on.

The form needs both an access key and a secret key for an S3 backend. Two options work only in YAML (see below): on AWS you can omit both keys and let the SDK pick up instance credentials, and you can keep a secret out of the file with a `${env:...}` reference.

## 2. Route the bucket

In the admin UI:

1. In the sidebar, open **Storage → Buckets** (`/_/admin/storage/buckets`). Each bucket is a row that shows its backend, its public access and its quota.
2. Click the `downloads` row to open it.

   ![The Buckets page with callout 1 at the Buckets entry of the sidebar and callout 2 at the downloads row.](/_/screenshots/route-bucket-buckets-row.webp)

3. In **Backend**, select `local-disk`. The option **Default** means that the bucket follows `default_backend`. If the bucket already had an explicit backend, a dialog reminds you that routing does not move objects; click **Re-route anyway** to continue.

   ![The open downloads row with its Backend list expanded and an arrow at the local-disk option.](/_/screenshots/route-bucket-backend-select.webp)

4. Click **Review & apply** in the bar at the bottom of the page.

   ![The bar at the bottom of the Buckets page reports unsaved changes, and an arrow points at its Review & apply button.](/_/screenshots/route-bucket-review-apply.webp)

5. Check the diff in the dialog, and then click **Apply and Persist**.

   ![The review dialog shows the storage diff that routes downloads to local-disk, and an arrow points at the Apply and Persist button.](/_/screenshots/route-bucket-apply-dialog.webp)

If the bucket does not exist yet, open the menu next to **Create bucket** and select **Add settings for a bucket that does not exist yet**. Type the bucket name, and continue with step 3. Any bucket without an explicit backend goes to `default_backend`.

## 3. Alias an upstream bucket name

If the real bucket on the backend has a different name from the one that clients should see, give the bucket an alias. In the admin UI:

1. On **Storage → Buckets**, click the `releases` row to open it.
2. Click **Advanced** inside the row.
3. Type `acme-prod-releases-fsn1` in **Real name on backend**.

   ![The open releases row with callout 1 at the row, callout 2 at the Advanced disclosure, and callout 3 at the Real name on backend field, which holds acme-prod-releases-fsn1.](/_/screenshots/route-bucket-alias.webp)

4. Click **Review & apply**, and then click **Apply and Persist**, as in the previous section.

Clients send requests to `s3://releases/firmware/widget-3000/fw-1.4.3.tar`, and the proxy translates them to `s3://acme-prod-releases-fsn1/firmware/widget-3000/fw-1.4.3.tar` on Hetzner.

Aliasing is useful when:

- You're moving buckets between backends without updating clients.
- The upstream name carries a prefix you don't want to expose (`acme-prod-releases-fsn1` vs `releases`).
- You want two logical namespaces in one physical bucket (two aliases that point at the same real bucket). Avoid this unless you also scope access by prefix with IAM.

## The same change in YAML

The steps above write this configuration into the `storage` section of `deltaglider_proxy.yaml` ([why the UI and the file hold the same configuration](../explanation/two-ways-to-configure.md)):

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
    - name: local-disk
      type: filesystem
      path: /var/lib/dgp-local
    - name: aws-dr
      type: s3
      region: eu-west-1
  buckets:
    downloads:
      backend: local-disk              # local filesystem
    releases:                          # name clients use
      backend: hetzner-fsn1
      alias: acme-prod-releases-fsn1        # real bucket on the backend
    # every other bucket goes to default_backend
```

Each S3 backend takes its credentials in `access_key_id` and `secret_access_key`. Keep the secret out of the file with a `${env:...}` reference:

```yaml
# fragment
      access_key_id: "${env:HETZNER_S3_KEY}"
      secret_access_key: "${env:HETZNER_S3_SECRET}"
```

On AWS you can omit both and let the SDK pick up instance credentials. The complete field list is in the [configuration reference](../reference/configuration.md).

After you edit the file, restart the proxy, or apply the file to the running proxy with `POST /_/api/admin/config/apply` or with `deltaglider_proxy config apply deltaglider_proxy.yaml --server https://s3.acme.example`.

## Buckets that the proxy creates at startup

The proxy creates each bucket declared under `storage.buckets` at startup, on the backend that its route points at, so the first write works without a `CreateBucket` step. On a filesystem backend, the proxy creates the bucket's directory. On an S3 backend, the proxy first sends a `HeadBucket`, and it sends a `CreateBucket` only when the backend does not have the bucket. When the backend's key may not create buckets, the proxy logs a warning and starts anyway. In that case, create the bucket yourself, for example through the proxy with `aws s3 mb s3://downloads --endpoint-url https://s3.acme.example`. Set `DGP_BOOT_CREATE_DECLARED_BUCKETS=false` to turn the startup creation off for every backend. A route that you add at runtime (with an apply, from the UI or from the file) does not create the bucket until the next start. If a client creates a bucket that has no `storage.buckets` entry at all, it lands on `default_backend`.

## What routing does not do

Routing never moves data. When you point an existing bucket at a new backend, the proxy looks for its objects **on the new backend**. Objects that are already stored on the old backend become invisible until you move them. To relocate data, use the built-in migrate job instead: [How to move a bucket to another backend](move-a-bucket-between-backends.md). On the Buckets page, the **Migrate data…** link next to **Backend** starts it.

## Verify

1. Check that the proxy applied the config:

   ```bash
   curl -b cookies https://s3.acme.example/_/api/admin/config/section/storage?format=yaml
   ```

2. Check that a SigV4 client sees the bucket:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 ls
   ```

3. Check that a round-trip works through the alias:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 cp test.txt s3://releases/test.txt
   aws --endpoint-url https://s3.acme.example s3 cp s3://releases/test.txt -
   ```

4. Check that the object landed on the right backend. For a filesystem backend, check the path directly. For S3, list the real (aliased) bucket on the provider:

   ```bash
   ls /var/lib/dgp-local/downloads/                # filesystem backend
   aws s3 ls s3://acme-prod-releases-fsn1/ --profile hetzner     # S3 backend, raw
   ```

If an object goes to the wrong backend, see [Troubleshooting](troubleshooting.md).

## Related

- [How to move a bucket to another backend](move-a-bucket-between-backends.md): relocate the data.
- [How to migrate an existing S3 bucket into the proxy](migrate-existing-data-into-the-proxy.md): adopt a pre-existing upstream bucket.
- [Configuration reference](../reference/configuration.md): every `storage.backends` and `storage.buckets` field.
- [The multi-backend architecture](../explanation/multi-backend-architecture.md): how virtual-bucket routing works.
- [Two ways to configure DeltaGlider](../explanation/two-ways-to-configure.md): how the admin UI and the YAML file relate.
