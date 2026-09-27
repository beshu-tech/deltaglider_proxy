# How to route a bucket to a different backend

This guide shows you how to serve a bucket from a specific storage backend, and how to map the bucket name clients use onto a different real bucket upstream. Routing is pure configuration. For how multi-backend routing works internally, see [the multi-backend architecture](../explanation/multi-backend-architecture.md).

## 1. Declare the backends

Name every backend you want to route to under `storage.backends`, and pick a default:

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
```

If the backend is a non-AWS provider (Hetzner, MinIO, Backblaze, Wasabi), set `force_path_style: true`. AWS itself needs `false`. Each backend takes its credentials in `access_key_id` / `secret_access_key`. Keep the secret out of the file with a `${env:...}` reference:

```yaml
      access_key_id: "${env:HETZNER_S3_KEY}"
      secret_access_key: "${env:HETZNER_S3_SECRET}"
```

On AWS you can omit both and let the SDK pick up instance credentials. The complete field list is in the [configuration reference](../reference/configuration.md).

In the admin UI, use **Settings → Storage → Backends → + Add backend**. The admin UI and `POST /_/api/admin/backends` refuse a filesystem backend whose `path` is not absolute, because a relative path resolves against the working directory of the proxy process, and that directory is different under systemd, Docker and a shell.

![Storage backends](/_/screenshots/storage_backends.jpg)

## 2. Route the bucket

Point the bucket at a named backend under `storage.buckets`. Any bucket without an explicit `backend` goes to `default_backend`:

```yaml
storage:
  buckets:
    releases:
      backend: hetzner-fsn1     # CI firmware lives on Hetzner
    downloads:
      backend: local-disk       # local filesystem
    # everything else → default_backend
```

In the admin UI, open **Settings → Storage → Buckets**. Each bucket is a row that shows its backend, origin, public access, and quota. Expand the row and pick the backend from the dropdown.

![Per-bucket storage form](/_/screenshots/config-storage-form.jpg)

The proxy creates each bucket declared under `storage.buckets` at startup, on the backend that its route points at, so the first write works without a `CreateBucket` step. On a filesystem backend, the proxy creates the bucket's directory. On an S3 backend, the proxy first sends a `HeadBucket`, and it sends a `CreateBucket` only when the backend does not have the bucket. When the backend's key may not create buckets, the proxy logs a warning and starts anyway. In that case, create the bucket yourself, for example through the proxy with `aws s3 mb s3://downloads --endpoint-url https://s3.acme.example`. Set `DGP_BOOT_CREATE_DECLARED_BUCKETS=false` to turn the startup creation off for every backend. A route that you add at runtime (with an apply) does not create the bucket until the next start. If a client creates a bucket that has no `storage.buckets` entry at all, it lands on `default_backend`.

## 3. Alias an upstream bucket name

If the real bucket on the backend is named differently from what clients should see, add an `alias`:

```yaml
storage:
  buckets:
    db-archive:                      # name clients use
      backend: hetzner-fsn1
      alias: acme-db-archive-prod    # real bucket on the backend
```

Clients send requests to `s3://db-archive/nightly/2026-06-11.dump`, and the proxy translates them to `s3://acme-db-archive-prod/nightly/2026-06-11.dump` on Hetzner.

Aliasing is useful when:

- You're moving buckets between backends without updating clients.
- The upstream name carries a prefix you don't want to expose (`acme-db-archive-prod` vs `db-archive`).
- You want two logical namespaces in one physical bucket (two aliases that point at the same real bucket). Avoid this unless you also scope access by prefix with IAM.

## 4. Apply the change

If you edit the YAML file directly, restart the proxy or apply it via `POST /_/api/admin/config/apply`. If you used the admin UI, the Buckets page's **Review & apply** bar does this for you.

## What routing does not do

Routing never moves data. When you point an existing bucket at a new backend, the proxy looks for its objects **on the new backend**. Objects that are already stored on the old backend become invisible until you move them. To relocate data, use the built-in migrate job instead: [How to move a bucket to another backend](move-a-bucket-between-backends.md).

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
   aws --endpoint-url https://s3.acme.example s3 cp test.txt s3://db-archive/test.txt
   aws --endpoint-url https://s3.acme.example s3 cp s3://db-archive/test.txt -
   ```

4. Check that the object landed on the right backend. For a filesystem backend, check the path directly. For S3, list the real (aliased) bucket on the provider:

   ```bash
   ls /var/lib/dgp-local/downloads/                # filesystem backend
   aws s3 ls s3://acme-db-archive-prod/ --profile hetzner   # S3 backend, raw
   ```

If an object goes to the wrong backend, see [Troubleshooting](troubleshooting.md).

## Related

- [How to move a bucket to another backend](move-a-bucket-between-backends.md): relocate the data.
- [How to migrate an existing S3 bucket into the proxy](migrate-existing-data-into-the-proxy.md): adopt a pre-existing upstream bucket.
- [Configuration reference](../reference/configuration.md): every `storage.backends` and `storage.buckets` field.
- [The multi-backend architecture](../explanation/multi-backend-architecture.md): how virtual-bucket routing works.
