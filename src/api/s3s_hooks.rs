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
}

#[async_trait::async_trait]
impl S3Auth for DeltaGliderS3sAuth {
    async fn get_secret_key(&self, access_key: &str) -> s3s::S3Result<SecretKey> {
        match self.iam_state.load().as_ref() {
            IamState::Disabled => {
                // s3s verifies every signed request, open mode included (it
                // needs the secret to read aws-chunked bodies). Open mode
                // accepts the "same access key + secret" dummy pattern
                // (test/test, anonymous/anonymous); any other pair fails
                // with SignatureDoesNotMatch, as the docs and the startup
                // banner say.
                Ok(SecretKey::from(access_key.to_string()))
            }
            IamState::Legacy(auth) if access_key == auth.access_key_id => {
                Ok(SecretKey::from(auth.secret_access_key.clone()))
            }
            IamState::Iam(index) => index
                .get(access_key)
                .filter(|user| user.enabled)
                .map(|user| SecretKey::from(user.secret_access_key.clone()))
                .ok_or_else(|| s3s::s3_error!(InvalidAccessKeyId)),
            _ => Err(s3s::s3_error!(InvalidAccessKeyId)),
        }
    }
}

/// s3s access hook: refuses a request unless s3s verified the identity the
/// SigV4 middleware resolved, then runs the post-verification gates.
#[derive(Clone)]
pub struct VerifiedIdentityS3sAccess;

#[async_trait::async_trait]
impl S3Access for VerifiedIdentityS3sAccess {
    async fn check(&self, cx: &mut S3AccessContext<'_>) -> s3s::S3Result<()> {
        // IAM/admission authorization is enforced by the outer Axum
        // middleware chain, against the identity the SigV4 middleware
        // resolved. This hook binds that identity to the key s3s verified
        // (see `resolved_identity_is_verified`). It also replaces s3s'
        // default "auth provider implies anonymous deny", which would
        // reject already-admitted public/open-mode requests.
        check_grant_xml_body_size(
            cx.s3_op().name(),
            cx.headers().get(axum::http::header::CONTENT_LENGTH),
        )?;
        let verified = cx.credentials().map(|c| c.access_key.clone());
        let resolved = crate::iam::request_principal(cx.extensions_mut()).cloned();
        if crate::api::auth::resolved_identity_is_verified(resolved.as_deref(), verified.as_deref())
        {
            // Only a real verified credential counts as an auth success
            // for the brute-force limiter; anonymous/open requests don't.
            if verified.is_some() {
                if let Some(outcome) = cx.extensions_mut().get::<crate::api::auth::AuthOutcome>() {
                    outcome.mark_verified();
                }
            }
            // The maintenance + backend-health gates run HERE, after s3s
            // verified the signature: their 503 names a busy bucket or a
            // backend's state, which a forged signature must not learn.
            let (method, path) = (cx.method().clone(), cx.uri().path().to_owned());
            // Through the one error bridge, like the form-POST path: the
            // client gets the gate's own message, and the response layer
            // adds the 503's `Retry-After`.
            crate::maintenance::gate::check_verified_request(cx.extensions_mut(), &method, &path)
                .map_err(s3s::S3Error::from)
        } else {
            tracing::warn!(
                "SECURITY | event=identity_mismatch | resolved={} | verified={}",
                resolved
                    .as_ref()
                    .map(|u| u.access_key_id.as_str())
                    .unwrap_or(""),
                verified.as_deref().unwrap_or("<none>")
            );
            Err(s3s::s3_error!(AccessDenied))
        }
    }
}

/// The largest XML request body s3s reads (s3s default: 20 MB). The
/// largest legitimate one, CompleteMultipartUpload with 10,000 parts, an
/// entity-escaped ETag, a SHA-256 checksum and indentation per part, is
/// about 2.1 MB; the cap leaves room for longer ETags and DeleteObjects
/// with 1,000 entity-escaped keys.
pub const S3S_XML_MAX_BODY_SIZE: usize = 4 * 1024 * 1024;

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
    std::sync::Arc::new(s3s::config::StaticConfigProvider::new(std::sync::Arc::new(
        config,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use s3s::config::S3ConfigProvider as _;

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
