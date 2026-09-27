// SPDX-License-Identifier: BUSL-1.1

//! Classification of S3 SDK errors into `StorageError`, and the pure
//! verdicts of conditional writes and deletes.

use super::*;

/// Operation context for S3 error classification.
#[derive(Debug, Clone, Copy)]
pub(crate) enum S3Op {
    ListObjects,
    CreateBucket,
    HeadBucket,
    PutObject,
    GetObject,
    DeleteObject,
    HeadObject,
    CreateMpu,
    UploadPart,
    CompleteMpu,
    AbortMpu,
    Other(&'static str),
}

/// Pure: is this S3 error the ordinary "not there" answer of a HEAD probe?
/// HeadBucket 404 (403 on providers that hide bucket existence) and
/// HeadObject 404. The caller logs those at debug, not warn.
pub(super) fn s3_error_is_expected_absence(op: &S3Op, status: Option<u16>) -> bool {
    matches!(
        (op, status),
        (S3Op::HeadBucket, Some(404 | 403)) | (S3Op::HeadObject, Some(404))
    )
}

impl std::fmt::Display for S3Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            S3Op::ListObjects => write!(f, "list_objects"),
            S3Op::CreateBucket => write!(f, "create_bucket"),
            S3Op::HeadBucket => write!(f, "head_bucket"),
            S3Op::PutObject => write!(f, "put_object"),
            S3Op::GetObject => write!(f, "get_object"),
            S3Op::DeleteObject => write!(f, "delete_object"),
            S3Op::HeadObject => write!(f, "head_object"),
            S3Op::CreateMpu => write!(f, "create_multipart_upload"),
            S3Op::UploadPart => write!(f, "upload_part"),
            S3Op::CompleteMpu => write!(f, "complete_multipart_upload"),
            S3Op::AbortMpu => write!(f, "abort_multipart_upload"),
            S3Op::Other(s) => write!(f, "{}", s),
        }
    }
}

impl S3Op {
    /// Returns true if this operation is a bucket-level operation where a 403
    /// should be treated as BucketNotFound (S3-compatible providers like MinIO
    /// and Ceph return 403 for non-existent buckets).
    pub(super) fn is_bucket_level(&self) -> bool {
        matches!(
            self,
            S3Op::ListObjects | S3Op::CreateBucket | S3Op::HeadBucket
        )
    }
}

impl S3Backend {
    /// [`Self::classify_s3_error`], plus the passive health signal: an
    /// `Unavailable` result names the backend and marks it unhealthy.
    pub(super) fn classify(
        &self,
        bucket: &str,
        e: &SdkError<impl std::fmt::Debug + ProvideErrorMetadata>,
        op: S3Op,
    ) -> StorageError {
        self.observe(Self::classify_s3_error(bucket, e, op))
    }

    /// [`Self::classify_get_error`] plus the passive health signal.
    pub(super) fn classify_get(
        &self,
        bucket: &str,
        key: &str,
        e: &SdkError<aws_sdk_s3::operation::get_object::GetObjectError>,
    ) -> StorageError {
        self.observe(Self::classify_get_error(bucket, key, e))
    }

    pub(super) fn observe(&self, err: StorageError) -> StorageError {
        match (err, &self.health_key) {
            (StorageError::Unavailable(msg), Some((name, fp))) => {
                crate::coordination::health::note_unavailable(name, fp, &msg);
                StorageError::Unavailable(format!("backend '{name}': {msg}"))
            }
            (err, _) => err,
        }
    }

    /// Classify an S3 SDK error with full diagnostic context.
    ///
    /// Logs bucket, key, body size, HTTP status, error code, and request-id
    /// for production debugging (per Python DeltaGlider team recommendations).
    /// Maps bucket-level 403 to BucketNotFound (Hetzner, Ceph return 403 for
    /// non-existent buckets to prevent enumeration).
    pub(super) fn classify_s3_error(
        bucket: &str,
        e: &SdkError<impl std::fmt::Debug + ProvideErrorMetadata>,
        op: S3Op,
    ) -> StorageError {
        // Extract diagnostic details from the SDK error
        let (status, request_id) = if let SdkError::ServiceError(ref svc) = e {
            let raw = svc.raw();
            let status = raw.status().as_u16();
            let rid = raw.headers().get("x-amz-request-id").unwrap_or("-");
            (Some(status), rid.to_string())
        } else {
            (None, "-".to_string())
        };

        // Log full context for production debugging. A HEAD that answers
        // "absent" is a normal probe result (routing, existence checks), so
        // it logs at debug; everything else is a warning.
        let status_text = status
            .map(|s| s.to_string())
            .unwrap_or_else(|| "-".to_string());
        if s3_error_is_expected_absence(&op, status) {
            debug!(
                "S3 absent: op={} bucket={} status={} request_id={}",
                op, bucket, status_text, request_id,
            );
        } else {
            warn!(
                "S3 error: op={} bucket={} status={} request_id={} error={:?}",
                op, bucket, status_text, request_id, e,
            );
        }

        // No answer at all (timeout, refused/reset connection): the backend
        // is unavailable, which is a 503 and a passive health signal.
        if matches!(e, SdkError::TimeoutError(_) | SdkError::DispatchFailure(_)) {
            return StorageError::Unavailable(format!("{op} on bucket '{bucket}': {e}"));
        }
        // Classify by the structured error CODE only. The Debug text embeds
        // the error message, which names the key: a key `SlowDown-q3.pdf`
        // turned every error on it into a 503 forever.
        let code = e.code().unwrap_or("");
        // Explicit NoSuchBucket → bucket doesn't exist.
        if code == "NoSuchBucket" {
            return StorageError::BucketNotFound(bucket.to_string());
        }
        // The backend refuses the key's length: a client error, not a 500.
        if code == "KeyTooLongError" {
            return StorageError::KeyTooLong(format!(
                "{op}: the storage backend refused the key as too long"
            ));
        }
        // NoSuchKey (object-level 404) → NotFound, not a 500. This generic
        // classifier runs for ops without a typed error variant (e.g.
        // CopyObject, where the *source* key may have been deleted by a
        // concurrent request). Without this, such a benign race surfaced as
        // `S3(...)` → HTTP 500 instead of 404. The typed GET path already
        // does this via `classify_get_error`; this covers the rest. Guard on
        // the op NOT being bucket-level so a 404 on a bucket op stays a
        // BucketNotFound concern, not a key NotFound.
        if !op.is_bucket_level() && (code == "NoSuchKey" || matches!(status, Some(404))) {
            return StorageError::NotFound(format!("{} key not found", op));
        }
        // Bucket-level 404 → the bucket is absent. HEAD responses carry NO
        // body, so the NoSuchBucket marker check above can never match for
        // HeadBucket — and S3 semantics define a HeadBucket 404 as "bucket
        // does not exist". Without this, a bare 404 fell into `S3(...)`,
        // which routing treats as TRANSIENT: an unrouted bucket became
        // "transiently" unroutable forever and backend discovery never ran
        // (the beshu-b2 incident).
        if op.is_bucket_level() && matches!(status, Some(404)) {
            return StorageError::BucketNotFound(bucket.to_string());
        }
        // Some S3-compatible providers (MinIO, Ceph) return 403 for non-existent
        // buckets to prevent bucket enumeration. Only treat 403 as BucketNotFound
        // if the operation is bucket-level. Object-level 403 errors are genuine
        // AccessDenied and should not be misclassified.
        if let Some(s) = status {
            if s == 403 && op.is_bucket_level() {
                return StorageError::BucketNotFound(bucket.to_string());
            }
            // E-P1-1: 503 SlowDown is the AWS-spec transient throttle
            // signal. Map to a dedicated `Throttled` variant so the
            // API layer can surface 503 SlowDown to the caller —
            // pre-fix this fell into `S3(...)` → catch-all in
            // `api/errors.rs` → 500 InternalError, which AWS SDKs
            // treat as permanent and DON'T back off on. Real
            // production load against a back-pressuring backend
            // would cascade into client retry storms with no
            // throttle propagation. Also catches `SlowDown` literal
            // in the SDK error body when the upstream returns it
            // without a 503 status (some implementations).
            // 429 Too Many Requests is the throttle signal from Backblaze B2 and
            // Cloudflare R2 (and some S3-compatibles) where AWS uses 503 SlowDown.
            // Classify it the same, or it falls into the S3(...) catch-all → 500,
            // which SDKs treat as permanent and don't back off on.
            if s == 503 || s == 429 || code == "SlowDown" {
                return StorageError::Throttled(format!("{} throttled (status={}): {}", op, s, e));
            }
            if s == 403 {
                return StorageError::AccessDenied(format!("{op} failed (status=403): {e}"));
            }
        } else if code == "SlowDown" {
            return StorageError::Throttled(format!("{} throttled: {}", op, e));
        }
        StorageError::S3(format!(
            "{} failed (status={}): {}",
            op,
            status.unwrap_or(0),
            e
        ))
    }

    /// Classify a GetObject SDK error, mapping NoSuchKey to NotFound.
    pub(super) fn classify_get_error(
        bucket: &str,
        key: &str,
        e: &SdkError<aws_sdk_s3::operation::get_object::GetObjectError>,
    ) -> StorageError {
        if let SdkError::ServiceError(service_error) = e {
            if matches!(
                service_error.err(),
                aws_sdk_s3::operation::get_object::GetObjectError::NoSuchKey(_)
            ) {
                return StorageError::NotFound(key.to_string());
            }
        }
        Self::classify_s3_error(bucket, e, S3Op::GetObject)
    }
}

/// How a failed fenced write reads.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum FencedWriteVerdict {
    /// 412: another writer changed reference.bin — never overwrite it.
    Lost,
    /// 501: the backend has no conditional writes (B2). Non-CAS backends are
    /// refused for multi-instance delta storage, so today's unconditional
    /// write is the right fallback.
    Unsupported,
    /// Not about the condition (or no condition was sent).
    Other,
}

/// Pure: classify a failed write by its SDK signal (status + code).
pub(super) fn fenced_write_verdict(fence: &RefFence, signal: &str) -> FencedWriteVerdict {
    let conditional = match fence {
        RefFence::Unfenced => false,
        RefFence::Absent => true,
        RefFence::ETag(e) => !e.is_empty(),
    };
    if !conditional {
        FencedWriteVerdict::Other
    } else if crate::coordination::cas::conditional_write_lost(signal) {
        FencedWriteVerdict::Lost
    } else if crate::config_db_sync::is_not_implemented(signal) {
        FencedWriteVerdict::Unsupported
    } else {
        FencedWriteVerdict::Other
    }
}

/// What a failed conditional `DeleteObject` means.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ConditionalDeleteVerdict {
    /// 412/409: the object is not the version we checked.
    Changed,
    /// 501: the backend has no conditional delete.
    Unsupported,
    Other,
}

/// Pure: classify a failed conditional delete from its error signal.
pub(super) fn conditional_delete_verdict(signal: &str) -> ConditionalDeleteVerdict {
    if crate::coordination::cas::conditional_write_lost(signal) {
        ConditionalDeleteVerdict::Changed
    } else if crate::config_db_sync::is_not_implemented(signal) {
        ConditionalDeleteVerdict::Unsupported
    } else {
        ConditionalDeleteVerdict::Other
    }
}

/// Pure: a CreateBucket refused because the bucket exists. The caller's own
/// bucket is a success (idempotent: the us-east-1 answer and what the
/// filesystem backend gives), someone else's is `AlreadyExists` (409
/// BucketAlreadyExists). `None`: not a conflict, classify as usual.
pub(super) fn classify_create_bucket_conflict(
    bucket: &str,
    code: Option<&str>,
) -> Option<Result<(), StorageError>> {
    match code {
        Some("BucketAlreadyOwnedByYou") => Some(Ok(())),
        Some("BucketAlreadyExists") => Some(Err(StorageError::AlreadyExists(bucket.to_string()))),
        _ => None,
    }
}
