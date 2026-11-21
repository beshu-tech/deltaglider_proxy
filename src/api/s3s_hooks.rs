// SPDX-License-Identifier: BUSL-1.1

//! The two s3s hooks the S3 router installs: the secret-key lookup s3s
//! verifies signatures with, and the access check that binds the identity
//! our SigV4 middleware resolved to the key s3s verified.
//!
//! They live in the library (not `startup.rs`) so the middleware-vs-s3s
//! contract test drives the same hooks as production.

use s3s::access::{S3Access, S3AccessContext};
use s3s::auth::{S3Auth, SecretKey};

use crate::iam::{IamState, SharedIamState};

/// s3s secret-key provider backed by the live IAM state.
#[derive(Clone)]
pub struct DeltaGliderS3sAuth {
    pub iam_state: SharedIamState,
    /// The coordination status (`Default`: no coordination bucket).
    pub coordination: crate::coordination::CoordinationStatus,
}

#[async_trait::async_trait]
impl S3Auth for DeltaGliderS3sAuth {
    async fn get_secret_key(&self, access_key: &str) -> s3s::S3Result<SecretKey> {
        let iam = self.iam_state.load();
        // The users of an unconfirmed IAM database are not looked up (the
        // SigV4 middleware refuses first; this is the second gate).
        let iam = self
            .coordination
            .credential_view(&iam)
            .map_err(|_| s3s::s3_error!(ServiceUnavailable))?;
        match iam {
            IamState::Disabled => {
                // s3s verifies every signed request, open mode included (it
                // needs the secret to read aws-chunked bodies). Open mode
                // accepts the "same access key + secret" dummy pattern
                // (test/test, anonymous/anonymous); any other pair fails
                // with SignatureDoesNotMatch, as the docs and the startup
                // banner say.
                Ok(SecretKey::from(access_key.to_string()))
            }
            IamState::Legacy(auth)
                if crate::security::secret_eq(
                    access_key.as_bytes(),
                    auth.access_key_id.as_bytes(),
                ) =>
            {
                Ok(SecretKey::from(auth.secret_access_key.clone()))
            }
            // An unknown or disabled key id: a secret nobody has, so s3s
            // answers SignatureDoesNotMatch as for a wrong secret.
            IamState::Iam(index) => Ok(SecretKey::from(
                index
                    .get(access_key)
                    .filter(|user| user.enabled)
                    .map_or_else(
                        || crate::security::unmatchable_secret().to_string(),
                        |user| user.secret_access_key.clone(),
                    ),
            )),
            _ => Ok(SecretKey::from(
                crate::security::unmatchable_secret().to_string(),
            )),
        }
    }
}

/// The only S3 operations the anonymous (`$anonymous`) principal may run:
/// public reads and public listings. Judged on the s3s operation, never on
/// the method or the query text.
const ANONYMOUS_OPS: [&str; 4] = ["GetObject", "HeadObject", "ListObjects", "ListObjectsV2"];

/// Pure: whether the anonymous principal may run the s3s operation `op`.
pub(crate) fn anonymous_may_run(op: &str) -> bool {
    ANONYMOUS_OPS.contains(&op)
}

/// s3s access hook: refuses a request unless s3s verified the identity the
/// SigV4 middleware resolved, then answers the authorization verdict that
/// the IAM middleware recorded, then runs the post-verification gates.
///
/// THE order: no layer before this hook shows an authorization verdict (or
/// anything else that depends on which key ids exist) to a caller whose
/// signature s3s has not verified. This hook runs after s3s checked the
/// signature and before s3s reads any body (`ops/mod.rs`: the access check
/// precedes `extract_full_body`, and no operation handler has run).
#[derive(Clone)]
pub struct VerifiedIdentityS3sAccess;

#[async_trait::async_trait]
impl S3Access for VerifiedIdentityS3sAccess {
    async fn check(&self, cx: &mut S3AccessContext<'_>) -> s3s::S3Result<()> {
        // The outer middleware resolves identity and records the IAM verdict;
        // this hook binds it to the key s3s verified and answers it.
        check_grant_xml_body_size(
            cx.s3_op().name(),
            cx.headers().get(axum::http::header::CONTENT_LENGTH),
        )?;
        let verified = cx.credentials().map(|c| c.access_key.clone());
        let resolved = crate::iam::request_principal(cx.extensions_mut()).cloned();
        // No principal for the key id: s3s could verify only another identity.
        if cx
            .extensions_mut()
            .get::<crate::api::auth::UnresolvedCredential>()
            .is_some()
        {
            return Err(s3s::s3_error!(SignatureDoesNotMatch));
        }
        if resolved.as_deref().is_some_and(|u| u.is_anonymous())
            && !anonymous_may_run(cx.s3_op().name())
        {
            tracing::warn!(
                "SECURITY | event=anonymous_operation_refused | op={}",
                cx.s3_op().name()
            );
            return Err(s3s::s3_error!(AccessDenied));
        }
        if crate::api::auth::resolved_identity_is_verified(resolved.as_deref(), verified.as_deref())
        {
            // Only a real verified credential counts as an auth success
            // for the brute-force limiter; anonymous/open requests don't.
            if verified.is_some() {
                if let Some(outcome) = cx.extensions_mut().get::<crate::api::auth::AuthOutcome>() {
                    outcome.mark_verified();
                    if resolved.as_deref().is_some_and(|u| !u.is_anonymous()) {
                        outcome.mark_identified();
                    }
                }
            }
            // The IAM verdict is shown only now: the signature is verified
            // (or the request needs none: anonymous, open access).
            if let Some(denied) = cx
                .extensions_mut()
                .get::<crate::iam::middleware::AuthzDenied>()
                .cloned()
            {
                // Audit-log every IAM denial with the resolved (action,
                // bucket, key) the check evaluated.
                crate::audit::audit_log(
                    "access_denied",
                    &denied.user,
                    &format!("{:?}", denied.action),
                    cx.headers(),
                    &denied.bucket,
                    &denied.key,
                );
                return Err(s3s::s3_error!(AccessDenied));
            }
            // The maintenance + backend-health gates run HERE, after s3s
            // verified the signature: their 503 names a busy bucket or a
            // backend's state, which a forged signature must not learn.
            let (method, path) = (cx.method().clone(), cx.uri().path().to_owned());
            let query = cx.uri().query().map(str::to_owned);
            // Through the one error bridge, like the form-POST path: the
            // client gets the gate's own message, and the response layer
            // adds the 503's `Retry-After`.
            crate::maintenance::gate::check_verified_request_q(
                cx.extensions_mut(),
                &method,
                &path,
                query.as_deref(),
            )
            .map_err(s3s::S3Error::from)
        } else {
            // s3s verified another identity than the one we resolved.
            tracing::warn!(
                "SECURITY | event=identity_mismatch | resolved={} | verified={}",
                resolved
                    .as_ref()
                    .map(|u| u.access_key_id.as_str())
                    .unwrap_or(""),
                verified.as_deref().unwrap_or("<none>")
            );
            Err(s3s::s3_error!(SignatureDoesNotMatch))
        }
    }
}

/// The largest XML request body s3s reads (s3s default: 20 MB). The
/// largest legitimate one, CompleteMultipartUpload with 10,000 parts, an
/// entity-escaped ETag, a SHA-256 checksum and indentation per part, is
/// about 2.1 MB; the cap leaves room for longer ETags and DeleteObjects
/// with 1,000 entity-escaped keys.
pub const S3S_XML_MAX_BODY_SIZE: usize = 4 * 1024 * 1024;

/// The largest file part s3s reads for a `PostObject`. The router never lets
/// such a request reach s3s (the browser form is served by the form handler,
/// every other multipart POST is refused), so this is a second bound.
pub const S3S_POST_OBJECT_MAX_FILE_SIZE: u64 = 1024 * 1024;

/// The operations whose XML body s3s parses through a `Grantee` start tag,
/// the only tag whose attributes it iterates (`Grant` and `TargetGrant` in
/// s3s 0.14 `xml/generated.rs`; re-check on an s3s bump). quick-xml 0.40 (pinned by
/// s3s 0.14) checks those attributes for duplicate names in O(N²)
/// (RUSTSEC-2026-0194): a 1 MB tag of 140,000 attributes took 17 s of CPU
/// in a release build. Their bodies get a smaller cap.
const GRANT_XML_OPS: [&str; 4] = [
    "PutBucketAcl",
    "PutObjectAcl",
    "PutBucketLogging",
    "RestoreObject",
];

/// An S3 ACL holds at most 100 grants: a few tens of KB of XML.
pub const GRANT_XML_MAX_BODY_SIZE: u64 = 64 * 1024;

/// Pure: refuses a `GRANT_XML_OPS` body whose `Content-Length` is over
/// `GRANT_XML_MAX_BODY_SIZE`. Without a valid `Content-Length`, s3s
/// refuses a non-empty XML body itself (`MissingContentLength`), and hyper
/// never delivers more bytes than the header names.
pub(crate) fn check_grant_xml_body_size(
    op: &str,
    content_length: Option<&axum::http::HeaderValue>,
) -> s3s::S3Result<()> {
    let len = content_length
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    match len {
        Some(len) if GRANT_XML_OPS.contains(&op) && len > GRANT_XML_MAX_BODY_SIZE => {
            Err(s3s::s3_error!(
                MaxMessageLengthExceeded,
                "the {op} body is over {GRANT_XML_MAX_BODY_SIZE} bytes"
            ))
        }
        _ => Ok(()),
    }
}

/// The s3s configuration the S3 router runs with.
/// `clock_skew_secs`: `DGP_CLOCK_SKEW_SECONDS` ([`crate::config::RuntimeTuning`]).
pub fn s3s_config(clock_skew_secs: u32) -> std::sync::Arc<s3s::config::StaticConfigProvider> {
    // Pass the documented skew tolerance to s3s; it used its own default.
    let mut config = s3s::config::S3Config::default();
    config.presigned_url_max_skew_time_secs = clock_skew_secs;
    config.xml_max_body_size = S3S_XML_MAX_BODY_SIZE;
    config.post_object_max_file_size = S3S_POST_OBJECT_MAX_FILE_SIZE;
    std::sync::Arc::new(s3s::config::StaticConfigProvider::new(std::sync::Arc::new(
        config,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use s3s::config::S3ConfigProvider as _;

    #[test]
    fn the_anonymous_principal_runs_only_reads_and_lists() {
        for op in ["GetObject", "HeadObject", "ListObjects", "ListObjectsV2"] {
            assert!(anonymous_may_run(op), "{op}");
        }
        for op in [
            "ListMultipartUploads",
            "ListParts",
            "ListObjectVersions",
            "ListBuckets",
            "HeadBucket",
            "GetBucketAcl",
            "GetBucketVersioning",
            "GetBucketLocation",
            "GetObjectAcl",
            "GetObjectTagging",
            "GetObjectAttributes",
            "PutObject",
            "CreateMultipartUpload",
            "DeleteObject",
            "PostObject",
        ] {
            assert!(!anonymous_may_run(op), "{op}");
        }
    }

    /// Source guard: the access hook answers the recorded authorization
    /// verdict only after it bound the identity to the key s3s verified.
    #[test]
    fn the_authorization_verdict_is_answered_after_the_identity_is_verified() {
        let text: String =
            crate::source_scan::prod_lines(&crate::source_scan::read("src/api/s3s_hooks.rs"))
                .into_iter()
                .map(|(_, l)| format!("{l}\n"))
                .collect();
        let verified = text
            .find("resolved_identity_is_verified(resolved")
            .expect("the identity check");
        let verdict = text
            .find("AuthzDenied>()")
            .expect("the recorded verdict is read");
        assert!(
            verified < verdict,
            "the verdict is answered before the identity check"
        );
    }

    #[test]
    fn s3s_reads_a_post_object_file_up_to_a_small_cap() {
        assert_eq!(
            s3s_config(900).snapshot().post_object_max_file_size,
            S3S_POST_OBJECT_MAX_FILE_SIZE
        );
    }

    #[test]
    fn s3s_reads_xml_bodies_up_to_the_cap() {
        assert_eq!(
            s3s_config(900).snapshot().xml_max_body_size,
            S3S_XML_MAX_BODY_SIZE
        );
    }

    /// The largest legitimate XML body fits under the cap and parses.
    #[test]
    fn the_largest_complete_multipart_upload_fits_the_cap() {
        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <CompleteMultipartUpload xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\n",
        );
        let sha256 = "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";
        for n in 1..=10_000 {
            xml.push_str(&format!(
                "    <Part>\n        <ETag>&quot;{n:032x}&quot;</ETag>\n        \
                 <ChecksumSHA256>{sha256}</ChecksumSHA256>\n        \
                 <PartNumber>{n}</PartNumber>\n    </Part>\n"
            ));
        }
        xml.push_str("</CompleteMultipartUpload>\n");
        assert!(
            xml.len() <= S3S_XML_MAX_BODY_SIZE,
            "{} bytes > cap {}",
            xml.len(),
            S3S_XML_MAX_BODY_SIZE
        );
        let mut de = s3s::xml::Deserializer::new(xml.as_bytes());
        let parsed: s3s::dto::CompletedMultipartUpload =
            s3s::xml::Deserialize::deserialize(&mut de).expect("parses");
        assert_eq!(parsed.parts.map(|p| p.len()), Some(10_000));
    }

    #[test]
    fn grant_xml_bodies_over_the_cap_are_refused() {
        let len = |n: u64| axum::http::HeaderValue::from_str(&n.to_string()).unwrap();
        let over = len(GRANT_XML_MAX_BODY_SIZE + 1);
        let at = len(GRANT_XML_MAX_BODY_SIZE);
        for op in GRANT_XML_OPS {
            let err = check_grant_xml_body_size(op, Some(&over)).unwrap_err();
            assert_eq!(err.code(), &s3s::S3ErrorCode::MaxMessageLengthExceeded);
            assert!(check_grant_xml_body_size(op, Some(&at)).is_ok());
            assert!(check_grant_xml_body_size(op, None).is_ok());
        }
        for op in ["CompleteMultipartUpload", "DeleteObjects", "PutObject"] {
            assert!(check_grant_xml_body_size(op, Some(&over)).is_ok());
        }
    }

    /// The worst `Grantee` tag the cap lets through: about 9,300 distinct
    /// attributes. It took 0.07 s (release) and 0.4 s (debug) to refuse.
    #[test]
    fn the_worst_grantee_tag_under_the_cap_parses_fast() {
        let mut xml = String::from("<AccessControlPolicy><AccessControlList><Grant><Grantee");
        let alphabet: Vec<char> = ('a'..='z').chain('A'..='Z').collect();
        'outer: for a in &alphabet {
            for b in &alphabet {
                for c in &alphabet {
                    if xml.len() as u64 + 200 > GRANT_XML_MAX_BODY_SIZE {
                        break 'outer;
                    }
                    xml.push_str(&format!(" {a}{b}{c}=''"));
                }
            }
        }
        xml.push_str(
            "><ID>x</ID></Grantee><Permission>READ</Permission></Grant>\
             </AccessControlList></AccessControlPolicy>",
        );
        assert!(xml.len() as u64 <= GRANT_XML_MAX_BODY_SIZE);
        let start = std::time::Instant::now();
        let mut de = s3s::xml::Deserializer::new(xml.as_bytes());
        let _: Result<s3s::dto::AccessControlPolicy, _> =
            s3s::xml::Deserialize::deserialize(&mut de);
        let took = start.elapsed();
        assert!(took < std::time::Duration::from_secs(10), "{took:?}");
    }
}
