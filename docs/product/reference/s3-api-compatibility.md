# S3 API compatibility

DeltaGlider speaks the S3 wire protocol through the [`s3s`](https://github.com/Nugine/s3s) framework, so standard tools (AWS CLI, boto3, the AWS SDKs, Cyberduck, rclone, `s3fs`) work unchanged. This page is the austere list of which S3 operations the proxy implements, which it stubs, and which it rejects — so you can tell, before integrating, whether your client's calls will work.

Status legend:

- **✅ Full** — real implementation; delta compression is applied transparently where relevant.
- **◑ Stub** — the call succeeds with a fixed, well-formed response. The proxy does not store or honour the underlying feature (e.g. ACLs), but compatible clients that merely *probe* for it keep working.
- **🚫 Not supported** — returns `501 NotImplemented` with a clear message.
- **— Not implemented** — no handler; `s3s` returns its default `NotImplemented` error.

> Delta compression, encryption-at-rest, replication, and lifecycle are **proxy-layer features**, not S3 operations. They are applied to the object operations below transparently — a client never sees them. Lifecycle and replication are configured through the proxy (YAML / admin API), **not** through the S3 `PutBucketLifecycle` / `PutBucketReplication` calls, which are intentionally not implemented.

## Object operations

| Operation | Status | Notes |
|---|---|---|
| `GetObject` | ✅ Full | Delta-decoded on read; range requests and `If-Match`/`If-None-Match`/`If-Modified-Since`/`If-Unmodified-Since` conditionals supported; response-header overrides via query params. |
| `HeadObject` | ✅ Full | Returns object metadata; same conditional headers as `GetObject`. |
| `PutObject` | ✅ Full | Delta-encoded on write for eligible types; quota-enforced; `If-Match`/`If-None-Match` conditionals; user metadata preserved. |
| `CopyObject` | ✅ Full | Source authorization + conditionals checked; `COPY`/`REPLACE` metadata directive; destination quota enforced. |
| `DeleteObject` | ✅ Full | Single key, or recursive prefix delete when the key ends in `/`. A missing key is treated as success (S3 semantics). |
| `DeleteObjects` | ✅ Full | Batch delete up to 1000 keys; `Quiet` flag and per-key error reporting honoured. |

## List operations

| Operation | Status | Notes |
|---|---|---|
| `ListObjectsV2` | ✅ Full | Continuation-token pagination; delimiter / common-prefix; IAM-filtered (a user sees only objects they can read). |
| `ListObjects` | ✅ Full | Legacy marker-based listing, implemented over the same path as V2. |
| `ListBuckets` | ✅ Full | IAM-filtered; optional prefix / `max-buckets` pagination. |

## Multipart upload

| Operation | Status | Notes |
|---|---|---|
| `CreateMultipartUpload` | ✅ Full | Allocates an upload ID; metadata and content-type persisted. |
| `UploadPart` | ✅ Full | Part buffering with ETag; `Content-MD5` validated; max-object-size enforced. |
| `UploadPartCopy` | ✅ Full | Copies a (ranged) slice of a source object into the upload; source authorization checked. |
| `CompleteMultipartUpload` | ✅ Full | Delta or passthrough chosen by size/eligibility; multipart ETag preserved; quota enforced. |
| `AbortMultipartUpload` | ✅ Full | Cancels the upload and reclaims state. |
| `ListParts` | ✅ Full | `part-number-marker` continuation; `max-parts` 1–1000. |
| `ListMultipartUploads` | ✅ Full | `key-marker` + `upload-id-marker` continuation; prefix / delimiter. |

## Bucket operations

| Operation | Status | Notes |
|---|---|---|
| `CreateBucket` | ✅ Full | Returns the location header. |
| `DeleteBucket` | ✅ Full | Requires the bucket to be empty; purges orphaned multipart state; blocks while uploads are completing. |
| `HeadBucket` | ✅ Full | `200` if the bucket exists, else `404 NoSuchBucket`. Region header is `us-east-1`. |
| `GetBucketLocation` | ◑ Stub | Returns an empty location-constraint (interpreted as `us-east-1`). |
| `GetBucketVersioning` | ◑ Stub | Returns an empty status. The proxy does **not** implement S3 object versioning — see [Versioning vs S3 versioning](../explanation/versioning-vs-s3-versioning.md). |

## ACLs, tagging & policy

The proxy enforces access control through its own **IAM / ABAC** model (see [IAM permissions](iam-permissions.md)), not through S3 ACLs, bucket policies, or object tags. The ACL probes below return a canned *private* response so clients that check ACLs on connect keep working; the mutation calls are explicitly rejected rather than silently ignored.

| Operation | Status | Notes |
|---|---|---|
| `GetBucketAcl` | ◑ Stub | Bucket existence checked; returns a canned private ACL (single owner, full control). |
| `GetObjectAcl` | ◑ Stub | Object existence checked; returns a canned private ACL. |
| `PutBucketAcl` | 🚫 Not supported | `501` — "Bucket ACL mutation is not supported by this proxy". |
| `PutObjectAcl` | 🚫 Not supported | `501` — "Object ACL mutation is not supported by this proxy". |
| `GetBucketTagging` / `PutBucketTagging` | 🚫 Not supported | `501` — bucket tagging is not supported. |
| `GetObjectTagging` / `PutObjectTagging` / `DeleteObjectTagging` | 🚫 Not supported | `501` — object tagging is not supported. |
| `GetBucketPolicy` / `PutBucketPolicy` / `DeleteBucketPolicy` | — Not implemented | Use IAM permissions and [admission rules](../how-to/gate-requests-with-admission-rules.md) instead. |

## Not implemented

The following families have no handler — `s3s` returns `NotImplemented`. Where a proxy-native equivalent exists, it is linked.

- **Lifecycle:** `PutBucketLifecycleConfiguration` / `GetBucketLifecycleConfiguration` / `DeleteBucketLifecycle` → configure through the proxy instead ([Expire and archive objects](../how-to/expire-and-archive-objects.md), [Lifecycle reference](lifecycle.md)).
- **Replication:** `PutBucketReplication` / `GetBucketReplication` / `DeleteBucketReplication` → configure through the proxy instead ([Replicate a bucket](../how-to/replicate-a-bucket.md), [Replication reference](replication.md)).
- **Notifications:** `PutBucketNotificationConfiguration` / `GetBucketNotificationConfiguration` → use the proxy's [event notifications](../how-to/send-event-notifications.md) / [event log](event-outbox.md).
- **Object Lock / retention / legal hold:** `PutObjectLockConfiguration`, `PutObjectRetention`, `PutObjectLegalHold` (and their getters).
- **CORS:** `PutBucketCors` / `GetBucketCors` / `DeleteBucketCors`.
- **Website / logging / accelerate / request-payment.**
- **Inventory / metrics / analytics / intelligent-tiering configurations.**
- **`RestoreObject`, `SelectObjectContent`.**

## Non-standard endpoints the proxy adds

These are not part of the S3 spec but are served on the S3 port for compatibility and the browser UI:

| Endpoint | Purpose |
|---|---|
| `POST /{bucket}` (`multipart/form-data`) | Browser HTML-form `PostObject` upload — used by the embedded S3 browser. SigV4 POST-policy validated; quota enforced. |
| `HEAD /` | Connection probe used by some clients (e.g. Cyberduck); returns `200 OK`. |

> The full admin API and the docs/UI live under the `/_/` prefix on the same port — `_` is not a valid S3 bucket name, so it never collides with object traffic. See the [admin API reference](admin-api.md).
