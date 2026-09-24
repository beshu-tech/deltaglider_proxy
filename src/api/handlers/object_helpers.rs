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
//! * [`check_quota`] — pre-write quota gate.
//! * [`enqueue_object_event`] / [`enqueue_object_events`] — best-
//!   effort event-outbox append for notification dispatch.
//!
//! Everything else moved into the s3s adapter or was already
//! axum-handler-specific and went away with `object.rs` /
//! `bucket.rs` / `multipart.rs`.

use super::AppState;
use crate::api::errors::S3Error;
use crate::event_outbox::NewEvent;
use std::sync::Arc;
use tracing::warn;

/// Append a single object event to the outbox. Silently noops when
/// no config DB is attached (open-mode dev runs). Errors are
/// warn-logged and dropped — notifications are best-effort by design.
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
    if filtered.is_empty() {
        return;
    }
    let events = filtered.as_slice();
    let Some(config_db) = state.config_db.as_ref() else {
        return;
    };
    let db = config_db.lock().await;
    if let Err(err) = db.event_outbox_insert_many(events) {
        warn!(
            "failed to append {} object event(s), first kind={} bucket={} key={:?}: {}",
            events.len(),
            events[0].kind.as_str(),
            events[0].bucket,
            events[0].key,
            err
        );
    }
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
/// (the "freeze the bucket" override).
///
/// "Used" is the bucket's STORED footprint from the O(1) running counter
/// (`bucket_usage`, updated inline on every write/delete and including the
/// delta baselines). The usage scanner is only a fallback when no counter row
/// exists yet: it sums per-object stored sizes WITHOUT the `reference.bin`
/// baselines, so a bucket of one-build-per-folder uploads (each a 3 MB
/// baseline + a 46-byte delta) looked almost empty and the quota never bit.
/// With neither source warm the write is let through optimistically.
pub(crate) fn check_quota(
    state: &Arc<AppState>,
    bucket: &str,
    incoming_bytes: u64,
) -> Result<(), S3Error> {
    let engine = state.engine.load();
    let Some(quota) = engine.bucket_policy_registry().quota_bytes(bucket) else {
        return Ok(());
    };
    let used = state
        .bucket_usage
        .as_ref()
        .and_then(|u| u.read(bucket).ok().flatten())
        .map(|row| row.stored_bytes)
        .or_else(|| {
            state
                .usage_scanner
                .get_or_scan(state, bucket, "")
                .map(|u| u.total_size)
        });
    quota_decision(quota, used, incoming_bytes).map_err(S3Error::AccessDeniedReason)
}

/// Pure quota verdict. `Err` carries the operator-facing reason; the caller
/// maps it to 403 AccessDenied (the documented status — it used to be a 500
/// InternalError, which S3 SDKs retry as a server fault).
pub(crate) fn quota_decision(quota: u64, used: Option<u64>, incoming: u64) -> Result<(), String> {
    if quota == 0 {
        return Err("Bucket is frozen (quota = 0)".into());
    }
    match used {
        Some(used) if used.saturating_add(incoming) > quota => Err(format!(
            "Bucket quota exceeded: {} MB used of {} MB limit",
            used / (1024 * 1024),
            quota / (1024 * 1024),
        )),
        _ => Ok(()),
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
        assert!(
            quota_decision(100, Some(u64::MAX), 1).is_err(),
            "saturating, never wraps"
        );
    }
}
