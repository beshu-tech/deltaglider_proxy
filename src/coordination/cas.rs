// SPDX-License-Identifier: BUSL-1.1

//! THE classifier for "a conditional write lost the race".
//!
//! A conditional S3 write (`If-Match` / `If-None-Match: *`) that another
//! writer beat answers `412 PreconditionFailed`. AWS answers two such writes
//! that race each other with `409 ConditionalRequestConflict` ("retry").
//! Both mean the same thing: the condition cannot hold, a peer won. A caller
//! that checked only 412 read the 409 as an unrelated error (a failed lease
//! or lock step, a non-retryable PUT). Every conditional-write call site uses
//! [`conditional_write_lost`]; the bare 412 check is private to this file.

/// `412 PreconditionFailed`: the `If-Match` / `If-None-Match` guard did not
/// hold. AWS and MinIO send `PreconditionFailed` and/or status 412. Private:
/// callers use [`conditional_write_lost`], which also covers the 409.
fn is_precondition_failed(signal: &str) -> bool {
    signal.contains("PreconditionFailed")
        || signal.contains("Precondition Failed")
        || signal.contains("412")
}

/// `409 ConditionalRequestConflict`. A plain 409 (BucketNotEmpty,
/// OperationAborted, …) is not about the condition.
pub fn is_conditional_conflict(signal: &str) -> bool {
    signal.contains("status=409") && signal.contains("code=ConditionalRequestConflict")
}

/// Pure: did a conditional write lose (412, or 409 ConditionalRequestConflict)?
/// `signal` is `sdk_error_signal` of the SDK error.
pub fn conditional_write_lost(signal: &str) -> bool {
    is_precondition_failed(signal) || is_conditional_conflict(signal)
}

/// Pure classifier: did the backend LOUDLY reject the conditional request as
/// unimplemented (HTTP 501 / NotImplemented)? Backblaze B2 answers conditional
/// writes this way — a DEFINITIVE "no CAS", unlike a transport error.
pub(crate) fn is_not_implemented(err_str: &str) -> bool {
    err_str.contains("NotImplemented") || err_str.contains("501")
}

/// Pure: does a stringified GET error signal the object is ABSENT (a 404-class
/// response) rather than a real failure? Used by the sync witness read and
/// the lease and lock reads.
pub(crate) fn is_object_absent(err_str: &str) -> bool {
    err_str.contains("NoSuchKey") || err_str.contains("NotFound") || err_str.contains("404")
}

/// Compact classification signal from a typed SDK error: HTTP status + error
/// code ONLY. Never feed the substring classifiers `format!("{e:?}")` — the
/// debug string embeds endpoint/bucket/request-ids that can contain "412"/
/// "501"/"404" and poison the match (e.g. a backend on port 9501).
pub(crate) fn sdk_error_signal<E>(e: &aws_sdk_s3::error::SdkError<E>) -> String
where
    E: aws_sdk_s3::error::ProvideErrorMetadata,
{
    use aws_sdk_s3::error::ProvideErrorMetadata;
    let code = e.code().unwrap_or("");
    match e {
        aws_sdk_s3::error::SdkError::ServiceError(svc) => {
            format!("status={} code={code}", svc.raw().status().as_u16())
        }
        _ => format!("transport code={code}"),
    }
}

/// THE conditional PUT of the lease and the lock: PUT a JSON coordination
/// object with a precondition: `None` → `If-None-Match: *` (create only),
/// `Some(etag)` → `If-Match: etag`. Returns the ETag of the written object. `Ok(Some(etag))` = written
/// (`etag` is empty when the response carries none), `Ok(None)` = a peer
/// won the race. The writer keeps the ETag, so its next step (release,
/// renew) can be conditional on it without a read first.
pub(crate) async fn conditional_put_etag(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
    body: aws_sdk_s3::primitives::ByteStream,
    precondition: Option<&str>,
) -> Result<Option<String>, String> {
    let put = client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(body)
        .content_type("application/json");
    let put = match precondition {
        Some(etag) => put.if_match(etag),
        None => put.if_none_match("*"),
    };
    match put.send().await {
        Ok(out) => Ok(Some(out.e_tag().unwrap_or_default().to_string())),
        Err(e) if conditional_write_lost(&sdk_error_signal(&e)) => Ok(None),
        Err(e) => Err(format!("{e:?}")),
    }
}

/// The ETag of each coordination object that this process wrote last, by
/// (key, owner): the lease and the lock keep it so that a release is one
/// `DELETE If-Match` with no read first (when the bucket enforces the
/// condition on DELETE). An entry is dropped when a write
/// of it lost or failed (the object may have moved): the caller then
/// reads before it deletes, as before.
#[derive(Debug, Default)]
pub(crate) struct KeptEtags(
    parking_lot::Mutex<std::collections::HashMap<(String, String), String>>,
);

impl KeptEtags {
    /// Record the outcome of a conditional PUT of `key` by `owner`.
    pub(crate) fn note<E>(&self, key: &str, owner: &str, outcome: &Result<Option<String>, E>) {
        let id = (key.to_string(), owner.to_string());
        let mut kept = self.0.lock();
        match outcome {
            Ok(Some(etag)) if !etag.is_empty() => {
                kept.insert(id, etag.clone());
            }
            _ => {
                kept.remove(&id);
            }
        }
    }

    /// Take the kept ETag of `key` by `owner` (the release forgets it).
    pub(crate) fn take(&self, key: &str, owner: &str) -> Option<String> {
        self.0.lock().remove(&(key.to_string(), owner.to_string()))
    }
}

/// Pure: may a release that READ the lock or lease first delete it? Only
/// when it is ours (`lock_owner == owner`) and still LIVE on the server
/// clock (`expires_at >= now`, the renew predicate). The read-first path
/// serves a bucket that may ignore `If-Match` on DELETE, where the delete
/// removes whatever is there. An expired object is free to every peer,
/// which may steal it between the read and the delete; skipping the delete
/// costs nothing, because the object is already free.
pub(crate) fn read_first_release_may_delete(
    lock_owner: &str,
    expires_at: i64,
    owner: &str,
    now: i64,
) -> bool {
    lock_owner == owner && expires_at >= now
}

/// `DELETE If-Match: etag`. A 412 means that the object is no longer the
/// one we wrote (a peer stole it after our hold lapsed): nothing of ours
/// is left to delete. Best-effort; errors are logged at debug.
///
/// Safe with no read first ONLY on a bucket whose CAS probe proved that
/// it enforces `If-Match` on DELETE (`ProbeResult::conditional_delete`):
/// a backend that ignores the condition deletes whatever is there.
pub(crate) async fn delete_if_match(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
    etag: &str,
) {
    if let Err(e) = client
        .delete_object()
        .bucket(bucket)
        .key(key)
        .if_match(etag)
        .send()
        .await
    {
        tracing::debug!(
            "coordination release of {key}: {} (the object is not ours any more, or the TTL frees it)",
            sdk_error_signal(&e)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precondition_failed_detected_from_common_shapes() {
        // S3-style service error display.
        assert!(is_precondition_failed(
            "service error: PreconditionFailed: At least one of the pre-conditions you specified did not hold"
        ));
        // MinIO / human-readable status text.
        assert!(is_precondition_failed(
            "unhandled error (Precondition Failed)"
        ));
        // Raw HTTP status code.
        assert!(is_precondition_failed(
            "dispatch failure: response status: 412"
        ));
    }

    #[test]
    fn non_precondition_errors_are_not_misclassified() {
        assert!(!is_precondition_failed(
            "dispatch failure: connection refused"
        ));
        assert!(!is_precondition_failed(
            "NoSuchBucket: bucket does not exist"
        ));
        assert!(!is_precondition_failed(
            "service error: AccessDenied (status 403)"
        ));
        assert!(!is_precondition_failed(""));
    }

    #[test]
    fn read_first_release_deletes_only_a_live_own_object() {
        assert!(read_first_release_may_delete("me", 100, "me", 100));
        assert!(read_first_release_may_delete("me", 160, "me", 100));
        // Expired: a peer may be stealing it right now.
        assert!(!read_first_release_may_delete("me", 99, "me", 100));
        // Another owner's: never.
        assert!(!read_first_release_may_delete("peer", 160, "me", 100));
    }

    #[test]
    fn conditional_write_lost_truth_table() {
        assert!(conditional_write_lost("status=412 code=PreconditionFailed"));
        assert!(conditional_write_lost("status=412 code="));
        assert!(conditional_write_lost(
            "status=409 code=ConditionalRequestConflict"
        ));
        assert!(!conditional_write_lost("status=409 code=BucketNotEmpty"));
        assert!(!conditional_write_lost("status=409 code=OperationAborted"));
        assert!(!conditional_write_lost("status=503 code=SlowDown"));
        assert!(!conditional_write_lost("transport code="));
    }

    #[test]
    fn object_absent_detected_from_common_shapes() {
        // The sdk_error_signal contract shape:
        assert!(is_object_absent("status=404 code=NoSuchKey"));
        assert!(!is_object_absent("status=403 code=AccessDenied"));
        assert!(is_object_absent("service error: NoSuchKey"));
        assert!(is_object_absent("dispatch failure: NotFound"));
        assert!(is_object_absent("HTTP 404"));
        assert!(!is_object_absent("AccessDenied"));
        assert!(!is_object_absent("PreconditionFailed"));
    }

    #[test]
    fn not_implemented_classification() {
        // B2-style loud rejections → DEFINITIVE non-CAS.
        assert!(is_not_implemented(
            "service error: NotImplemented: conditional writes not supported"
        ));
        assert!(is_not_implemented("http status: 501"));
        // The sdk_error_signal contract shape — and the poisoning class it
        // exists to prevent: "501" in an endpoint/bucket must never classify.
        assert!(is_not_implemented("status=501 code=NotImplemented"));
        assert!(!is_not_implemented("status=403 code=AccessDenied"));
        // Transport-class errors are NOT "not implemented" — they must map to
        // Indeterminate, never to a fatal non-CAS verdict.
        assert!(!is_not_implemented("dispatch failure: connection refused"));
        assert!(!is_not_implemented("timeout waiting for response"));
        assert!(!is_not_implemented(
            "service error: AccessDenied (status 403)"
        ));
        assert!(!is_not_implemented(""));
    }
}
