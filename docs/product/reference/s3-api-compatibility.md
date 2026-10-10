# S3 API compatibility

DeltaGlider speaks the S3 wire protocol through the [`s3s`](https://github.com/Nugine/s3s) framework, so standard tools (AWS CLI, boto3, the AWS SDKs, Cyberduck, rclone, `s3fs`) work unchanged. This page lists the S3 operations that the proxy implements, stubs or rejects. Use it to check, before you integrate a client, whether the requests of that client will work.

Status legend:

- **✅ Full**: the proxy implements the operation, and it applies delta compression transparently where relevant.
- **◑ Stub**: the request succeeds with a fixed, well-formed response. The proxy does not store or honour the underlying feature (e.g. ACLs), but compatible clients that merely *probe* for it keep working.
- **🚫 Not supported**: the proxy returns `501 NotImplemented` with a clear message.
- **— Not implemented**: no handler exists, so `s3s` returns its default `NotImplemented` error.

> Delta compression, encryption-at-rest, replication and lifecycle are proxy-layer features, not S3 operations. The proxy applies them transparently to the object operations below, and a client never sees them. You configure lifecycle and replication through the proxy (YAML or the admin API). The proxy does not implement the S3 `PutBucketLifecycle` and `PutBucketReplication` requests, on purpose.

## Object operations

| Operation | Status | Notes |
|---|---|---|
| `GetObject` | ✅ Full | Delta-decoded on read; range requests and `If-Match`/`If-None-Match`/`If-Modified-Since`/`If-Unmodified-Since` conditionals supported (a date compares whole seconds, as HTTP dates have no fractions); an `If-Range` that does not match the object serves the whole object with `200`; a `304 Not Modified` carries `ETag` and `Last-Modified`; a range that starts past the end of the object, or a suffix range on an empty object, gets `416 InvalidRange`, and a range end past the object is cut to its last byte; response-header overrides via query params (refused with `400 InvalidRequest` on anonymous requests, as on S3). Responses whose content type a browser could run as a document carry `Content-Security-Policy: sandbox`. |
| `HeadObject` | ✅ Full | Returns object metadata; same conditional headers as `GetObject`. A request with `Range` answers `206` with `Content-Range`, as on S3. |
| `PutObject` | ✅ Full | Delta-encoded on write for eligible types; quota-enforced; `If-Match` (strong ETag comparison; a missing key gets `404 NoSuchKey`) and `If-None-Match: *` conditionals (another `If-None-Match` value gets `501 NotImplemented`, as on S3; the check and the write are atomic against other `PutObject` requests to the same key on one instance); user metadata preserved, up to the S3 limit of 2 KB (larger metadata gets `400 MetadataTooLarge`, as does a copy, a multipart upload, a browser form upload, or a write whose metadata does not fit the extended attribute of the filesystem backend). On an S3 backend, the proxy stores its own metadata fields in the same 2 KB, so the limit for the client's metadata is a few hundred bytes lower, and a request over it also gets `400 MetadataTooLarge`. A key that ends in `/` (such as `photos/`) is a folder marker: a request with an empty body stores a zero-byte object under that key, and a request with a body gets `400 InvalidArgument`. The proxy stores a folder marker without its content type or user metadata, and never encrypts it. The user metadata `x-amz-meta-dg-no-delta: true` makes the proxy store a delta-eligible object as a plain (passthrough) object, and the proxy does not store this hint with the object. |
| `CopyObject` | ✅ Full | Source authorization + conditionals checked; `COPY`/`REPLACE` metadata directive; destination quota enforced. A copy onto its own key that changes nothing (`COPY`, and no new storage class or encryption setting) gets `400 InvalidRequest`, as on S3. A source larger than the spool threshold (`DGP_SPOOL_THRESHOLD_BYTES`) streams into a spool file, so the copy does not hold the whole source in memory. When the source changes during such a copy, the copy gets `503 SlowDown`, and the retry copies the new object. When the destination stores the object as it is (a key that is not delta-eligible, the `dg-no-delta: true` hint, or a bucket with compression disabled), the source can be up to 5 GiB, the CopyObject limit of S3, even when that is more than `max_object_size`. When the destination can become a delta, the source can be at most `max_object_size`, and a larger one gets `400 EntityTooLarge`. |
| `DeleteObject` | ✅ Full | Deletes one key. A key that ends in `/` deletes only the folder marker with that name, never the keys under it, as on S3. To delete a folder, list its keys and delete them with `DeleteObjects` (the object browser and `aws s3 rm --recursive` do this). A missing key is treated as success (S3 semantics). |
| `DeleteObjects` | ✅ Full | Batch delete up to 1000 keys; `Quiet` flag and per-key error reporting honoured. The batch finishes even when the client disconnects or the request times out, because the proxy runs it in its own task. The proxy works on up to 8 folders at the same time, and each deleted key gets its event as soon as it is deleted. |

### A request to a missing bucket

Every object and list request to a bucket that does not exist gets `404 NoSuchBucket`, as on S3. This includes a `ListObjects` request (it does not get an empty `200`), a `GetObject` or `HeadObject` request (it does not get `NoSuchKey`), and a `DeleteObject` request (it does not get `204`). The proxy checks the bucket only after the object is not found, so a request that finds its object costs no extra backend request.

### Maximum key length

The longest key that the proxy stores depends on the backend of the bucket:

| Backend | Limit | Why |
|---|---|---|
| S3 | 1024 bytes for the whole key | The S3 limit. A backend that refuses the key answers `KeyTooLongError`, and the proxy passes that on. |
| Filesystem | 255 bytes for each part of the key between `/` characters | Each part becomes a directory or file name, and file systems such as ext4 and XFS limit one name to 255 bytes. A delta-compressed object is stored with a `.delta` suffix, so its last part has 249 bytes. |

A key over the limit gets `400 KeyTooLongError`. SDKs do not retry a 400.

### A key and a folder with the same name (filesystem backend)

The filesystem backend stores the key `a` as a file named `a`, and it stores every key that starts with `a/` inside a directory named `a`. One path cannot be a file and a directory at the same time, so on this backend an object `a` and an object under `a/` (such as `a/b`) cannot both exist. S3 has no such limit.

- A `PutObject` (or copy, or multipart completion) that needs the other kind of entry gets `400 InvalidRequest`, with a message that names this limitation. SDKs do not retry a 400. To store the key, delete the other object first, or put the bucket on an S3 backend.
- A read of a key whose path is a directory, or whose path is under a file, gets `404 NoSuchKey`, because no object has that key. A `DeleteObject` of such a key succeeds and deletes nothing.

The S3 backend stores keys as S3 keys, so there both objects can exist.

## List operations

| Operation | Status | Notes |
|---|---|---|
| `ListObjectsV2` | ✅ Full | Continuation-token and `start-after` pagination (`NextContinuationToken` is opaque: `dg1.` followed by the base64url form of the last key; send it back unchanged. A token in the older raw-key form is still accepted in this release, and each such request adds one to `deltaglider_list_legacy_continuation_tokens_total`); `max-keys=0` answers no keys; delimiter / common-prefix; `encoding-type=url`; IAM-filtered (a user sees only objects they can read, and a continuation token never names a hidden key; the proxy reads only the prefixes that the user's policy can reach, see [ListBucket prefix scoping](iam-permissions.md#listbucket-prefix-scoping)). When the proxy cannot narrow the user's policy to prefixes, one request reads at most `advanced.filtered_list_max_engine_pages` backend pages (default 50, `DGP_FILTERED_LIST_MAX_ENGINE_PAGES`) and then gets `400 InvalidRequest` ("use a narrower prefix"). Folder markers are listed as zero-byte objects. On an S3 backend, a page can hold fewer than `max-keys` entries and still be truncated (`IsTruncated=true`), as S3 allows: the proxy serves the entries that one backend page proves complete, so a client page costs one backend LIST request instead of two. Follow `NextContinuationToken` until `IsTruncated` is false, as the AWS SDKs do. On an S3 backend, a delta or a proxy-encrypted object is listed with its original size and ETag. For a short time after the upload (usually well under one second), another proxy instance, or this one after a restart, can list the stored size and ETag instead, because the proxy writes the listing facts index in the background after the upload completes. An object that was stored before this index existed lists with its stored size until the proxy reads its metadata once (a download or a `HEAD` request). With `DGP_DEBUG_HEADERS=true`, each LIST response carries `x-deltaglider-listing-facts-misses` with the number of such entries on the page. The non-standard query parameter `metadata=true` adds the metadata of each object to the listing, with the same names as a `HeadObject` response (for example `x-amz-meta-foo`). |
| `ListObjects` | ✅ Full | Legacy marker-based listing, implemented over the same path as V2. |
| `ListBuckets` | ✅ Full | IAM-filtered; optional prefix / `max-buckets` pagination. |

## Multipart upload

| Operation | Status | Notes |
|---|---|---|
| `CreateMultipartUpload` | ✅ Full | Allocates an upload ID; metadata and content-type persisted. |
| `UploadPart` | ✅ Full | `Content-MD5` validated; max-object-size enforced. When the upload can become a delta, the proxy keeps its parts in memory until they hold more than 64 MiB (`DGP_MPU_DELTA_RECONSTRUCT_MAX_BYTES`), and then moves them to relay files in the spool directory (`DGP_SPOOL_DIR`). When the upload is always stored as a plain object (a key that is not delta-eligible, the `dg-no-delta: true` hint, or a bucket with compression disabled), every part goes to a relay file from the first part. Relay files count against `DGP_SPOOL_MAX_BYTES`. One upload can hold at most `DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES` of spool space (default half of `DGP_SPOOL_MAX_BYTES`, `0` removes this limit), and a part that goes past it gets `413 EntityTooLarge`. A part that finds no free spool space gets `503 SlowDown`. |
| `UploadPartCopy` | ✅ Full | Copies a (ranged) slice of a source object into the upload; source authorization checked. A ranged part of an object that is stored as-is, or of a delta object larger than the spool threshold, reads only that range, from the version that the copy-source conditions checked. Only the part, not the whole source, must fit `max_object_size`. A smaller delta source is read whole. |
| `CompleteMultipartUpload` | ✅ Full | Delta or passthrough chosen by size/eligibility; multipart ETag preserved; quota enforced. An upload that is always stored as a plain object is stored from its parts, without assembling them in memory. A completion of an upload that is already complete, with another part list, gets `404 NoSuchUpload`, as on S3. A retry that joins a completion in flight gets the same answer as the first request. |
| `AbortMultipartUpload` | ✅ Full | Cancels the upload and reclaims state. |
| `ListParts` | ✅ Full | `part-number-marker` continuation; `max-parts` 1 to 1000. |
| `ListMultipartUploads` | ✅ Full | `key-marker` + `upload-id-marker` continuation; prefix / delimiter. |

## Bucket operations

| Operation | Status | Notes |
|---|---|---|
| `CreateBucket` | ✅ Full | Returns the location header. |
| `DeleteBucket` | ✅ Full | Requires the bucket to be empty; purges orphaned multipart state; blocks while uploads are completing. |
| `HeadBucket` | ✅ Full | `200` if the bucket exists, else `404 NoSuchBucket`. Region header is `us-east-1`. |
| `GetBucketLocation` | ◑ Stub | Returns an empty location-constraint (interpreted as `us-east-1`). |
| `GetBucketVersioning` | ◑ Stub | Returns an empty status. The proxy does **not** implement S3 object versioning. See [Versioning vs S3 versioning](../explanation/versioning-vs-s3-versioning.md). |

## ACLs, tagging & policy

The proxy enforces access control through its own IAM / ABAC model (see [IAM permissions](iam-permissions.md)), not through S3 ACLs, bucket policies or object tags. The ACL probes below return a canned *private* response, so clients that check ACLs when they connect keep working. The proxy rejects the mutation requests explicitly, and does not silently ignore them.

A `PutObject`, `CopyObject` or `CreateMultipartUpload` request can carry an `x-amz-acl` header, an `x-amz-grant-*` header or the `x-amz-object-lock-*` headers. The proxy accepts these requests and ignores these headers, because it stores no ACL and no Object Lock setting. `PutObject` and `CopyObject` give the same answer to the same headers. To protect the stored objects with Object Lock, turn it on at the backend (see [Versioning vs S3 versioning](../explanation/versioning-vs-s3-versioning.md)).

| Operation | Status | Notes |
|---|---|---|
| `GetBucketAcl` | ◑ Stub | Bucket existence checked; returns a canned private ACL (single owner, full control). |
| `GetObjectAcl` | ◑ Stub | Object existence checked; returns a canned private ACL. |
| `PutBucketAcl` | 🚫 Not supported | `501`: "Bucket ACL mutation is not supported by this proxy". |
| `PutObjectAcl` | 🚫 Not supported | `501`: "Object ACL mutation is not supported by this proxy". |
| `GetBucketTagging` / `PutBucketTagging` | 🚫 Not supported | `501`: bucket tagging is not supported. |
| `GetObjectTagging` / `PutObjectTagging` / `DeleteObjectTagging` | 🚫 Not supported | `501`: object tagging is not supported. |
| `GetBucketPolicy` / `PutBucketPolicy` / `DeleteBucketPolicy` | — Not implemented | Use IAM permissions and [admission rules](../how-to/gate-requests-with-admission-rules.md) instead. |

## Reserved and normalised keys

The proxy stores its own files next to your objects, so a few object keys are reserved. It also changes or refuses some key shapes that S3 stores unchanged. A refused key gets `400 InvalidArgument`, and the error message names the rule and links this section.

| Key | What the proxy does | Why |
|---|---|---|
| A file name of `reference.bin`, in any prefix (for example `docs/reference.bin`) | Refuses the request. A prefix segment named `reference.bin/` is allowed. | The proxy stores the delta baseline of each prefix under this file name. |
| A file name that ends in `.delta` (for example `backups/db.sql.delta`) | Refuses the request. | The proxy stores delta-encoded objects under this suffix. |
| A key under `.dg/facts/` at the root of the bucket | Refuses a write. | The S3 backend keeps its listing facts under this prefix. |
| A key that starts with `/` (for example `/leading.txt`) | Removes the leading slashes, so the object is stored and listed as `leading.txt`. | The proxy derives the storage path from the key. S3 stores the slash as part of the key. |
| A key that contains an empty segment (`a//b`) | Refuses a write. A read or a delete of such a key works, so that an object that was stored before this rule can be removed. | An empty segment is almost always a path-join error in the client. |
| A key that contains a `..` segment, or a file name of `.` or `..` | Refuses the request. | These segments would leave the object's prefix on a filesystem backend. |
| A key that contains a NUL byte or a backslash (`\`) | Refuses the request. | These characters are not safe in a storage path. |
| A key that ends in `/` (for example `photos/`) | Stores a zero-byte folder marker (see `PutObject` above). | S3 clients create these markers for empty folders. |

A bucket name on the S3 API cannot contain `_`, as on S3. For this reason, the transient `__dgmigrate_*` routes of a migrate job and the `/_/` prefix of the admin API and the UI never collide with a client bucket.

## Not implemented

The following families have no handler, so `s3s` returns `NotImplemented`. Where a proxy-native equivalent exists, it is linked.

- **Lifecycle:** `PutBucketLifecycleConfiguration` / `GetBucketLifecycleConfiguration` / `DeleteBucketLifecycle` → configure through the proxy instead ([Expire and archive objects](../how-to/expire-and-archive-objects.md), [Lifecycle reference](lifecycle.md)).
- **Replication:** `PutBucketReplication` / `GetBucketReplication` / `DeleteBucketReplication` → configure through the proxy instead ([Replicate a bucket](../how-to/replicate-a-bucket.md), [Replication reference](replication.md)).
- **Notifications:** `PutBucketNotificationConfiguration` / `GetBucketNotificationConfiguration` → use the proxy's [event notifications](../how-to/send-event-notifications.md) / [event log](event-outbox.md).
- **Object Lock / retention / legal hold:** `PutObjectLockConfiguration`, `PutObjectRetention`, `PutObjectLegalHold` (and their getters).
- **CORS:** `PutBucketCors` / `GetBucketCors` / `DeleteBucketCors`.
- **Website / logging / accelerate / request-payment.**
- **Inventory / metrics / analytics / intelligent-tiering configurations.**
- **`RestoreObject`, `SelectObjectContent`.**

## Non-standard endpoints the proxy adds

These endpoints are not part of the S3 specification. The proxy serves them on the S3 port for client compatibility and for the browser UI:

| Endpoint | Purpose |
|---|---|
| `POST /{bucket}` (`multipart/form-data`) | Browser HTML-form `PostObject` upload. The embedded S3 browser uses it. SigV4 POST-policy validated; quota enforced. |
| `HEAD /` | Connection probe used by some clients (e.g. Cyberduck); returns `200 OK`. |

> The full admin API and the docs/UI live under the `/_/` prefix on the same port. `_` is not a valid S3 bucket name, so this prefix never collides with object traffic. See the [admin API reference](admin-api.md).
