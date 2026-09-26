// SPDX-License-Identifier: BUSL-1.1

//! The conditional-write probe for a backend or bucket.
//!
//! Leases and reference locks use BOTH conditions: `If-None-Match:*` to
//! create, `If-Match:<etag>` to steal, renew and release. A backend that
//! honours only the first passes a create-only probe and then lets two nodes
//! renew or steal the same lock. So the probe tests both, on a caller-owned
//! key, and the verdict is a pure function of the four step outcomes.

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;

/// How one conditional step answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// The write went through.
    Written,
    /// 412: the condition was enforced.
    PreconditionFailed,
    /// 501 / NotImplemented: the condition was rejected (Backblaze B2).
    NotImplemented,
    /// Anything else: the probe cannot tell.
    Other(String),
}

/// Verdict from the three conditional steps (after an unconditional PUT
/// that returned the key's etag). `Ok(true)` = CAS enforced; `Ok(false)` =
/// DEFINITIVELY not enforced; `Err` = indeterminate (never treat as either).
pub fn classify_cas_probe(
    create_on_existing: &StepOutcome,
    if_match_wrong_etag: &StepOutcome,
    if_match_right_etag: &StepOutcome,
) -> Result<bool, String> {
    use StepOutcome::*;
    for (step, outcome) in [
        ("If-None-Match:* on an existing key", create_on_existing),
        ("If-Match with a wrong etag", if_match_wrong_etag),
    ] {
        match outcome {
            PreconditionFailed => {}
            Written | NotImplemented => return Ok(false),
            Other(e) => return Err(format!("{step}: {e}")),
        }
    }
    match if_match_right_etag {
        Written => Ok(true),
        // Refusing the CORRECT etag breaks renew and release just as surely.
        PreconditionFailed | NotImplemented => Ok(false),
        Other(e) => Err(format!("If-Match with the current etag: {e}")),
    }
}

fn outcome<T, E>(res: &Result<T, aws_sdk_s3::error::SdkError<E>>) -> StepOutcome
where
    E: std::error::Error + aws_sdk_s3::error::ProvideErrorMetadata + 'static,
{
    match res {
        Ok(_) => StepOutcome::Written,
        Err(e) => {
            let signal = crate::config_db_sync::sdk_error_signal(e);
            if crate::coordination::cas::conditional_write_lost(&signal) {
                StepOutcome::PreconditionFailed
            } else if crate::config_db_sync::is_not_implemented(&signal) {
                StepOutcome::NotImplemented
            } else {
                StepOutcome::Other(format!("{signal}: {e:?}"))
            }
        }
    }
}

/// Run the probe on `bucket/key` and delete the key afterwards (best
/// effort). Same three-way result as [`classify_cas_probe`].
pub async fn probe_cas(client: &Client, bucket: &str, key: &str) -> Result<bool, String> {
    let first = client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from_static(b"1"))
        .send()
        .await
        .map_err(|e| format!("probe could not write to '{bucket}': {e:?}"))?;
    let etag = first.e_tag().unwrap_or_default().to_string();
    let put = |body: &'static [u8]| {
        client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from_static(body))
    };
    let create = outcome(&put(b"2").if_none_match("*").send().await);
    let wrong = outcome(
        &put(b"3")
            .if_match("\"00000000000000000000000000000000\"")
            .send()
            .await,
    );
    let right = if etag.is_empty() {
        StepOutcome::Other("the first PUT returned no etag".into())
    } else {
        outcome(&put(b"4").if_match(&etag).send().await)
    };
    let _ = client.delete_object().bucket(bucket).key(key).send().await;
    classify_cas_probe(&create, &wrong, &right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use StepOutcome::*;

    #[test]
    fn full_cas_is_verified() {
        assert_eq!(
            classify_cas_probe(&PreconditionFailed, &PreconditionFailed, &Written),
            Ok(true)
        );
    }

    /// The case a create-only probe passed: `If-None-Match` enforced,
    /// `If-Match` ignored. Renew and steal would then silently clobber.
    #[test]
    fn ignored_if_match_is_not_cas() {
        assert_eq!(
            classify_cas_probe(&PreconditionFailed, &Written, &Written),
            Ok(false)
        );
        assert_eq!(
            classify_cas_probe(&PreconditionFailed, &NotImplemented, &Written),
            Ok(false)
        );
        assert_eq!(
            classify_cas_probe(
                &PreconditionFailed,
                &PreconditionFailed,
                &PreconditionFailed
            ),
            Ok(false),
            "refusing the current etag breaks renew"
        );
    }

    #[test]
    fn ignored_or_rejected_create_is_not_cas() {
        assert_eq!(
            classify_cas_probe(&Written, &PreconditionFailed, &Written),
            Ok(false)
        );
        assert_eq!(
            classify_cas_probe(&NotImplemented, &PreconditionFailed, &Written),
            Ok(false)
        );
    }

    #[test]
    fn transport_errors_are_indeterminate() {
        assert!(classify_cas_probe(&Other("timeout".into()), &Written, &Written).is_err());
        assert!(classify_cas_probe(&PreconditionFailed, &Other("x".into()), &Written).is_err());
        assert!(
            classify_cas_probe(&PreconditionFailed, &PreconditionFailed, &Other("x".into()))
                .is_err()
        );
    }
}
