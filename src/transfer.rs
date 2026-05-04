//! Shared engine-routed object transfer primitives.
//!
//! Replication and lifecycle transitions both need the same copy semantics:
//! retrieve through the DeltaGlider engine, store through the engine, preserve
//! multipart ETags, stamp provenance metadata, and retry narrow transient
//! transport failures.

use crate::deltaglider::DynEngine;
use std::sync::Arc;
use tracing::warn;

pub(crate) const DEFAULT_COPY_MAX_ATTEMPTS: u32 = 3;
pub(crate) const REPLICATION_RULE_METADATA_KEY: &str = "dg-replication-rule";
pub(crate) const LIFECYCLE_RULE_METADATA_KEY: &str = "dg-lifecycle-rule";

#[derive(Debug, Clone, Copy)]
pub(crate) struct TransferProvenance<'a> {
    pub metadata_key: &'a str,
    pub metadata_value: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ObjectTransferRequest<'a> {
    pub source_bucket: &'a str,
    pub source_key: &'a str,
    pub destination_bucket: &'a str,
    pub destination_key: &'a str,
    pub provenance: Option<TransferProvenance<'a>>,
    pub operation: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ObjectTransferOutcome {
    pub bytes_copied: usize,
}

pub(crate) async fn copy_object_with_retries(
    engine: &Arc<DynEngine>,
    request: ObjectTransferRequest<'_>,
) -> Result<ObjectTransferOutcome, Box<dyn std::error::Error + Send + Sync>> {
    let mut last_err: Option<String> = None;
    for attempt in 1..=DEFAULT_COPY_MAX_ATTEMPTS {
        match copy_object_once(engine, request).await {
            Ok(outcome) => return Ok(outcome),
            Err(err) => {
                let msg = err.to_string();
                if !is_transient_copy_error(&msg) || attempt == DEFAULT_COPY_MAX_ATTEMPTS {
                    return Err(if attempt > 1 {
                        format!("{} (after {} attempts)", msg, attempt).into()
                    } else {
                        msg.into()
                    });
                }
                warn!(
                    "{} transient copy failure attempt {}/{} src={}/{} dst={}/{}: {}",
                    request.operation,
                    attempt,
                    DEFAULT_COPY_MAX_ATTEMPTS,
                    request.source_bucket,
                    request.source_key,
                    request.destination_bucket,
                    request.destination_key,
                    msg
                );
                last_err = Some(msg);
                tokio::time::sleep(std::time::Duration::from_millis(250 * attempt as u64)).await;
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| "copy failed without error detail".to_string())
        .into())
}

async fn copy_object_once(
    engine: &Arc<DynEngine>,
    request: ObjectTransferRequest<'_>,
) -> Result<ObjectTransferOutcome, Box<dyn std::error::Error + Send + Sync>> {
    // HEAD first so callers get a crisp source-disappeared failure row.
    let source_head = engine
        .head(request.source_bucket, request.source_key)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("source head failed: {}", e).into()
        })?;

    let (data, meta) = engine
        .retrieve(request.source_bucket, request.source_key)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("source retrieve failed: {}", e).into()
        })?;

    let content_type = meta.content_type.clone();
    let mut user_metadata = meta.user_metadata.clone();
    let bytes = data.len();

    if let Some(provenance) = request.provenance {
        user_metadata.insert(
            provenance.metadata_key.to_string(),
            provenance.metadata_value.to_string(),
        );
    }

    if let Some(mp_etag) = meta.multipart_etag.clone() {
        engine
            .store_with_multipart_etag(
                request.destination_bucket,
                request.destination_key,
                &data,
                content_type,
                user_metadata,
                mp_etag,
            )
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("destination store failed: {}", e).into()
            })?;
    } else {
        engine
            .store(
                request.destination_bucket,
                request.destination_key,
                &data,
                content_type,
                user_metadata,
            )
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("destination store failed: {}", e).into()
            })?;
    }

    verify_destination(
        engine,
        request,
        bytes,
        source_head.multipart_etag.as_deref(),
    )
    .await?;
    Ok(ObjectTransferOutcome {
        bytes_copied: bytes,
    })
}

async fn verify_destination(
    engine: &Arc<DynEngine>,
    request: ObjectTransferRequest<'_>,
    expected_bytes: usize,
    expected_multipart_etag: Option<&str>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let dest = engine
        .head(request.destination_bucket, request.destination_key)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("destination verify head failed: {}", e).into()
        })?;
    if dest.file_size != expected_bytes as u64 {
        return Err(format!(
            "destination verify failed: expected {} bytes, found {}",
            expected_bytes, dest.file_size
        )
        .into());
    }
    if let Some(expected) = expected_multipart_etag {
        if dest.multipart_etag.as_deref() != Some(expected) {
            return Err(format!(
                "destination verify failed: expected multipart etag {:?}, found {:?}",
                expected, dest.multipart_etag
            )
            .into());
        }
    }
    Ok(())
}

pub(crate) fn is_transient_copy_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    [
        "failed to read response body",
        "streaming error",
        "connection reset",
        "connection closed",
        "connection aborted",
        "timed out",
        "timeout",
        "temporary failure",
        "service unavailable",
        "slowdown",
        "internalerror",
        "broken pipe",
    ]
    .iter()
    .any(|needle| m.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_copy_error_classification_is_narrow() {
        assert!(is_transient_copy_error(
            "source retrieve failed: Storage error: S3 error: Failed to read response body: streaming error"
        ));
        assert!(is_transient_copy_error("connection reset by peer"));
        assert!(is_transient_copy_error("503 SlowDown"));

        assert!(!is_transient_copy_error(
            "destination store failed: Storage error: Bucket not found: test-bucket"
        ));
        assert!(!is_transient_copy_error("AccessDenied"));
        assert!(!is_transient_copy_error("NoSuchKey"));
    }
}
