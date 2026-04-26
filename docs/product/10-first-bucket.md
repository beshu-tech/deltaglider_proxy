# Setting up a bucket

*Route a bucket to a specific backend, alias one name to another, tune compression per bucket, and publish a public prefix.*

Every bucket the proxy serves is configured in one place: `storage.buckets` in YAML. This page shows the common shapes.

## The shape

```yaml
storage:
  backend:
    type: s3                    # default backend (used when a bucket doesn't override)
    endpoint: https://s3.eu-central-1.amazonaws.com
    region: eu-central-1
  buckets:
    my-bucket:                  # bucket name the client sees
      backend: hetzner           # route this bucket to a named backend (optional)
      alias: real-bucket-2024    # real name on the backend (optional)
      compression: false         # override per-bucket compression policy
      max_delta_ratio: 0.5       # per-bucket delta ratio cutoff
      public_prefixes: [docs/]   # key prefixes with anonymous read access
      quota_bytes: 10737418240   # 10 GB write limit (soft)
```

All fields except the bucket name are optional. Unset fields inherit from the top-level defaults (compression behaviour) or the default backend (routing).

## Route a bucket to a different backend

Named backends let you mix storage providers behind one S3 endpoint:

```yaml
# validate
storage:
  default_backend: primary
  backends:
    - name: primary
      type: s3
      endpoint: https://s3.eu-central-1.amazonaws.com
      region: eu-central-1
    - name: hetzner
      type: s3
      endpoint: https://fsn1.your-objectstorage.com
      region: fsn1
      force_path_style: true
    - name: local
      type: filesystem
      path: /var/lib/dgp-local
  buckets:
    public-downloads:
      backend: hetzner            # lives on Hetzner
    scratch:
      backend: local              # lives on local disk
    # Everything else hits the default (primary) backend.
```

From the admin UI: **Configuration → Storage → Backends → + Add backend** and then on the per-bucket page, pick the backend from a dropdown.

## Alias (different bucket name on the backend)

When the name the client uses doesn't match the real bucket on the backend:

```yaml
storage:
  buckets:
    archive:              # clients see this name
      backend: hetzner
      alias: prod-archive-2024   # real Hetzner bucket
```

Clients call `s3://archive/file.zip`; the proxy translates to `s3://prod-archive-2024/file.zip` on the backend.

Useful when:

- You're moving buckets between backends without updating clients.
- The real bucket name has a prefix you don't want to expose (`prod-archive-2024` vs `archive`).
- You want to colocate two logically-separate namespaces in one physical bucket (two aliases pointing at the same real bucket — you probably don't want this without prefix scoping via IAM).

## Per-bucket compression policy

Compression is on globally by default. Turn it off for a specific bucket when the content won't benefit (images, video, already-compressed archives):

```yaml
# validate
storage:
  buckets:
    images:
      compression: false    # JPEGs never delta-compress well
    videos:
      compression: false
    builds:
      max_delta_ratio: 0.5  # stricter than the global 0.75 — only keep deltas with >=50% savings
```

`compression: false` short-circuits the router — every file in that bucket goes passthrough regardless of extension. Saves the xdelta3 CPU cost entirely.

`max_delta_ratio` is the fallback cutoff: if `delta_size / original_size ≥ ratio`, the object falls back to passthrough anyway. Lower = stricter (less delta, more passthrough). See [how delta works](reference/how-delta-works.md) for the routing logic.

> **Note:** compression is **per-bucket**, not per-prefix. If you want some prefixes inside a bucket to compress and others to passthrough, today the options are (a) split into two buckets with different policies, or (b) accept that non-compressible files will fall back to passthrough via `max_delta_ratio` on their own. Per-prefix compression policy is on the roadmap.

## Public prefixes (anonymous read)

Publish a single prefix for anonymous download without exposing the rest of the bucket:

```yaml
# validate
storage:
  buckets:
    my-releases:
      public_prefixes:
        - releases/public/
```

Anyone can:

```bash
curl https://dgp.example.com/my-releases/releases/public/latest.zip  # works, no auth
aws --endpoint-url https://dgp.example.com s3 ls s3://my-releases/releases/public/   # works, dummy creds
```

But:

```bash
curl https://dgp.example.com/my-releases/secret/file.zip  # 403 AccessDenied
```

**`public: true` shorthand** expands to `public_prefixes: ["" ]` (entire bucket is public):

```yaml
storage:
  buckets:
    opensource-releases:
      public: true
```

Trailing slash matters. `releases/public/` matches `releases/public/foo` but not `releases/publicish/bar`. Always end prefixes with `/` unless you deliberately want a string-prefix match.

List operations on public prefixes return *only* the public subtree — the rest of the bucket stays invisible.

## Soft quota (write limit)

Soft write limit for a bucket:

```yaml
# validate
storage:
  buckets:
    uploads:
      quota_bytes: 5368709120   # 5 GB
```

PUT requests that would exceed the quota are rejected with 403. Quota is **soft** — it reads from the 5-minute-cached usage scanner, so a burst of concurrent writes can overshoot by up to 5 minutes of throughput. For a strict hard cap, enforce at the reverse proxy or storage-provider layer.

`quota_bytes: 0` freezes the bucket — all writes blocked. Useful for read-only migrations.

## From the admin UI

Everything above is also reachable from **Configuration → Storage → Buckets**. The UI round-trips through the same YAML, so changes made in the GUI show up in `/_/api/admin/config/export?section=storage` and vice versa.

## Verification

After configuring a bucket:

```bash
# (1) the config applied
curl -b cookies -X GET https://dgp.example.com/_/api/admin/config/section/storage?format=yaml

# (2) SigV4 client sees it
aws --endpoint-url https://dgp.example.com s3 ls

# (3) public prefix works without creds (if configured)
curl -I https://dgp.example.com/my-releases/releases/public/

# (4) object lands on the right backend (filesystem backend)
ls /var/lib/deltaglider_proxy/data/deltaspaces/my-bucket/
```

If something doesn't behave as expected, check [Troubleshooting](41-troubleshooting.md) — especially the "object goes to the wrong backend" and "public prefix returns 403" sections.

## Next steps

- **Encrypt the backend.** Each backend carries an `encryption` block with four modes (`none`, `aes256-gcm-proxy`, `sse-kms`, `sse-s3`). Configure it in Admin → Storage → Backends, or add an `encryption: { mode: aes256-gcm-proxy, key: "${DGP_ENCRYPTION_KEY}" }` block to your YAML. See [reference/encryption-at-rest.md](reference/encryption-at-rest.md) for the decision tree and worked examples.
- **Add IAM users.** The admin GUI at `/_/admin/users` — or via YAML + OAuth mapping rules. See [auth/31-sigv4-and-iam.md](auth/31-sigv4-and-iam.md).
- **Harden for production.** [20-production-security-checklist.md](20-production-security-checklist.md) covers SigV4, bootstrap password, rate limiting, TLS, and encryption at rest as a linear step-by-step.

## Related

- [reference/configuration.md](reference/configuration.md) — the complete `storage.buckets` field reference.
- [auth/32-iam-conditions.md](auth/32-iam-conditions.md) — scope IAM users to specific buckets or prefixes.
- [reference/how-delta-works.md](reference/how-delta-works.md) — what "compression" actually does internally.
- [reference/encryption-at-rest.md](reference/encryption-at-rest.md) — per-backend encryption modes, wire format, rotation recipes.
