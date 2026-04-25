# s3s Adapter Migration

DeltaGlider's product logic should remain independent from its S3 protocol
adapter. With the `s3s-adapter` Cargo feature enabled, `s3s` is the default
serving path. Set `DGP_S3_ADAPTER=axum` to roll back to the legacy Axum S3
handlers for one release cycle.

## Current State

- `src/s3_adapter_s3s.rs` compiles only with `--features s3s-adapter`.
- `DeltaGliderS3Service` holds the existing `AppState`.
- The service implements `ListBuckets`, `HeadObject`, `GetObject`,
  `ListObjectsV2`, `CreateBucket`, `DeleteBucket`, `DeleteObject`,
  `DeleteObjects`, normal single-part `PutObject`, the multipart lifecycle,
  `CopyObject`, and `UploadPartCopy`.
- All other operations still use the `s3s` default `NotImplemented`.
- The runtime uses `s3s` by default when built with `--features s3s-adapter`.
  The old Axum handler path remains available via `DGP_S3_ADAPTER=axum`.
- The opt-in route keeps the existing outer Axum middleware chain for admission,
  SigV4/IAM authorization, metrics, body limits, timeouts, concurrency limits,
  and CORS. An internal `s3s` auth bridge supplies secrets so `s3s` can decode
  signed AWS SDK requests after the outer middleware has admitted them.
- `ListBuckets` and `ListObjectsV2` consume the existing `AuthenticatedUser` /
  `ListScope` request extensions so IAM prefix filtering is preserved.
- Compatibility shims currently preserve DeltaGlider-specific response
  contracts that `s3s` does not model directly: `x-amz-request-id` on every
  response, `<RequestId>` in error XML, legacy ACL XML root shape, debug
  storage headers, and the MinIO-style `ListObjectsV2?metadata=true`
  `<UserMetadata>` extension.
- `s3s` responses include `x-deltaglider-s3-adapter: s3s` so operators and
  tests can confirm which runtime path served a request.

`GetObject` forwards passthrough responses as a `StreamingBlob`; reconstructed
delta responses still enter the adapter as buffered data because the engine's
delta decode path necessarily materialises the object.

## Migration Order

1. Implement read-only object operations: `HeadObject`, `GetObject`. (done)
2. Implement bucket listing: `ListObjectsV2` (done), then `ListBuckets`.
3. Implement write/delete operations: `DeleteObject`, `DeleteObjects`, and
   single-part `PutObject` (done).
4. Implement copy and multipart operations (done for the common paths).
5. Wire the feature-gated runtime switch while keeping current outer SigV4/IAM
   middleware as the authorization source of truth. (done)

`PutObject` currently supports the normal single-part path only. It preserves
the important existing hardening: body size limit, bucket existence,
Content-MD5, and ETag preconditions.

Remaining adapter gaps before deleting the legacy adapter:

- `UploadPartCopy` supports byte ranges by materialising the source object
  before slicing, matching the old adapter but not a future streaming ideal.
  This is acceptable for beta but should become streaming before removing the
  legacy adapter fallback.
- `tests/s3_compat_test.rs` passes serially against `DGP_S3_ADAPTER=s3s`.
- S3-facing IAM/admission/public-prefix suites pass against
  `DGP_S3_ADAPTER=s3s` (`admission_test`, `iam_authorization_test`,
  `iam_list_scope_test`, `public_prefix_list_test`, `public_prefix_test`).
- High-risk S3 suites pass against `DGP_S3_ADAPTER=s3s` (`aws_chunked_upload_test`,
  `concurrency_test`, `metadata_validation_test`, `multipart_etag_test`,
  `recursive_delete_test`, `s3_correctness_test`, `storage_resilience_test`,
  `unmanaged_objects_test`).
- CI now has a dedicated `s3s Adapter Tests` job covering adapter build checks,
  parity tests, the S3 compatibility corpus, and the S3-facing auth/admission
  suites. It also runs high-risk semantics that previously exposed drift:
  AWS streaming-chunked uploads, recursive prefix delete, multipart ETags,
  concurrent S3 operations, unmanaged objects, storage resilience, metadata
  validation, and correctness/subresource precedence.

## Rollout Plan

1. **Current state:** build with `--features s3s-adapter`; `s3s` is default.
   Use `DGP_S3_ADAPTER=axum` as the rollback switch.
2. **Staging beta:** run the dedicated CI job plus staging traffic with the
   response header `x-deltaglider-s3-adapter: s3s` sampled in logs.
3. **Cleanup:** after one release with no rollback, delete the old S3 Axum
   handler surface and keep admin/status/demo routes.

Each phase must run the same integration corpus against both adapters before
the `s3s` path can become default.

## Non-Negotiable Boundaries

- `s3s` owns S3 HTTP parsing, DTOs, XML/error rendering, and protocol shape.
- DeltaGlider keeps compression, encryption, replication, IAM/admission,
  metadata cache, quota, audit, and storage backend behavior.
- Existing body limits, rate limiting, and concurrency limits remain outside
  `s3s`; the crate explicitly does not provide those protections.
