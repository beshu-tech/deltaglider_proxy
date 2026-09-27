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
/// `signal` is [`sdk_error_signal`] of the SDK error.
pub fn conditional_write_lost(signal: &str) -> bool {
    is_precondition_failed(signal) || is_conditional_conflict(signal)
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

/// PUT a JSON coordination object with a precondition: `None` →
/// `If-None-Match: *` (create only), `Some(etag)` → `If-Match: etag`.
/// `Ok(true)` = written, `Ok(false)` = a peer won the race, `Err` = any
/// other failure. The one conditional PUT of the lease and the lock.
pub(crate) async fn conditional_put(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
    body: aws_sdk_s3::primitives::ByteStream,
    precondition: Option<&str>,
) -> Result<bool, String> {
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
        Ok(_) => Ok(true),
        Err(e) if conditional_write_lost(&sdk_error_signal(&e)) => Ok(false),
        Err(e) => Err(format!("{e:?}")),
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
}
