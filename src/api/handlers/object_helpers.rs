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
//! * [`store_client_multipart_admit`] / [`store_client_multipart_commit`]
//!   — the same quota gate, write lock and event for a multipart completion.
//! * [`check_quota_for_write`] / [`check_quota`] — pre-write quota gate.
//! * [`enqueue_object_event`] / [`enqueue_object_events`] — best-
//!   effort event-outbox append for notification dispatch; never waits
//!   for the config DB mutex.
//!
//! Everything else moved into the s3s adapter or was already
//! axum-handler-specific and went away with `object.rs` /
//! `bucket.rs` / `multipart.rs`.

use super::AppState;
use crate::api::errors::S3Error;
use crate::event_outbox::NewEvent;
use std::sync::Arc;

/// Append a single object event to the outbox. Silently noops when
/// no config DB is attached (open-mode dev runs). Errors are
/// warn-logged and dropped — notifications are best-effort by design.
/// Never waits for the config DB mutex (see
/// [`crate::event_outbox::append_events`]).
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
    let Some(config_db) = state.config_db.as_ref() else {
        return;
    };
    crate::event_outbox::append_events(config_db, filtered);
}

/// One client write of a whole body (PutObject, CopyObject, form POST).
pub(crate) struct ClientWrite<'a> {
    pub bucket: &'a str,
    pub key: &'a str,
    pub data: &'a [u8],
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
    check_quota_for_write(state, write.bucket, write.key, write.data.len() as u64).await?;
    let result = state
        .engine
        .load()
        .store_conditional(
            write.bucket,
            write.key,
            write.data,
            write.content_type,
            write.user_metadata,
            write.precondition,
        )
        .await?;
    object_created(
        state,
        write.bucket,
        write.key,
        write.data.len() as u64,
        &result,
    )
    .await;
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
        let _guard = engine
            .lock_and_check(write.bucket, write.key, write.precondition)
            .await?;
        engine
            .store_spooled_delta(
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

/// Pre-write quota gate. Returns `Err` when the write would push the
/// bucket past its `quota_bytes` policy, or when quota is set to 0
/// (the "freeze the bucket" override). "Used" is [`quota_used`]. This
/// variant does not know the key, so an overwrite is judged as a new
/// object; a client write uses [`check_quota_for_write`].
pub(crate) fn check_quota(
    state: &Arc<AppState>,
    bucket: &str,
    incoming_bytes: u64,
) -> Result<(), S3Error> {
    let engine = state.engine.load();
    let Some(quota) = engine.bucket_policy_registry().quota_bytes(bucket) else {
        return Ok(());
    };
    quota_decision(quota, quota_used(state, bucket), incoming_bytes)
        .map_err(S3Error::AccessDeniedReason)
}

/// [`check_quota`] for a write of `key`: an overwrite replaces the stored
/// bytes of the object it replaces, so a write that the gate refuses as a
/// new object is judged again with the prior object's stored size netted
/// out. Only that refusal pays the HEAD of the prior object. Before, an
/// overwrite at the limit was refused even when it added nothing.
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
/// config DB.
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
        state.config_db = Some(Arc::new(tokio::sync::Mutex::new(
            crate::config_db::ConfigDb::in_memory("pw").unwrap(),
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
                data: &data,
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
            .put_passthrough("b", "", "old.jpg", &[0u8; 90], &meta)
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
        let db = state.config_db.as_ref().unwrap().lock().await;
        db.event_outbox_since(0, 100)
            .unwrap()
            .into_iter()
            .map(|r| r.key)
            .collect()
    }

    /// A client PUT/DELETE never waits for the config DB mutex: a long holder
    /// (a parity verify, the sync snapshot) used to stall every write
    /// request on its event append. The events still land, in order.
    #[tokio::test]
    async fn an_object_event_never_waits_for_the_config_db() {
        let f = fixture(u64::MAX).await;
        let db = f.state.config_db.clone().unwrap();
        let held = db.lock().await;
        for key in ["a.zip", "b.zip"] {
            tokio::time::timeout(
                Duration::from_millis(500),
                enqueue_object_events(&f.state, &[event(key)]),
            )
            .await
            .expect("the event append waited for the config DB mutex");
        }
        drop(held);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while outbox_keys(&f.state).await.len() < 2 {
            assert!(tokio::time::Instant::now() < deadline, "events lost");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // An idle DB with nothing queued: the event is written before the
        // append returns, after the queued ones.
        enqueue_object_events(&f.state, &[event("c.zip")]).await;
        assert_eq!(outbox_keys(&f.state).await, ["a.zip", "b.zip", "c.zip"]);
    }
}
