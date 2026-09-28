# How to publish a folder publicly

*Serve one prefix to anyone, for example installers that people download with `curl` and no credentials. The rest of the bucket stays locked.*

This guide shows you how to make one prefix of a bucket readable without credentials. A public prefix is part of the bucket policy under `storage.buckets`, so you can set it in the admin UI or in the YAML file. The example publishes the installers under `public/` in the `downloads` bucket.

## 1. Mark the prefix public

In the admin UI:

1. In the sidebar, open **Storage → Buckets** (`/_/admin/storage/buckets`).
2. Click the `downloads` row to open it.

   ![The Buckets page shows one row per bucket with its backend; callout 1 marks Buckets in the sidebar and callout 2 marks the downloads row.](/_/screenshots/route-bucket-buckets-row.webp)

3. Under **Public access**, select **Specific prefixes public**. An empty prefix field appears below the option.
4. Type `public/` in the prefix field. To publish a second prefix, click **Add prefix** and type it in the new field.

   ![The downloads bucket is open; callout 3 marks the Specific prefixes public option and callout 4 marks the prefix field, which holds public/.](/_/screenshots/bucket-public-prefix.webp)

5. Click **Review & apply** in the bar at the bottom of the page. Check the diff in the dialog, and then click **Apply and Persist**. The dialog is the same as in [How to route a bucket to a different backend](route-a-bucket-to-a-backend.md#2-route-the-bucket).

The three options of **Public access** map directly to the YAML:

| Option | YAML | Effect |
|---|---|---|
| **Private (default)** | no key | Only authenticated requests. |
| **Specific prefixes public** | `public_prefixes: [...]` | Only the keys under these prefixes are readable without credentials. |
| **Entire bucket public** | `public: true` | Every object is readable and listable without credentials. |

The proxy applies the change without a restart. It creates a read-only request rule named `public-prefix:downloads` from the policy.

The proxy refuses to make the coordination bucket (`config_sync_bucket`) public, because that bucket holds the config sync, the leases and the locks of the proxy, and S3 clients may never reach it.

## The same change in YAML

The steps above write this configuration into the `storage` section of `deltaglider_proxy.yaml` ([why the UI and the file hold the same configuration](../explanation/two-ways-to-configure.md)):

```yaml
# validate
storage:
  buckets:
    downloads:
      public_prefixes:
        - public/
```

After you edit the file, restart the proxy, or apply the file to the running proxy with `POST /_/api/admin/config/apply` or with `deltaglider_proxy config apply deltaglider_proxy.yaml --server https://s3.acme.example`.

## What anonymous requests get

Anonymous requests can GET, HEAD and LIST under the prefix, and a LIST result stays inside the prefix. These requests run as the built-in `$anonymous` user, and they can never write. The full rules are in the [authentication reference](../reference/authentication.md#public-prefixes).

A response to an anonymous request carries no `x-amz-meta-dg-tool` header, because that metadata names the version of the proxy. The lockout after failed sign-ins does not block public reads: a client address that the proxy locked out still gets the objects under a public prefix.

## Mind the trailing slash

The prefix `public/` matches `public/installer.zip`, but it does **not** match `publicity/report.pdf`. The prefix `public`, without a slash, is a plain string-prefix match, so it exposes both. Always end the prefix with `/`, unless you want the string-prefix match on purpose.

## Whole-bucket public

`public: true` is a short form of `public_prefixes: [""]`. It makes every object in the bucket readable without credentials. In the admin UI, it is the option **Entire bucket public**. In YAML:

```yaml
# validate
storage:
  buckets:
    downloads:
      public: true
```

The proxy logs a warning at startup when a whole bucket is public. Use it only for buckets that contain nothing but published artifacts. Anything that someone uploads there later is public as soon as it arrives, and a LIST shows every key name in the bucket.

To share one object for a limited time, generate a presigned URL instead of publishing a prefix. The URL expires after at most 7 days, and you can revoke it when you rotate the key of the user who signed it:

```bash
aws --endpoint-url https://s3.acme.example \
  s3 presign s3://downloads/internal/draft-installer.zip --expires-in 3600
```

## Verify

Test from a shell with **no** AWS environment. A leftover `AWS_ACCESS_KEY_ID` would authenticate the request, so the test would prove nothing, because credentials always win over a public prefix:

```bash
env -i curl -sw "%{http_code}\n" -o /dev/null \
  https://s3.acme.example/downloads/public/installer-1.2.0.zip
# 200

env -i curl -sw "%{http_code}\n" -o /dev/null \
  https://s3.acme.example/downloads/internal/roadmap.pdf
# 403 — outside the public prefix

env -i curl -sw "%{http_code}\n" -o /dev/null -X PUT \
  https://s3.acme.example/downloads/public/evil.zip -d x
# 403 — anonymous writes are always denied
```

Each anonymous request writes a line with `action=public_read` and `user=$anonymous` to the proxy log. These lines do not appear in the audit log of the admin UI.

## Lock writes down

A public prefix does not change who can write to it. Review the write access separately:

- Scope the upload credentials to the prefix and pin them to your network: [How to restrict access by IP and prefix](restrict-access-with-conditions.md).
- Reject anonymous write attempts on the bucket before authentication runs: [How to gate requests before authentication](gate-requests-with-admission-rules.md).

## Related

- [Authentication reference](../reference/authentication.md#public-prefixes): the exact anonymous rules and how the proxy validates a prefix.
- [About authentication and access control](../explanation/security-model.md): why public prefixes are exceptions, not a type of credential.
- [How to gate requests before authentication](gate-requests-with-admission-rules.md): the `public-prefix:*` request rules, and how one deny rule takes a public folder offline.
- [How to create IAM users and groups](create-iam-users.md): credentials for everyone who is not anonymous.
- [Two ways to configure DeltaGlider](../explanation/two-ways-to-configure.md): how the admin UI and the YAML file relate.
