// SPDX-License-Identifier: BUSL-1.1

//! The `s3s` protocol adapter — THE production S3 implementation.
//!
//! `DeltaGliderS3Service` implements `s3s::S3` (~32 verb methods) and is mounted
//! as the axum `fallback_service` in `startup.rs::build_s3_router`. The migration
//! is complete: the legacy hand-rolled axum S3 handlers were retired, and `s3s`
//! is now the only S3 protocol surface. (`api/handlers/` retains only shared
//! state + the shapes s3s can't model — browser form-POST, health/stats.)
//!
//! Boundary contract:
//! - `s3s` owns HTTP/S3 parsing, generated DTOs, XML/error rendering, and
//!   protocol validation.
//! - DeltaGlider keeps all product logic: admission/IAM policy, compression,
//!   encryption wrappers, metadata cache, replication, metrics, and storage.

use crate::api::handlers::AppState;
use crate::deltaglider::RetrieveResponse;
use crate::iam::{AuthenticatedUser, ListScope, S3Action};
use crate::storage::StorageError;
use crate::types::FileMetadata;
use futures::stream::BoxStream;
use futures::Stream;
use futures::StreamExt;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::SystemTime;

mod bucket;
mod copy;
mod list;
mod multipart;
mod object;

// The verb groups share helpers through these imports (and `use super::*`).
use self::{bucket::*, list::*, object::*};
#[cfg(test)]
use self::{copy::*, multipart::*};
#[cfg(test)]
pub(crate) use copy::copy_source_bucket_key;
pub use list::{ListMetadataXmlExtensions, LEGACY_V2_TOKENS};

/// The `s3s::S3` service. Each verb is a one-line delegation to its verb
/// group (`bucket`, `object`, `list`, `multipart`, `copy`); a verb that is
/// not implemented keeps the s3s default `NotImplemented` answer.
#[derive(Clone)]
pub struct DeltaGliderS3Service {
    state: Arc<AppState>,
    config: crate::config::SharedConfig,
    /// The last filtered-LIST budget read from the config: what a LIST uses
    /// while a config apply holds the write lock.
    last_list_budget: Arc<std::sync::atomic::AtomicUsize>,
}

impl DeltaGliderS3Service {
    pub fn new(state: Arc<AppState>, config: crate::config::SharedConfig) -> Self {
        let budget = config.try_read().map_or_else(
            |_| crate::config::default_filtered_list_max_engine_pages(),
            |c| c.filtered_list_max_engine_pages,
        );
        Self {
            state,
            config,
            last_list_budget: Arc::new(budget.into()),
        }
    }

    /// The live filtered-LIST scan budget (`advanced.filtered_list_max_engine_pages`).
    /// Never waits for the config lock: a config apply holds the write lock
    /// while it probes the backends it changes, and a LIST that waited for
    /// it stalled every bucket for the probes. While the lock is held, the
    /// last value read is used.
    async fn list_budget(&self) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        match self.config.try_read() {
            Ok(cfg) => {
                let budget = cfg.filtered_list_max_engine_pages;
                self.last_list_budget.store(budget, Relaxed);
                budget
            }
            Err(_) => self.last_list_budget.load(Relaxed),
        }
    }

    /// Exposed for adapter tests and for the future router builder.
    pub fn state(&self) -> &Arc<AppState> {
        &self.state
    }

    /// Append an object-mutation event to the durable outbox (best-effort).
    ///
    /// This is what makes replication EVENT-DRIVEN: every successful PUT /
    /// DELETE / COPY / CompleteMultipartUpload publishes a fact here, which the
    /// replication consumer (and webhook delivery) drain. DG-internal keys
    /// (delta artifacts, dir markers, config-sync) are filtered so they never
    /// generate replication work. Noops silently when no config DB is present
    /// (open-mode dev) — same contract as the form-POST path.
    async fn emit_object_event(
        &self,
        kind: crate::event_outbox::EventKind,
        bucket: &str,
        key: &str,
        payload: serde_json::Value,
    ) {
        if !crate::replication::event_consumer::is_user_object_key(key) {
            return;
        }
        crate::api::handlers::object_helpers::enqueue_object_event(
            &self.state,
            crate::event_outbox::NewEvent::new(
                kind,
                bucket,
                key,
                crate::event_outbox::EventSource::S3Api,
                crate::replication::current_unix_seconds(),
                payload,
            ),
        )
        .await;
    }
}

#[async_trait::async_trait]
impl s3s::S3 for DeltaGliderS3Service {
    async fn head_bucket(
        &self,
        req: s3s::S3Request<s3s::dto::HeadBucketInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::HeadBucketOutput>> {
        bucket::head_bucket(self, req).await
    }

    async fn head_object(
        &self,
        req: s3s::S3Request<s3s::dto::HeadObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::HeadObjectOutput>> {
        object::head_object(self, req).await
    }

    async fn get_object(
        &self,
        req: s3s::S3Request<s3s::dto::GetObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetObjectOutput>> {
        object::get_object(self, req).await
    }

    async fn list_objects(
        &self,
        req: s3s::S3Request<s3s::dto::ListObjectsInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListObjectsOutput>> {
        list::list_objects(self, req).await
    }

    async fn list_objects_v2(
        &self,
        req: s3s::S3Request<s3s::dto::ListObjectsV2Input>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListObjectsV2Output>> {
        list::list_objects_v2(self, req).await
    }

    async fn list_buckets(
        &self,
        req: s3s::S3Request<s3s::dto::ListBucketsInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListBucketsOutput>> {
        bucket::list_buckets(self, req).await
    }

    async fn get_bucket_acl(
        &self,
        req: s3s::S3Request<s3s::dto::GetBucketAclInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetBucketAclOutput>> {
        bucket::get_bucket_acl(self, req).await
    }

    async fn get_bucket_location(
        &self,
        req: s3s::S3Request<s3s::dto::GetBucketLocationInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetBucketLocationOutput>> {
        bucket::get_bucket_location(self, req).await
    }

    async fn get_bucket_versioning(
        &self,
        req: s3s::S3Request<s3s::dto::GetBucketVersioningInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetBucketVersioningOutput>> {
        bucket::get_bucket_versioning(self, req).await
    }

    async fn get_bucket_tagging(
        &self,
        req: s3s::S3Request<s3s::dto::GetBucketTaggingInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetBucketTaggingOutput>> {
        bucket::get_bucket_tagging(self, req).await
    }

    async fn put_bucket_tagging(
        &self,
        req: s3s::S3Request<s3s::dto::PutBucketTaggingInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutBucketTaggingOutput>> {
        bucket::put_bucket_tagging(self, req).await
    }

    async fn put_bucket_acl(
        &self,
        req: s3s::S3Request<s3s::dto::PutBucketAclInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutBucketAclOutput>> {
        bucket::put_bucket_acl(self, req).await
    }

    async fn get_object_acl(
        &self,
        req: s3s::S3Request<s3s::dto::GetObjectAclInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetObjectAclOutput>> {
        object::get_object_acl(self, req).await
    }

    async fn put_object_acl(
        &self,
        req: s3s::S3Request<s3s::dto::PutObjectAclInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectAclOutput>> {
        object::put_object_acl(self, req).await
    }

    async fn get_object_tagging(
        &self,
        req: s3s::S3Request<s3s::dto::GetObjectTaggingInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetObjectTaggingOutput>> {
        object::get_object_tagging(self, req).await
    }

    async fn put_object_tagging(
        &self,
        req: s3s::S3Request<s3s::dto::PutObjectTaggingInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectTaggingOutput>> {
        object::put_object_tagging(self, req).await
    }

    async fn delete_object_tagging(
        &self,
        req: s3s::S3Request<s3s::dto::DeleteObjectTaggingInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::DeleteObjectTaggingOutput>> {
        object::delete_object_tagging(self, req).await
    }

    async fn create_bucket(
        &self,
        req: s3s::S3Request<s3s::dto::CreateBucketInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CreateBucketOutput>> {
        bucket::create_bucket(self, req).await
    }

    async fn delete_bucket(
        &self,
        req: s3s::S3Request<s3s::dto::DeleteBucketInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::DeleteBucketOutput>> {
        bucket::delete_bucket(self, req).await
    }

    async fn delete_object(
        &self,
        req: s3s::S3Request<s3s::dto::DeleteObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::DeleteObjectOutput>> {
        object::delete_object(self, req).await
    }

    async fn delete_objects(
        &self,
        req: s3s::S3Request<s3s::dto::DeleteObjectsInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::DeleteObjectsOutput>> {
        object::delete_objects(self, req).await
    }

    async fn put_object(
        &self,
        req: s3s::S3Request<s3s::dto::PutObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectOutput>> {
        object::put_object(self, req).await
    }

    async fn copy_object(
        &self,
        req: s3s::S3Request<s3s::dto::CopyObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CopyObjectOutput>> {
        copy::copy_object(self, req).await
    }

    async fn create_multipart_upload(
        &self,
        req: s3s::S3Request<s3s::dto::CreateMultipartUploadInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CreateMultipartUploadOutput>> {
        multipart::create_multipart_upload(self, req).await
    }

    async fn upload_part(
        &self,
        req: s3s::S3Request<s3s::dto::UploadPartInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::UploadPartOutput>> {
        multipart::upload_part(self, req).await
    }

    async fn abort_multipart_upload(
        &self,
        req: s3s::S3Request<s3s::dto::AbortMultipartUploadInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::AbortMultipartUploadOutput>> {
        multipart::abort_multipart_upload(self, req).await
    }

    async fn list_parts(
        &self,
        req: s3s::S3Request<s3s::dto::ListPartsInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListPartsOutput>> {
        multipart::list_parts(self, req).await
    }

    async fn complete_multipart_upload(
        &self,
        req: s3s::S3Request<s3s::dto::CompleteMultipartUploadInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CompleteMultipartUploadOutput>> {
        multipart::complete_multipart_upload(self, req).await
    }

    async fn list_multipart_uploads(
        &self,
        req: s3s::S3Request<s3s::dto::ListMultipartUploadsInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListMultipartUploadsOutput>> {
        multipart::list_multipart_uploads(self, req).await
    }

    async fn upload_part_copy(
        &self,
        req: s3s::S3Request<s3s::dto::UploadPartCopyInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::UploadPartCopyOutput>> {
        copy::upload_part_copy(self, req).await
    }
}

struct SyncStorageStream {
    inner: Mutex<BoxStream<'static, Result<bytes::Bytes, StorageError>>>,
}

impl SyncStorageStream {
    fn new(inner: BoxStream<'static, Result<bytes::Bytes, StorageError>>) -> Self {
        Self {
            inner: Mutex::new(inner),
        }
    }
}

impl Stream for SyncStorageStream {
    type Item = Result<bytes::Bytes, s3s::StdError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut guard = self.inner.lock().expect("stream mutex poisoned");
        Pin::new(&mut *guard)
            .poll_next(cx)
            .map_err(|e| Box::new(e) as s3s::StdError)
    }
}

impl s3s::stream::ByteStream for SyncStorageStream {}

/// True when s3s already removed the aws-chunked framing. It does so for
/// every SigV4 HEADER-signed streaming request (it verifies each chunk
/// signature while it decodes). Decoding again made every signed SDK
/// streaming PUT / UploadPart fail with 400 (review C1). Unsigned or
/// presigned streaming bodies still reach us framed.
fn s3s_decoded_aws_chunked(headers: &axum::http::HeaderMap, has_credentials: bool) -> bool {
    has_credentials
        && headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("AWS4-HMAC-SHA256 "))
}

/// The headers `collect_blob_limited` needs to decode aws-chunked framing,
/// or `None` when s3s already decoded the body.
fn headers_if_still_aws_chunked<T>(req: &s3s::S3Request<T>) -> Option<axum::http::HeaderMap> {
    (!s3s_decoded_aws_chunked(&req.headers, req.credentials.is_some())).then(|| req.headers.clone())
}

async fn collect_blob_limited(
    body: Option<s3s::dto::StreamingBlob>,
    limit: u64,
    headers: Option<&axum::http::HeaderMap>,
) -> s3s::S3Result<bytes::Bytes> {
    let Some(mut body) = body else {
        return Ok(bytes::Bytes::new());
    };
    let mut out = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| {
            // s3s wraps the request body in a hashing reader that
            // verifies `x-amz-content-sha256` AS THE BODY STREAMS.
            // On mismatch, the chunk read fails with
            // `UploadStreamError::Sha256Mismatch` (Display string
            // `"UploadStreamError: Sha256Mismatch"`). That IS the
            // H1 integrity check the axum adapter performs after
            // collecting the body — surface it as the same wire
            // error (`BadDigest`, 400) instead of a generic 500
            // InternalError. Match-on-Display is fragile because
            // `UploadStreamError` is private to s3s, but the
            // alternative (downcast) needs the type to be public.
            // If the s3s crate ever exports the type or renames the
            // variant, the test `test_sigv4_payload_hash_mismatch_rejected`
            // catches the drift.
            let msg = e.to_string();
            if msg.contains("Sha256Mismatch") {
                tracing::warn!(
                    "request body sha256 doesn't match x-amz-content-sha256 header — rejecting as BadDigest"
                );
                return s3s::s3_error!(BadDigest);
            }
            tracing::error!(error = ?e, "collect_blob_limited: body chunk read failed");
            s3s::s3_error!(InternalError, "failed to read request body: {e}")
        })?;
        let next_len = out
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| s3s::s3_error!(EntityTooLarge))?;
        if next_len as u64 > limit {
            return Err(s3s::s3_error!(EntityTooLarge));
        }
        out.extend_from_slice(&chunk);
    }
    let body = bytes::Bytes::from(out);
    if let Some(headers) = headers {
        if crate::api::aws_chunked::is_aws_chunked(headers) {
            let expected_len = crate::api::aws_chunked::get_decoded_content_length(headers);
            if expected_len.is_some_and(|len| len as u64 > limit) {
                return Err(s3s::s3_error!(EntityTooLarge));
            }
            // Require x-amz-decoded-content-length for chunked bodies: without it
            // the decoder can only frame-check, and a body truncated at a chunk
            // boundary (with a surviving/crafted 0\r\n\r\n terminator) would pass
            // framing yet store short. AWS SDKs always send this header for
            // streaming uploads, so requiring it rejects only malformed inputs.
            if expected_len.is_none() {
                return Err(s3s::s3_error!(
                    InvalidArgument,
                    "aws-chunked transfer encoding requires x-amz-decoded-content-length"
                ));
            }
            return crate::api::aws_chunked::decode_aws_chunked(&body, expected_len).ok_or_else(
                || {
                    s3s::s3_error!(
                        InvalidArgument,
                        "Failed to decode AWS chunked transfer encoding"
                    )
                },
            );
        }
    }
    Ok(body)
}

async fn ensure_bucket_exists_s3s(state: &Arc<AppState>, bucket: &str) -> s3s::S3Result<()> {
    ensure_bucket_on(&state.engine.load(), bucket).await
}

async fn ensure_bucket_on(
    engine: &crate::deltaglider::DynEngine,
    bucket: &str,
) -> s3s::S3Result<()> {
    if engine.head_bucket(bucket).await? {
        Ok(())
    } else {
        Err(s3s::s3_error!(NoSuchBucket))
    }
}

/// S3 answers any request to a missing bucket with `NoSuchBucket`; the
/// proxy answered `NoSuchKey` (GET, HEAD) or success (DELETE) (s3surface-5).
/// Asked only after a miss, so a hit pays no extra bucket request.
async fn no_such_key_or_bucket(
    engine: &crate::deltaglider::DynEngine,
    bucket: &str,
    err: s3s::S3Error,
) -> s3s::S3Error {
    if *err.code() != s3s::S3ErrorCode::NoSuchKey {
        return err;
    }
    ensure_bucket_on(engine, bucket).await.err().unwrap_or(err)
}

/// Policy context for the per-key checks the adapter runs itself (batch
/// delete, copy-source read). The middleware authorizes only the
/// request line; without `aws:SourceIp` here an IP-conditioned Deny is
/// skipped (X-ray H17, review S14). The context-free `can()` is `cfg(test)`,
/// so production code cannot call it.
fn request_policy_context(ext: &axum::http::Extensions) -> iam_rs::Context {
    policy_context_for_ip(ext.get::<crate::api::auth::RequestClientIp>().map(|c| c.0))
}

fn policy_context_for_ip(client_ip: Option<std::net::IpAddr>) -> iam_rs::Context {
    let mut context = iam_rs::Context::new();
    crate::iam::permissions::insert_source_ip(&mut context, client_ip);
    context
}

fn validate_content_md5_s3s(content_md5: Option<&str>, body: &[u8]) -> s3s::S3Result<()> {
    let Some(content_md5) = content_md5 else {
        return Ok(());
    };
    use base64::Engine as _;
    use md5::Digest as _;
    let expected = base64::engine::general_purpose::STANDARD
        .decode(content_md5.trim())
        .map_err(|_| s3s::s3_error!(InvalidDigest))?;
    let actual = md5::Md5::digest(body);
    if actual.as_slice() != expected.as_slice() {
        return Err(s3s::s3_error!(BadDigest));
    }
    Ok(())
}

fn query_flag(uri: &axum::http::Uri, key: &str, expected: &str) -> bool {
    uri.query()
        .map(|query| {
            query.split('&').any(|part| {
                part.split_once('=')
                    .map(|(k, v)| k == key && v.eq_ignore_ascii_case(expected))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn check_user_metadata_size_s3s(
    metadata: Option<&std::collections::HashMap<String, String>>,
) -> s3s::S3Result<()> {
    let Some(metadata) = metadata else {
        return Ok(());
    };
    crate::api::handlers::object_helpers::user_metadata_size_check(metadata).map_err(|size| {
        s3s::s3_error!(
            MetadataTooLarge,
            "{}",
            crate::api::handlers::object_helpers::user_metadata_too_large_message(size)
        )
    })
}

/// Who is reading an object's metadata, as far as provenance disclosure is
/// concerned. Computed once per request from the request extensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reader {
    /// A caller who holds credentials (SigV4 with a real principal), or
    /// open-access mode (`authentication: none`, no principal at all — a
    /// deliberate dev/test setting where every byte is public already and
    /// the proxy's own tooling reads the `dg-*` keys).
    Authenticated,
    /// The synthesized `$anonymous` public-prefix principal, or the holder of
    /// a presigned URL (authenticated as the signer, but the link holder is an
    /// anonymous party — the docs recommend presigned links for third parties).
    Anonymous,
}

impl Reader {
    fn of(ext: &axum::http::Extensions) -> Self {
        let anonymous_principal = ext
            .get::<AuthenticatedUser>()
            .is_some_and(|u| u.is_anonymous());
        if anonymous_principal || ext.get::<crate::api::auth::PresignedRequest>().is_some() {
            Self::Anonymous
        } else {
            Self::Authenticated
        }
    }
}

/// Drop deployment provenance from metadata bound for an anonymous reader.
/// Every object the proxy stores carries `dg-tool = deltaglider_proxy/<version>`;
/// an anonymous reader must not learn the exact running build from a HEAD,
/// GET, or `metadata=true` LIST. Handles both the bare-key map (HEAD/GET)
/// and the `x-amz-meta-*` map (LIST extension).
fn strip_fingerprint_metadata(
    metadata: &mut std::collections::HashMap<String, String>,
    reader: Reader,
) {
    if reader == Reader::Authenticated {
        return;
    }
    metadata.remove(crate::types::meta_keys::TOOL);
    metadata.remove(crate::types::meta_keys::H_TOOL);
}

/// The user-visible metadata map for HEAD/GET, already scrubbed for `reader`.
/// Every HEAD/GET output builder goes through here, so a new output path
/// cannot forget the provenance strip.
fn response_metadata_map(
    meta: &FileMetadata,
    reader: Reader,
) -> std::collections::HashMap<String, String> {
    let mut map = meta.to_bare_metadata_map();
    map.remove("content-type");
    map.retain(|key, _| !key.starts_with("user-"));
    for (key, value) in &meta.user_metadata {
        if !key.to_lowercase().starts_with("dg-") {
            map.insert(key.clone(), value.clone());
        }
    }
    strip_fingerprint_metadata(&mut map, reader);
    map
}

/// `DGP_DEBUG_HEADERS`, as the running engine was built with it.
fn debug_headers_enabled(svc: &DeltaGliderS3Service) -> bool {
    svc.state.engine.load().tuning().debug_headers
}

fn add_storage_debug_headers(
    svc: &DeltaGliderS3Service,
    headers: &mut axum::http::HeaderMap,
    meta: &FileMetadata,
) {
    if !debug_headers_enabled(svc) {
        return;
    }
    if let Ok(value) = axum::http::HeaderValue::from_str(meta.storage_info.label()) {
        headers.insert("x-amz-storage-type", value);
    }
    let stored_size = meta.stored_size();
    if let Ok(value) = axum::http::HeaderValue::from_str(&stored_size.to_string()) {
        headers.insert("x-deltaglider-stored-size", value);
    }
}

/// `x-deltaglider-listing-facts-misses`: how many entries of a LIST page
/// show only their stored size, because the logical size was in neither
/// this process's cache nor the listing facts (debug headers only).
fn add_listing_debug_headers(
    svc: &DeltaGliderS3Service,
    headers: &mut axum::http::HeaderMap,
    facts_misses: usize,
) {
    if debug_headers_enabled(svc) {
        headers.insert(
            "x-deltaglider-listing-facts-misses",
            axum::http::HeaderValue::from(facts_misses),
        );
    }
}

fn parse_s3s_etag(etag: &str) -> s3s::S3Result<s3s::dto::ETag> {
    etag.parse::<s3s::dto::ETag>()
        .map_err(|_| s3s::s3_error!(InternalError, "invalid metadata ETag"))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod review2_tests;

#[cfg(test)]
mod delete_bucket_tests;

#[cfg(test)]
mod token_and_csp_proptests;

#[cfg(test)]
mod list_budget_tests {
    use super::*;

    /// A config apply holds the config write lock while it probes the
    /// backends it changes. Every ListObjects and ListObjectsV2 read its
    /// page budget through that lock, so every LIST on every bucket waited
    /// for the probes.
    #[tokio::test]
    async fn the_list_budget_does_not_wait_for_a_config_apply() {
        let dir = tempfile::tempdir().unwrap();
        let backend: Box<crate::storage::DynStorageBackend<'static>> =
            crate::storage::DynStorageBackend::new_box(
                crate::storage::FilesystemBackend::new(dir.path().to_path_buf())
                    .await
                    .unwrap(),
            );
        let cfg = crate::config::Config {
            filtered_list_max_engine_pages: 7,
            ..Default::default()
        };
        let engine =
            crate::deltaglider::DeltaGliderEngine::new_with_backend(Arc::new(backend), &cfg, None);
        let config = cfg.into_shared();
        let svc = DeltaGliderS3Service::new(AppState::for_tests(engine), config.clone());
        assert_eq!(svc.list_budget().await, 7);

        let apply = config.write().await;
        let got =
            tokio::time::timeout(std::time::Duration::from_millis(100), svc.list_budget()).await;
        drop(apply);
        assert_eq!(got.ok(), Some(7), "a LIST waited for the config write lock");
    }

    /// Source guard: the S3 request path never awaits the config lock. A
    /// config apply holds the write lock while it probes the backends it
    /// changes, so a request that waited for it stalled every bucket for
    /// the probes. Read the config with `try_read` (and a fallback), or
    /// from a snapshot that the engine was built with.
    #[test]
    fn the_s3_request_path_never_awaits_the_config_lock() {
        let mut offenders = Vec::new();
        for dir in [
            "src/s3_adapter_s3s",
            "src/api/handlers",
            "src/admission",
            "src/api/auth.rs",
            "src/api/s3_router.rs",
            "src/api/s3s_hooks.rs",
            "src/iam/middleware.rs",
            "src/maintenance/gate.rs",
        ] {
            let sources: Vec<(String, String)> = if dir.ends_with(".rs") {
                vec![(dir.to_string(), crate::source_scan::read(dir))]
            } else {
                crate::source_scan::prod_sources(dir)
            };
            for (rel, text) in sources {
                for (n, line) in crate::source_scan::prod_lines(&text) {
                    let code = line.split("//").next().unwrap_or("");
                    if code.contains("config.read().await") || code.contains("config.write().await")
                    {
                        offenders.push(format!("{rel}:{n}: {}", line.trim()));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "the S3 request path awaits the config lock:\n{}",
            offenders.join("\n")
        );
    }
}
