# S3 Adapter Migration — axum legacy → s3s destination

## Status

**s3s is the migration destination.** axum is the legacy
implementation being phased out. The migration is in flight;
both adapters live in the codebase under a feature flag during
the transition.

The next concrete step is enabling `--features s3s-adapter` in
the production Dockerfile so the production image runs s3s
instead of axum. Today's prod still runs axum because the
Dockerfile doesn't enable the feature.

## Why s3s

The bespoke axum S3 implementation accumulated organically as
features needed them: presigned form-POST, multipart relay,
range-aware decryption, conditional-header precedence, RFC 7232
edge cases, SigV4 chunked, response-override headers, etc. Each
was correct in isolation; together they're ~3500 LOC of
hand-written protocol code that we own forever.

The `s3s` crate provides a code-generated S3 protocol surface
driven from the AWS S3 spec. Using it means:

- **Stop owning the protocol surface.** S3 spec evolution
  (e.g. checksum-mode headers, conditional writes, new query
  parameters) shows up upstream and we adopt it via dependency
  bump rather than feature work.
- **Code-generated correctness.** Header parsing, query
  encoding, XML serialisation, error mapping all come from a
  single source-of-truth schema. Drift between what we emit and
  what AWS clients expect drops to zero.
- **Smaller surface to test.** Our test corpus migrates from
  "did we implement DeleteObjects correctly" to "did we wire
  s3s to DeltaGlider's storage backend correctly." The former
  is open-ended; the latter is finite.

## Migration shape

Two adapters live side by side:

- **axum** — `src/api/handlers/` plus `startup.rs::build_s3_router`.
  Legacy. ~3500 LOC of handler code.
- **s3s** — `src/s3_adapter_s3s.rs` (1823 LOC) plus
  `build_s3s_router` (~400 LOC) and an XML-rewrite middleware
  (`add_s3_request_id`, ~100 LOC) that papers over residual
  spec gaps in the upstream crate.

Selection is via the `s3s-adapter` feature flag + the
`DGP_S3_ADAPTER` env var:

| Compile flag | Env var | Adapter |
|---|---|---|
| `s3s-adapter` ON | `DGP_S3_ADAPTER=s3s` (default) | s3s |
| `s3s-adapter` ON | `DGP_S3_ADAPTER=axum` | axum (rollback) |
| `s3s-adapter` OFF | (any) | axum (compile-time forced) |

Production Dockerfile builds without the feature — for now.

## Open work toward removing axum

In rough order of dependency:

1. **Get s3s into the production image.** Add
   `--features s3s-adapter` to the Dockerfile build invocation.
   This switches production traffic onto s3s while keeping the
   axum rollback available via env var.

2. ~~**Fix the form-POST routing on s3s.**~~ ✅ **DONE.** A
   method+content-type-aware middleware (`intercept_form_post_for_s3s`
   in `startup.rs::build_s3s_router`) intercepts only `POST /<bucket>`
   requests with `Content-Type: multipart/form-data` and dispatches
   to the same `handle_form_post_upload` the axum adapter uses.
   Other POSTs (DeleteObjects-XML, CreateMultipartUpload) fall
   through to the s3s service unchanged. The `using_s3s_adapter()`
   skip gate was removed from the 4 form-POST tests; they now run
   on both adapters and pass on both.

3. **Audit the `add_s3_request_id` XML-rewrite middleware**
   (`src/startup.rs::502+`). It exists to paper over three
   specific output-format drifts in the upstream s3s crate.
   Each drift should either be fixed upstream or moved into
   our DeltaGliderS3Service impl directly so the middleware
   can be deleted.

4. **Audit the parallel evaluators.** Five conditional-header
   evaluation functions exist (3 in s3s, 2 in axum). They
   should converge on shared pure-function helpers in
   `api/handlers/object_helpers.rs` so the protocol-spec logic
   lives in one place.

5. **Delete axum.** `src/api/handlers/{object,bucket,multipart,
   form_post}.rs`, `build_s3_router`, `using_s3s_adapter()`
   test gate, and the `s3s-adapter` feature flag (no longer
   needed). Reclaim ~3500 LOC of handler code plus ~400 LOC of
   the legacy router builder.

## Open work in s3s itself (parity gaps)

- ~~Form-POST upload~~ ✅ closed.
- Three XML output drifts patched by `add_s3_request_id`.
- Four `s3s_adapter_parity_test` tests pass today, but the
  test corpus is small. A full protocol-conformance fixture
  (s3-tests, AWS Java SDK integration tests, boto3) running
  against both adapters would surface remaining gaps.

## Why not switch the production Dockerfile right now

The four `s3s_adapter_parity_test` cases pass, but they only
cover a fraction of the wire surface. The conformance fixture
is the prerequisite. Once it's green on s3s, switching the
Dockerfile is a one-line change.

## What this means for new features

Until axum is removed: **every cross-adapter feature must land
on both adapters.** If the feature is axum-only (a typed-IAM
thing, a new admin API endpoint, a new metric), it's free.
If it's an S3 protocol change (a header, a status code, a new
operation), budget the work for both implementations + parity
tests.

If a new feature is *easier* to implement on s3s than axum,
that's a signal — accelerate the migration step that matters
and let axum lag.

## References

- `src/startup.rs::build_s3_router` — adapter selector.
- `src/s3_adapter_s3s.rs` — s3s implementation.
- `tests/s3s_adapter_parity_test.rs` — parity test suite.
- The `s3s` crate: <https://github.com/Nugine/s3s>.
