# How to migrate an existing S3 bucket into the proxy

This guide shows you how to put an existing S3 bucket — with years of objects already in it — behind the proxy. There are two routes; pick by how much you care about compressing the historical objects.

- **If you want zero data movement**, point the proxy at the bucket in place. Existing objects pass through untouched; only new uploads get delta compression.
- **If you want history compressed too**, copy the data through the proxy once, then cut over.

For why old objects can't be compressed retroactively, see [delta compression](../explanation/delta-compression.md).

## Route 1: point at the bucket in place

Use this when the existing data can stay where it is. Example: the legacy AWS bucket `acme-firmware` becomes the proxy bucket `releases`.

1. Declare the backend the bucket lives on, and alias the proxy-side name to the real one:

   ```yaml
   # validate
   storage:
     default_backend: aws-dr
     backends:
       - name: aws-dr
         type: s3
         region: eu-west-1
     buckets:
       releases:
         backend: aws-dr
         alias: acme-firmware    # the pre-existing bucket, unchanged
   ```

   From the admin UI: **Settings → Storage → Backends** to add the backend, then **Settings → Storage → Buckets** to add `releases` with the alias.

2. Apply the config and list through the proxy — every existing object is already visible:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 ls s3://releases/firmware/widget-3000/
   ```

3. Done. Existing objects are served as passthrough (the proxy reads them as-is). New uploads — say `ci-uploader` pushing `firmware/widget-3000/fw-2.4.1.tar` — go through the delta router and start saving space immediately.

**The caveat:** objects that entered the bucket before the proxy never retro-compress. The proxy only delta-encodes at write time; a passthrough object stays passthrough forever unless something rewrites it through the proxy. If historical savings matter, use route 2.

## Route 2: copy through the proxy

Use this when you want the version history itself stored as deltas. The proxy rebuilds each object on write, so a one-time sync **through** the proxy re-stores everything compressed.

1. Set up the destination bucket behind the proxy (no alias — this is a fresh namespace), routed to whichever backend should hold the compressed copy:

   ```yaml
   storage:
     buckets:
       releases:
         backend: hetzner-fsn1
   ```

2. Sync the old bucket into the proxy. The source read uses your normal AWS credentials; the destination write goes to the proxy endpoint:

   ```bash
   aws s3 sync s3://acme-firmware /tmp/acme-firmware          # pull from AWS
   aws --endpoint-url https://s3.acme.example \
       s3 sync /tmp/acme-firmware s3://releases               # push through the proxy
   ```

   If both sides are reachable from one host with enough disk, you can pipe bucket-to-bucket with any S3 tool (`rclone copy` works too) — what matters is that **writes land on the proxy endpoint**, so each object passes through the delta router.

3. Upload order matters for ratios: the first object in each prefix becomes the reference baseline, and later versions delta against it. `aws s3 sync` copies in key order, which for versioned names (`fw-2.3.0.tar`, `fw-2.4.0.tar`…) is usually also version order — good enough in practice.

4. Spot-check the savings on the stats endpoint before cutting over. The endpoint reveals the size of every bucket, so it answers only an admin session (`401` otherwise). Sign in first to store the session cookie:

   ```bash
   curl -s -c /tmp/dgp.cookies -X POST https://s3.acme.example/_/api/admin/login \
     -H 'Content-Type: application/json' -d '{"password": "<bootstrap-password>"}'
   curl -s -b /tmp/dgp.cookies https://s3.acme.example/_/stats
   # per bucket, with the running counter:
   curl -s -b /tmp/dgp.cookies 'https://s3.acme.example/_/stats?bucket=releases'
   ```

## Cut clients over

Either route ends the same way: swap the endpoint.

1. Point clients at the proxy: change `--endpoint-url` (or the SDK's `endpoint_url`) from the provider's URL to the proxy's. Bucket names and key paths are unchanged (route 1) or unchanged-by-construction (route 2).
2. Issue proxy credentials — clients sign with the proxy's SigV4 credentials now, not the provider's. Create per-client IAM users (e.g. `ci-uploader` with write on `releases/*`) in **Settings → Access → Users**.
3. If you ran route 2, freeze or retire the old bucket once traffic has moved, so nothing writes around the proxy.

Do not keep writing to the backend bucket directly (e.g. with the Python DeltaGlider CLI or raw AWS credentials) — writes that bypass the proxy don't get compressed, and on an encrypted backend they'd be stored plaintext. Standardize on the proxy as the only write path.

## Verify

1. List and read an old object through the proxy:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 cp s3://releases/firmware/widget-3000/fw-2.3.0.tar - | sha256sum
   ```

   The hash must match the original — the proxy is byte-exact.

2. Upload a new version and confirm it stored as a delta — check the `x-amz-storage-type` response header on a GET/HEAD (`delta` for compressed, `passthrough` otherwise).

3. Watch the overall savings grow on the dashboard at **Settings → Observability → Dashboard** (`/_/admin/dashboard`). Its **Analytics** view shows the savings for each bucket. The same counters are also available to Prometheus at `/_/metrics`.

## Related

- [How to route a bucket to a different backend](route-a-bucket-to-a-backend.md) — the alias/routing mechanics in full.
- [Configuration reference](../reference/configuration.md) — the `storage.backends` and `storage.buckets` (alias) fields used here.
- [How to set per-bucket compression and quotas](set-bucket-compression-and-quotas.md) — tune what gets compressed.
- [Your first delta savings](../tutorials/first-delta-savings.md) — see the compression pipeline end to end.
- [Delta compression](../explanation/delta-compression.md) — why compression happens only at write time.
