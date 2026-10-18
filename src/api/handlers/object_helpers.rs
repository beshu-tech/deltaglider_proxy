// SPDX-License-Identifier: BUSL-1.1

//! Shared object-write helpers.
//!
//! Pre-consolidation this module hosted ~1200 LOC of axum-handler
//! internals (range parsing, conditional headers, body decoding,
//! PUT/COPY/multipart implementations). With the axum S3 path
//! retired in favour of `s3_adapter_s3s`, only the bits that BOTH
//! the s3s adapter and the surviving form-POST handler need are
//! kept here:
//!
//! * [`store_client_write`] — THE client write of a whole body: quota
//!   gate, conditional store under the object's write lock, event.
//! * [`store_client_delta_copy`] — the same for a CopyObject of a delta
//!   that ships the `.delta` verbatim.
//! * [`store_client_server_copy`] — the same for a CopyObject that the
//!   backend copies server-side.
//! * [`store_client_multipart_admit`] / [`store_client_multipart_commit`]
//!   — the same quota gate, write lock and event for a multipart completion.
//! * [`check_quota_for_write`] — pre-write quota gate.
//! * [`enqueue_object_event`] / [`enqueue_object_events`] — best-
//!   effort event-outbox append for notification dispatch; never waits
//!   for the jobs DB mutex (a full writer queue makes it wait).
//!
//! Everything else moved into the s3s adapter or was already
//! axum-handler-specific and went away with `object.rs` /
//! `bucket.rs` / `multipart.rs`.

use super::AppState;
use crate::api::errors::S3Error;
use crate::event_outbox::NewEvent;
use crate::storage::StorageError;
use std::sync::Arc;

/// Append a single object event to the outbox. Silently noops when
/// no jobs DB is attached (open-mode dev runs). Errors are
/// warn-logged and dropped — notifications are best-effort by design.
/// Never waits for the jobs DB mutex; waits only when the writer's
/// queue is full (see [`crate::event_outbox::append_events`]).
pub(crate) async fn enqueue_object_event(state: &Arc<AppState>, event: NewEvent) {
    enqueue_object_events(state, &[event]).await;
}

/// Batched variant of [`enqueue_object_event`].
pub(crate) async fn enqueue_object_events(state: &Arc<AppState>, events: &[NewEvent]) {
    // Drop events for DG-internal keys (reference.bin, dir markers, staging
    // routes) at the single enqueue chokepoint. The s3s adapter filters before
    // calling, but the form-POST path enqueued directly — without this a raw
    // webhook could fire for a `.dg/reference.bin` write. Idempotent double-filter.
    let filtered: Vec<NewEvent> = events
        .iter()
        .filter(|e| crate::replication::event_consumer::is_user_object_key(&e.key))
        .cloned()
        .collect();
    let Some(jobs_db) = state.jobs_db.as_ref() else {
        return;
    };
    crate::event_outbox::append_events(jobs_db, filtered).await;
}

/// One client write of a whole body (PutObject, CopyObject, form POST).
pub(crate) struct ClientWrite<'a> {
    pub bucket: &'a str,
    pub key: &'a str,
    /// The body as collected: shared, never copied, down to the storage write.
    pub data: bytes::Bytes,
    pub content_type: Option<String>,
    pub user_metadata: std::collections::HashMap<String, String>,
    pub precondition: &'a crate::deltaglider::Precondition,
}

/// THE client write of a whole body: the quota gate, then the engine's
/// conditional store (the object's write lock, the preconditions, the
/// store), then the `ObjectCreated` event. Every surface that stores a
/// client body goes through here, so none of them skips the lock, the
/// conditionals, the quota or the event.
pub(crate) async fn store_client_write(
    state: &Arc<AppState>,
    write: ClientWrite<'_>,
) -> Result<crate::types::StoreResult, S3Error> {
    store_client_write_hashed(state, write, None).await
}

/// [`store_client_write`] of a body whose SHA-256 the caller proved
/// (`known_sha256`): the engine stores that digest and does not hash the
/// body for it again.
pub(crate) async fn store_client_write_hashed(
    state: &Arc<AppState>,
    write: ClientWrite<'_>,
    known_sha256: Option<crate::deltaglider::KnownSha256>,
) -> Result<crate::types::StoreResult, S3Error> {
    let size = write.data.len() as u64;
    check_quota_for_write(state, write.bucket, write.key, size).await?;
    let result = state
        .engine
        .load()
        .store_conditional(
            write.bucket,
            write.key,
            write.data,
            known_sha256,
            write.content_type,
            write.user_metadata,
            write.precondition,
        )
        .await?;
    object_created(state, write.bucket, write.key, size, &result).await;
    Ok(result)
}

/// One client write of a body staged in a spool file (a CopyObject of a
/// source above the spool threshold).
pub(crate) struct SpooledClientWrite<'a> {
    pub bucket: &'a str,
    pub key: &'a str,
    pub spool: &'a crate::deltaglider::spool::Spool,
    pub size: u64,
    pub content_type: Option<String>,
    pub user_metadata: std::collections::HashMap<String, String>,
    pub precondition: &'a crate::deltaglider::Precondition,
}

/// [`store_client_write`] of a spooled body: the same quota gate, write
/// lock, preconditions and event, and the streaming store, so the body
/// never comes into memory.
pub(crate) async fn store_client_write_spooled(
    state: &Arc<AppState>,
    write: SpooledClientWrite<'_>,
) -> Result<crate::types::StoreResult, S3Error> {
    check_quota_for_write(state, write.bucket, write.key, write.size).await?;
    let engine = state.engine.load();
    let result = {
        let guard = engine
            .lock_and_check(write.bucket, write.key, write.precondition)
            .await?;
        engine
            .store_spooled_delta(
                &guard,
                write.bucket,
                write.key,
                write.spool,
                write.size,
                write.content_type,
                write.user_metadata,
                None,
            )
            .await?
    };
    object_created(state, write.bucket, write.key, write.size, &result).await;
    Ok(result)
}

/// A client CopyObject that copies the stored object as it is (see
/// [`store_client_delta_copy`], [`store_client_server_copy`]).
pub(crate) struct ClientStoredCopy<'a> {
    pub source: (&'a str, &'a str),
    pub bucket: &'a str,
    pub key: &'a str,
    /// The source HEAD that the copy-source conditionals judged.
    pub source_head: &'a crate::types::FileMetadata,
    pub content_type: Option<String>,
    pub user_metadata: std::collections::HashMap<String, String>,
    pub precondition: &'a crate::deltaglider::Precondition,
}

/// [`store_client_write`] of a CopyObject whose source is a delta: the
/// same quota gate (on the logical size), write lock, preconditions and
/// event, but the `.delta` ships verbatim
/// ([`crate::transfer::client_delta_copy`]): no reconstruction and no
/// re-encode. `Ok(None)`: the fast path does not apply (another
/// reference at the destination, an incompatible encryption key); the
/// caller copies through [`store_client_write`], after this lock is
/// released.
pub(crate) async fn store_client_delta_copy(
    state: &Arc<AppState>,
    copy: ClientStoredCopy<'_>,
) -> Result<Option<crate::types::StoreResult>, S3Error> {
    let size = copy.source_head.file_size;
    check_quota_for_write(state, copy.bucket, copy.key, size).await?;
    let engine = state.engine.load_full();
    let result = {
        let guard = engine
            .lock_and_check(copy.bucket, copy.key, copy.precondition)
            .await?;
        crate::transfer::client_delta_copy(
            &engine,
            &guard,
            copy.source,
            (copy.bucket, copy.key),
            copy.source_head,
            copy.content_type,
            copy.user_metadata,
        )
        .await
        .map_err(crate::transfer::CopyError::into_s3_error)?
    };
    let Some(result) = result else {
        return Ok(None);
    };
    object_created(state, copy.bucket, copy.key, size, &result).await;
    Ok(Some(result))
}

/// [`store_client_write`] of a CopyObject of a passthrough source into a
/// key that stores it as it is: the same quota gate, write lock,
/// preconditions and event, but the backend copies the stored file
/// ([`crate::deltaglider::DynEngine::copy_passthrough_within`]); no byte
/// passes through the proxy. `Ok(None)`: the backend cannot copy it
/// (another backend or key, no CopyObject on the provider); the caller
/// copies through [`store_client_write`], after this lock is released.
pub(crate) async fn store_client_server_copy(
    state: &Arc<AppState>,
    copy: ClientStoredCopy<'_>,
) -> Result<Option<crate::types::StoreResult>, S3Error> {
    let size = copy.source_head.file_size;
    let Some(metadata) = crate::transfer::destination_metadata(
        copy.source_head,
        crate::types::ObjectKey::parse(copy.bucket, copy.key)
            .filename
            .as_str(),
        crate::transfer::DestinationFields {
            content_type: copy.content_type,
            user_metadata: copy.user_metadata,
            multipart_etag: None,
        },
    ) else {
        return Ok(None);
    };
    check_quota_for_write(state, copy.bucket, copy.key, size).await?;
    let engine = state.engine.load_full();
    let result = {
        let guard = engine
            .lock_and_check(copy.bucket, copy.key, copy.precondition)
            .await?;
        engine
            .copy_passthrough_within(
                &guard,
                copy.source,
                (copy.bucket, copy.key),
                copy.source_head,
                metadata,
                None,
            )
            .await
            .map_err(|e| match e {
                crate::deltaglider::EngineError::Storage(StorageError::PreconditionFailed(_)) => {
                    S3Error::SlowDown(
                        "the copy source changed during the copy; retry the request".to_string(),
                    )
                }
                e => e.into(),
            })?
    };
    let Some(result) = result else {
        return Ok(None);
    };
    object_created(state, copy.bucket, copy.key, size, &result).await;
    Ok(Some(result))
}

/// The `ObjectCreated` event of a client write.
async fn object_created(
    state: &Arc<AppState>,
    bucket: &str,
    key: &str,
    content_length: u64,
    result: &crate::types::StoreResult,
) {
    enqueue_object_event(
        state,
        object_created_event(bucket, key, content_length, result),
    )
    .await;
}

/// The admission of a client multipart completion, the multipart half of
/// [`store_client_write`]: the quota gate, then the object's write lock
/// with the preconditions (`If-None-Match: *` is create-only). The caller
/// holds the lock until the store ends, then calls
/// [`store_client_multipart_commit`].
pub(crate) async fn store_client_multipart_admit(
    state: &Arc<AppState>,
    bucket: &str,
    key: &str,
    total_size: u64,
    precondition: &crate::deltaglider::Precondition,
) -> Result<crate::deltaglider::ObjectWriteGuard, S3Error> {
    check_quota_for_write(state, bucket, key, total_size).await?;
    Ok(state
        .engine
        .load()
        .lock_and_check(bucket, key, precondition)
        .await?)
}

/// After a client multipart completion stored: the `ObjectCreated` event,
/// with the same payload as [`store_client_write`].
pub(crate) async fn store_client_multipart_commit(
    state: &Arc<AppState>,
    bucket: &str,
    key: &str,
    result: &crate::types::StoreResult,
) {
    enqueue_object_event(
        state,
        object_created_event(bucket, key, result.metadata.file_size, result),
    )
    .await;
}

/// THE `ObjectCreated` event of a client write (whole body or multipart).
fn object_created_event(
    bucket: &str,
    key: &str,
    content_length: u64,
    result: &crate::types::StoreResult,
) -> NewEvent {
    NewEvent::new(
        crate::event_outbox::EventKind::ObjectCreated,
        bucket,
        key,
        crate::event_outbox::EventSource::S3Api,
        crate::replication::current_unix_seconds(),
        serde_json::json!({
            "content_length": content_length,
            "storage_type": result.metadata.storage_info.label(),
            "etag": result.metadata.etag(),
        }),
    )
}

/// Client-write boundary gate. A bucket marked `replication_target_only`
/// refuses ALL client writes (PUT/DELETE/multipart/copy-into/admin bulk)
/// with 403 so replication remains the single writer — the property that
/// makes a non-CAS backend (e.g. B2) safe as a mirror. Replication,
/// lifecycle, and maintenance call the engine directly and never pass
/// through this gate.
pub(crate) fn check_client_write_allowed(
    state: &Arc<AppState>,
    bucket: &str,
) -> Result<(), S3Error> {
    let engine = state.engine.load();
    // Blocks the configured marked name AND an unconfigured name that resolves
    // onto marked storage (the alias hole). Registry lowercases internally.
    if let Some(reason) = engine
        .bucket_policy_registry()
        .client_write_block_reason(bucket)
    {
        return Err(S3Error::AccessDeniedReason(reason));
    }
    Ok(())
}

/// Pre-write quota gate of a write of `key`. Returns `Err` when the write
/// would push the bucket past its `quota_bytes` policy, or when quota is
/// set to 0 (the "freeze the bucket" override). "Used" is [`quota_used`].
/// An overwrite replaces the stored bytes of the object it replaces, so a
/// write that the gate refuses as a new object is judged again with the
/// prior object's stored size netted out. Only that refusal pays the HEAD
/// of the prior object. Before, an overwrite at the limit was refused even
/// when it added nothing.
pub(crate) async fn check_quota_for_write(
    state: &Arc<AppState>,
    bucket: &str,
    key: &str,
    incoming_bytes: u64,
) -> Result<(), S3Error> {
    let engine = state.engine.load();
    let Some(quota) = engine.bucket_policy_registry().quota_bytes(bucket) else {
        return Ok(());
    };
    let used = quota_used(state, bucket);
    let Err(reason) = quota_decision(quota, used, incoming_bytes) else {
        return Ok(());
    };
    if quota == 0 {
        // Frozen: no overwrite gets through either.
        return Err(S3Error::AccessDeniedReason(reason));
    }
    let prior_stored = match engine.head(bucket, key).await {
        Ok(meta) => crate::bucket_usage::usage_delta_for(&meta, 1).2.max(0) as u64,
        // Absent, or unknown: judged as a new object.
        Err(_) => 0,
    };
    if prior_stored > 0
        && quota_decision(
            quota,
            used.map(|u| u.saturating_sub(prior_stored)),
            incoming_bytes,
        )
        .is_ok()
    {
        return Ok(());
    }
    Err(S3Error::AccessDeniedReason(reason))
}

/// The STORED bytes a quota check counts (delta baselines included; the
/// LOGICAL size is the wrong figure for a storage quota).
///
/// The O(1) running counter (`bucket_usage`) is trusted only after a full
/// scan stamped its row (`last_scan_at`). A never-scanned row holds only
/// the writes since the counter started: data that was in the bucket before
/// (a bucket adopted in place, data from before the counter, writes through
/// another instance) is missing, and the bucket could grow past its quota
/// by all of it. Then the usage scanner's size counts too (a cached scan,
/// up to 5 minutes old; a missing one starts a background scan), and the
/// larger of the two wins. With neither source warm the write is let
/// through optimistically.
fn quota_used(state: &Arc<AppState>, bucket: &str) -> Option<u64> {
    let row = state
        .bucket_usage
        .as_ref()
        .and_then(|u| u.read(bucket).ok().flatten());
    if let Some(row) = row.filter(|r| r.last_scan_at.is_some()) {
        return Some(row.stored_bytes);
    }
    let counted = row.map(|r| r.stored_bytes);
    let scanned = state
        .usage_scanner
        .get_or_scan(state, bucket, "")
        .map(|u| u.stored_size);
    counted.max(scanned)
}

/// 1024-based, so IEC labels: the quota field in the admin GUI says GiB.
fn human_bytes(b: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if b >= 1024 * MIB {
        format!("{:.1} GiB", b as f64 / (1024 * MIB) as f64)
    } else if b >= MIB {
        format!("{:.1} MiB", b as f64 / MIB as f64)
    } else {
        format!("{} KiB", b.div_ceil(1024))
    }
}

/// Pure quota verdict. `Err` carries the operator-facing reason; the caller
/// maps it to 403 AccessDenied (the documented status — it used to be a 500
/// InternalError, which S3 SDKs retry as a server fault).
pub(crate) fn quota_decision(quota: u64, used: Option<u64>, incoming: u64) -> Result<(), String> {
    if quota == 0 {
        return Err("Bucket is frozen (quota = 0)".into());
    }
    match used {
        // Say why THIS write fails: "24 MB used of 25 MB" alone reads as
        // if there were still room.
        Some(used) if used.saturating_add(incoming) > quota => Err(format!(
            "Bucket quota exceeded: {} used + {} upload > {} limit",
            human_bytes(used),
            human_bytes(incoming),
            human_bytes(quota),
        )),
        _ => Ok(()),
    }
}

/// S3's limit on user-defined metadata: the UTF-8 bytes of every
/// `x-amz-meta-*` key (without the prefix) and value, summed.
pub(crate) const USER_METADATA_MAX_BYTES: usize = 2048;

/// THE client message for user metadata over the limit (PUT, copy, multipart,
/// form POST), sent with the S3 code `MetadataTooLarge`.
pub(crate) fn user_metadata_too_large_message(size: usize) -> String {
    format!("user metadata is {size} bytes; the limit is {USER_METADATA_MAX_BYTES} bytes")
}

/// Pure metadata-size verdict for every client write that carries user
/// metadata (PUT, CreateMultipartUpload, CopyObject REPLACE, form POST).
/// Oversized metadata used to reach storage and fail there: 500 on S3
/// (header too large) and ENOSPC ("disk full") from xattrs on the
/// filesystem backend (review C8). `Err` carries the measured size.
pub(crate) fn user_metadata_size_check(
    metadata: &std::collections::HashMap<String, String>,
) -> Result<(), usize> {
    let size: usize = metadata.iter().map(|(k, v)| k.len() + v.len()).sum();
    if size > USER_METADATA_MAX_BYTES {
        Err(size)
    } else {
        Ok(())
    }
}

/// Pure verdict: can every GET and HEAD of the object send these values
/// back as response headers? A PutObject reads them from request headers,
/// so they are valid there by construction. A form POST reads them from
/// form fields, which can hold any text: a metadata name with a space or a
/// `Content-Type` with CR/LF was stored, and then every GET and HEAD of the
/// object failed with 500 because s3s could not build the header. A
/// metadata VALUE needs no check, because s3s RFC 2047-encodes any value
/// that is not a safe header value. `Err` names the bad field.
pub(crate) fn stored_header_fields_check(
    metadata: &std::collections::HashMap<String, String>,
    content_type: Option<&str>,
) -> Result<(), String> {
    for name in metadata.keys() {
        let header = format!("x-amz-meta-{name}");
        if axum::http::HeaderName::from_bytes(header.as_bytes()).is_err() {
            return Err(format!("'{header}' is not a valid HTTP header name"));
        }
    }
    // ASCII only, like PutObject: s3s reads a request header with
    // `to_str`, which refuses other bytes.
    if let Some(value) = content_type {
        if !value.is_ascii() || axum::http::HeaderValue::from_str(value).is_err() {
            return Err("Content-Type is not a valid HTTP header value".to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod stored_header_fields_tests {
    use super::stored_header_fields_check;
    use std::collections::HashMap;

    fn meta(name: &str) -> HashMap<String, String> {
        HashMap::from([(name.to_string(), "v".to_string())])
    }

    #[test]
    fn names_must_be_header_tokens() {
        for good in ["a", "build-id", "x_y.z", "a1"] {
            assert_eq!(
                stored_header_fields_check(&meta(good), None),
                Ok(()),
                "{good}"
            );
        }
        for bad in ["a b", "a\r\nb", "a:b", "a\"b", "é", "a(b)"] {
            assert!(
                stored_header_fields_check(&meta(bad), None).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn values_need_no_check() {
        let m = HashMap::from([("k".to_string(), "line\r\nbreak é".to_string())]);
        assert_eq!(stored_header_fields_check(&m, None), Ok(()));
    }

    #[test]
    fn content_type_must_be_a_header_value() {
        let none = HashMap::new();
        for good in ["text/plain", "text/plain; charset=utf-8", "a\tb"] {
            assert_eq!(
                stored_header_fields_check(&none, Some(good)),
                Ok(()),
                "{good}"
            );
        }
        for bad in ["text/plain\r\nX-Evil: 1", "a\nb", "a\0b", "tëxt/plain"] {
            assert!(
                stored_header_fields_check(&none, Some(bad)).is_err(),
                "{bad:?}"
            );
        }
    }
}

#[cfg(test)]
mod metadata_size_tests {
    use super::{user_metadata_size_check, USER_METADATA_MAX_BYTES};
    use std::collections::HashMap;

    #[test]
    fn limit_counts_keys_and_values() {
        let at_limit = HashMap::from([("k".to_string(), "v".repeat(USER_METADATA_MAX_BYTES - 1))]);
        assert_eq!(user_metadata_size_check(&at_limit), Ok(()));
        let over = HashMap::from([("kk".to_string(), "v".repeat(USER_METADATA_MAX_BYTES - 1))]);
        assert_eq!(
            user_metadata_size_check(&over),
            Err(USER_METADATA_MAX_BYTES + 1)
        );
        assert_eq!(user_metadata_size_check(&HashMap::new()), Ok(()));
    }
}

#[cfg(test)]
mod quota_tests {
    use super::quota_decision;

    #[test]
    fn frozen_bucket_rejects_everything() {
        assert!(quota_decision(0, None, 0).is_err());
        assert!(quota_decision(0, Some(0), 1).is_err());
    }

    #[test]
    fn cold_usage_is_optimistic() {
        assert!(quota_decision(1, None, 10_000).is_ok());
    }

    #[test]
    fn enforces_against_used_plus_incoming() {
        assert!(
            quota_decision(100, Some(50), 50).is_ok(),
            "exactly at the limit is allowed"
        );
        let e = quota_decision(100, Some(50), 51).unwrap_err();
        assert!(e.starts_with("Bucket quota exceeded"), "{e}");
        let mib = 1024 * 1024;
        assert_eq!(
            quota_decision(25 * mib, Some(24 * mib), 3 * mib).unwrap_err(),
            "Bucket quota exceeded: 24.0 MiB used + 3.0 MiB upload > 25.0 MiB limit"
        );
        assert!(
            quota_decision(100, Some(u64::MAX), 1).is_err(),
            "saturating, never wraps"
        );
    }

    #[test]
    fn quota_message_units_are_iec() {
        // The sizes are 1024-based, so the labels are KiB/MiB/GiB.
        let gib = 1024 * 1024 * 1024;
        assert_eq!(
            quota_decision(10 * gib, Some(10 * gib), 1000).unwrap_err(),
            "Bucket quota exceeded: 10.0 GiB used + 1 KiB upload > 10.0 GiB limit"
        );
    }
}

/// The quota gate and the outbox enqueue against a real engine, counter and
/// jobs DB.
#[cfg(test)]
mod write_path_tests {
    use super::*;
    use crate::bucket_usage::BucketUsage;
    use crate::deltaglider::savings::SavingsTotals;
    use crate::storage::{DynStorageBackend, FilesystemBackend, StorageBackend};
    use crate::types::FileMetadata;
    use std::time::Duration;

    struct Fixture {
        _tmp: tempfile::TempDir,
        state: Arc<AppState>,
        usage: Arc<BucketUsage>,
        /// The same storage as the engine's, for out-of-band writes.
        raw: FilesystemBackend,
    }

    async fn fixture(quota: u64) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.buckets.insert(
            "b".into(),
            crate::bucket_policy::BucketPolicyConfig {
                quota_bytes: Some(quota),
                ..Default::default()
            },
        );
        let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(
            FilesystemBackend::new(tmp.path().to_path_buf())
                .await
                .unwrap(),
        );
        let usage = Arc::new(BucketUsage::in_memory().unwrap());
        let engine = crate::deltaglider::DeltaGliderEngine::new_with_backend(
            Arc::new(backend),
            &config,
            None,
        )
        .with_bucket_usage(Some(usage.clone()));
        engine.create_bucket("b").await.unwrap();
        let mut state = Arc::try_unwrap(AppState::for_tests(engine)).ok().unwrap();
        state.bucket_usage = Some(usage.clone());
        state.jobs_db = Some(Arc::new(tokio::sync::Mutex::new(
            crate::config_db::JobsDb::in_memory("pw").unwrap(),
        )));
        let raw = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .unwrap();
        Fixture {
            _tmp: tmp,
            state: Arc::new(state),
            usage,
            raw,
        }
    }

    async fn put(state: &Arc<AppState>, key: &str, size: usize) -> Result<(), S3Error> {
        let data = vec![7u8; size];
        store_client_write(
            state,
            ClientWrite {
                bucket: "b",
                key,
                data: bytes::Bytes::from(data),
                content_type: None,
                user_metadata: Default::default(),
                precondition: &crate::deltaglider::Precondition::none(),
            },
        )
        .await
        .map(|_| ())
    }

    /// Data that the counter never saw (a bucket adopted in place, data from
    /// before the counter) is not in a never-scanned row. Trusting that row
    /// let the bucket grow past its quota by the whole adopted size.
    #[tokio::test]
    async fn a_never_scanned_counter_is_not_trusted_for_quota() {
        let f = fixture(100).await;
        let meta = FileMetadata::new_passthrough(
            "old.jpg".into(),
            "0".repeat(64),
            "0".repeat(32),
            90,
            None,
        );
        f.raw
            .put_passthrough(
                "b",
                "",
                "old.jpg",
                bytes::Bytes::copy_from_slice(&[0u8; 90]),
                &meta,
            )
            .await
            .unwrap();
        put(&f.state, "a.jpg", 20)
            .await
            .expect("no usage known yet");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while f.state.usage_scanner.get("b", "").is_none() {
            assert!(tokio::time::Instant::now() < deadline, "no usage scan");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            put(&f.state, "c.jpg", 20).await.is_err(),
            "the never-scanned counter (20 B) hid the 90 B the counter never saw"
        );
    }

    /// An overwrite nets the size of the object it replaces: at the limit,
    /// replacing 10 B with 10 B changes nothing and is allowed.
    #[tokio::test]
    async fn an_overwrite_at_the_quota_nets_the_prior_size() {
        let f = fixture(100).await;
        put(&f.state, "k.jpg", 10).await.unwrap();
        put(&f.state, "big.jpg", 90).await.unwrap();
        // A Refresh: the counter row is ground truth now.
        let engine = f.state.engine.load();
        let mut totals = SavingsTotals::default();
        for k in ["k.jpg", "big.jpg"] {
            totals.accumulate(&engine.head("b", k).await.unwrap());
        }
        f.usage
            .overwrite_from_scan(f.usage.begin_scan("b"), &totals, 1)
            .unwrap();
        assert!(
            put(&f.state, "k.jpg", 10).await.is_ok(),
            "an overwrite that adds nothing was refused"
        );
        assert!(put(&f.state, "k.jpg", 11).await.is_err(), "net +1 B");
        assert!(put(&f.state, "new.jpg", 1).await.is_err(), "a new object");
    }

    fn event(key: &str) -> NewEvent {
        NewEvent::new(
            crate::event_outbox::EventKind::ObjectCreated,
            "b",
            key,
            crate::event_outbox::EventSource::S3Api,
            1,
            serde_json::json!({}),
        )
    }

    async fn outbox_keys(state: &Arc<AppState>) -> Vec<String> {
        let db = state.jobs_db.as_ref().unwrap().lock().await;
        db.event_outbox_since(0, 100)
            .unwrap()
            .into_iter()
            .map(|r| r.key)
            .collect()
    }

    /// A client PUT/DELETE never waits for the jobs DB mutex: a long holder
    /// (a parity verify, the sync snapshot) used to stall every write
    /// request on its event append. The events still land, in order.
    #[tokio::test]
    async fn an_object_event_never_waits_for_the_jobs_db() {
        let f = fixture(u64::MAX).await;
        let db = f.state.jobs_db.clone().unwrap();
        let held = db.lock().await;
        // The mutex stays held: an append that waits for it never ends, so
        // the bound only tells "never" from "eventually".
        for key in ["a.zip", "b.zip"] {
            tokio::time::timeout(
                Duration::from_secs(10),
                enqueue_object_events(&f.state, &[event(key)]),
            )
            .await
            .expect("the event append waited for the jobs DB mutex");
        }
        drop(held);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while outbox_keys(&f.state).await.len() < 2 {
            assert!(tokio::time::Instant::now() < deadline, "events lost");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // A later event lands after the queued ones.
        enqueue_object_events(&f.state, &[event("c.zip")]).await;
        crate::event_outbox::flush_appends(&db).await;
        assert_eq!(outbox_keys(&f.state).await, ["a.zip", "b.zip", "c.zip"]);
    }
}
