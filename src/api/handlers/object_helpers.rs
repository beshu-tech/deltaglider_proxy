// SPDX-License-Identifier: GPL-3.0-only

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
    if events.is_empty() {
        return;
    }
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

/// Pre-write quota gate. Returns `Err` when the write would push the
/// bucket past its `quota_bytes` policy, or when quota is set to 0
/// (the "freeze the bucket" override).
///
/// Uses cached usage data; if the cache is cold we trigger a
/// background scan and allow this single write through optimistically.
/// Operators who want strict enforcement scan the bucket first via
/// `POST /_/api/admin/usage/scan`.
pub(crate) fn check_quota(
    state: &Arc<AppState>,
    bucket: &str,
    incoming_bytes: u64,
) -> Result<(), S3Error> {
    let engine = state.engine.load();
    if let Some(quota) = engine.bucket_policy_registry().quota_bytes(bucket) {
        // quota=0 means freeze — always reject, even without usage data.
        if quota == 0 {
            return Err(S3Error::InternalError(
                "Bucket is frozen (quota = 0)".into(),
            ));
        }
        // get_or_scan: returns cached usage if available, otherwise triggers a
        // background scan and returns None (first PUT is optimistic).
        if let Some(usage) = state.usage_scanner.get_or_scan(state, bucket, "") {
            if usage.total_size.saturating_add(incoming_bytes) > quota {
                let used_mb = usage.total_size / (1024 * 1024);
                let quota_mb = quota / (1024 * 1024);
                return Err(S3Error::InternalError(format!(
                    "Bucket quota exceeded: {} MB used of {} MB limit",
                    used_mb, quota_mb,
                )));
            }
        }
    }
    Ok(())
}
