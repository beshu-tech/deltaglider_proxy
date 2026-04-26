# Per-Bucket Storage Quota

> **Status:** shipped. This planning note is kept for archaeology only.
> Current operator docs live in
> [`docs/product/10-first-bucket.md`](../docs/product/10-first-bucket.md#soft-quota-write-limit)
> and [`docs/product/reference/configuration.md`](../docs/product/reference/configuration.md).
> The shipped feature is a **soft** per-bucket write limit backed by
> the cached usage scanner; `quota_bytes: 0` freezes a bucket.

## Value Proposition

Limit storage consumption per bucket. Leverages the existing `UsageScanner` which already computes per-bucket sizes with a cached tree structure (5-minute TTL, 1000-entry LRU).

## Design

- Add `quota_bytes: Option<u64>` to `BucketPolicyConfig`
- On PUT, before `engine.store()`: check cached usage against quota
- `total_size + incoming_size > quota_bytes` → reject with `QuotaExceeded` (HTTP 403)
- Cached sizes = O(1) quota checks (no scan on every PUT)
- Soft quota: 5-minute cache TTL allows overshoot during burst writes. Acceptable.
- Admin GUI: quota input + usage progress bar on bucket policy card
- Config: `[buckets.releases] quota_bytes = 10737418240` (10 GB)

## What Already Exists

| Component | Status |
|-----------|--------|
| `UsageScanner` with `total_size`, `total_objects`, `children` tree | DONE |
| `/_/api/admin/usage` endpoint | DONE |
| Per-bucket policy config (`BucketPolicyConfig`) | DONE |
| PUT handler hook point (before `engine.store()`) | DONE |

## What's Needed

Shipped; see the status note above.

## Delta Compression Interaction

- Quota counts STORED size (compressed), not original
- Incentivizes compression — users who compress more can store more
- `UsageScanner` already counts stored sizes (from LIST)
- Edge case: 100MB file compresses to 1MB → counts as 1MB against quota

## Encryption Interaction

- Encrypted blobs are +28 bytes per object. Negligible for quota.

## Multipart Interaction

- Quota check on `CompleteMultipartUpload` (in `engine.store()`), not per-part
- Risk: start many multipart uploads to bypass quota
- Mitigation: existing `max_multipart_uploads` (100) * `max_object_size` (100MB) = 10GB bounded worst case
- Future: count in-flight multipart sizes against quota too

## Replication Interaction

- Quota applies to PRIMARY only. Replica doesn't count.

## Race Conditions

- Concurrent PUTs can race past quota (check → both pass → both store → over quota)
- Acceptable for soft quotas (scanner refreshes every 5 minutes)
- Future: atomic counter per bucket for hard quotas

## Edge Cases

| Case | Handling |
|------|----------|
| DELETE reduces usage | Eventually (5-minute scanner lag). Not instant. |
| Quota = 0 | Blocks ALL writes. Valid use case (freeze bucket). |
| Quota change while cache stale | New quota takes effect on next scan. Acceptable. |

## Effort

~2 days. Most infrastructure already exists.

## Addresses

Stefano requirement: Quota
