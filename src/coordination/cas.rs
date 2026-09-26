// SPDX-License-Identifier: BUSL-1.1

//! THE classifier for "a conditional write lost the race".
//!
//! A conditional S3 write (`If-Match` / `If-None-Match: *`) that another
//! writer beat answers `412 PreconditionFailed`. AWS answers two such writes
//! that race each other with `409 ConditionalRequestConflict` ("retry").
//! Both mean the same thing: the condition cannot hold, a peer won. A caller
//! that checked only 412 read the 409 as an unrelated error (a failed lease
//! or lock step, a non-retryable PUT). Every conditional-write call site uses
//! [`conditional_write_lost`]; a source test refuses a bare 412 check.

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
/// `signal` is `config_db_sync::sdk_error_signal` of the SDK error.
pub fn conditional_write_lost(signal: &str) -> bool {
    is_precondition_failed(signal) || is_conditional_conflict(signal)
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

    /// A bare 412 check misses the 409: every site uses the classifier.
    #[test]
    fn no_bare_precondition_checks() {
        // (file, reason). Keep this short.
        const ALLOWED: &[(&str, &str)] = &[("src/coordination/cas.rs", "the classifier itself")];
        let needle = ["is_precondition", "_failed("].concat();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut stack = vec![root.join("src")];
        let mut offenders = Vec::new();
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if ALLOWED.iter().any(|(f, _)| *f == rel) {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                for (n, line) in text.lines().enumerate() {
                    let code = line.split("//").next().unwrap_or("");
                    if code.contains(needle.as_str()) {
                        offenders.push(format!("{rel}:{}: {}", n + 1, line.trim()));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "use coordination::cas::conditional_write_lost (412 or 409 \
             ConditionalRequestConflict):\n{}",
            offenders.join("\n")
        );
    }
}
