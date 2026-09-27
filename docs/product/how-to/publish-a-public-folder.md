# How to publish a folder publicly

*Serve one prefix to anyone, for example installers that people download with `curl` and no credentials. The rest of the bucket stays locked.*

## 1. Mark the prefix public

Acme publishes installers from `public/` in the `downloads` bucket. In YAML:

```yaml
# validate
storage:
  buckets:
    downloads:
      public_prefixes:
        - public/
```

Or in the UI: **Settings → Storage → Buckets** → expand the `downloads` row → **Public access** → pick **Specific prefixes public** and add `public/`:

![Bucket policies with the public-access tri-state](/_/screenshots/bucket-policies.jpg)

The tri-state maps directly to the YAML: **Private (default)** (no anonymous access), **Specific prefixes public** (`public_prefixes: [...]`), **Entire bucket public** (`public: true`).

Apply the change. The proxy hot-reloads it and creates a read-only request rule named `public-prefix:downloads` from it.

The proxy refuses to make the coordination bucket (`config_sync_bucket`) public, because S3 clients can never reach that bucket.

## 2. Know what anonymous callers get

Anonymous requests can GET, HEAD, and LIST under the prefix, and LIST results stay inside the prefix. They run as the built-in `$anonymous` user and can never write. The full semantics are in the [authentication reference](../reference/authentication.md#public-prefixes).

A response to an anonymous request carries no `x-amz-meta-dg-tool` header, because that metadata names the proxy version. The failed-sign-in lockout does not block public reads: a client address that the proxy locked out still gets the objects under a public prefix.

## Mind the trailing slash

`public/` matches `public/installer.zip` but **not** `publicity/report.pdf`; `public` (no slash) is a string-prefix match and would expose both. Always end the prefix with `/` unless you deliberately want string-prefix matching.

## Whole-bucket public

`public: true` is shorthand for `public_prefixes: [""]`. It makes every object in the bucket readable without credentials:

```yaml
# validate
storage:
  buckets:
    downloads:
      public: true
```

The proxy logs a startup warning when a whole bucket is public. Use it only for buckets that contain nothing but published artifacts. Anything that someone uploads there later is public as soon as it arrives, and LIST exposes every key name in the bucket.

To share one object for a limited time, generate a presigned URL instead of publishing a prefix. The URL expires after at most 7 days, and you can revoke it by rotating the signing user's key:

```bash
aws --endpoint-url https://s3.acme.example \
  s3 presign s3://downloads/internal/draft-installer.zip --expires-in 3600
```

## Verify with a cold curl

Test from a shell with **no** AWS environment. A leftover `AWS_ACCESS_KEY_ID` would authenticate the request, so the test would prove nothing (credentials always win over public-prefix config):

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

Each anonymous request writes a line with `action=public_read` and `user=$anonymous` to the proxy log. These lines do not appear in the admin GUI's audit log.

## Lock writes down

Publishing a prefix does not change who can write to it. Review write access separately:

- Scope upload credentials to the prefix and pin them to your network: [How to restrict access by IP and prefix](restrict-access-with-conditions.md).
- Reject anonymous mutation attempts on the bucket before authentication even runs: [How to gate requests before authentication](gate-requests-with-admission-rules.md).

## Related

- [Authentication reference](../reference/authentication.md#public-prefixes): exact anonymous semantics and prefix validation rules.
- [About authentication and access control](../explanation/security-model.md): why public prefixes are carve-outs, not a credential type.
- [How to gate requests before authentication](gate-requests-with-admission-rules.md): the `public-prefix:*` public-access rules, and how one deny rule takes a public folder offline.
- [How to create IAM users and groups](create-iam-users.md): credentials for everyone who is not anonymous.
