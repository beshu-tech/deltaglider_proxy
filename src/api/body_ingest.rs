// SPDX-License-Identifier: BUSL-1.1

//! An S3 request body into the engine's ingest (`deltaglider::ingest`):
//! the checks the client's headers allow before the first byte, the body
//! read ONCE (de-framed when it is still aws-chunked, hashed on the way),
//! and the checks of what arrived. All of it runs before the object's write
//! lock: a failure drops the [`IngestedBody`], so nothing is stored and the
//! RAM and spool budgets are whole again.
//!
//! Order:
//!
//! 1. Before any byte: `Content-MD5` must be base64 (`InvalidDigest`); the
//!    checksum headers must name one digest (`InvalidRequest`); the request
//!    must declare its length (`411 MissingContentLength`); that length must
//!    be within the request's limit and the spool budget (`EntityTooLarge`);
//!    the bucket quota must admit it (`AccessDenied`).
//! 2. The body: s3s fails the read when the bytes do not hash to a signed
//!    `x-amz-content-sha256` (`BadDigest`); a still-framed body is decoded as
//!    it streams; the planned checksums are computed on the way.
//! 3. After the body: the client's checksum (header or trailer; an
//!    algorithm nobody announced is computed again over the stored body),
//!    a signed payload hash that s3s did not check, and `Content-MD5`.

use std::sync::Arc;

use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;

use crate::api::checksum::{self, ChecksumPlan};
use crate::api::handlers::AppState;
use crate::api::S3Error;
use crate::deltaglider::ingest::{
    self, IngestError, IngestPlan, IngestedBody, MissingContentLength,
};
use crate::deltaglider::KnownSha256;
use crate::s3_adapter_s3s::BodyContext;

/// What the client says about its body besides the bytes.
#[derive(Default)]
pub(crate) struct ContentChecks<'a> {
    /// The `Content-MD5` header.
    pub content_md5: Option<&'a str>,
    /// The signed `x-amz-content-sha256` (any value; only a hex digest is
    /// checked).
    pub signed_payload_hash: Option<&'a crate::api::auth::SignedPayloadHash>,
    /// The body's SHA-256 that s3s verifies while the body streams
    /// (`s3s_verified_payload_sha256`): it is not computed again.
    pub s3s_verified_sha256: Option<KnownSha256>,
}

/// Pure: the length the request declares for its body: the decoded length
/// of an aws-chunked body (never its framed length), else `Content-Length`,
/// else the exact length the transport knows (`transport_len`: a request
/// with neither `Content-Length` nor `Transfer-Encoding` has an empty body).
pub(crate) fn declared_len(
    headers: &axum::http::HeaderMap,
    transport_len: Option<u64>,
) -> Result<u64, MissingContentLength> {
    match ingest::declared_body_len(headers) {
        Ok(n) => Ok(n),
        Err(e) if crate::api::aws_chunked::is_aws_chunked(headers) => Err(e),
        Err(e) => transport_len.ok_or(e),
    }
}

/// Pure: the digest a `Content-MD5` header names; `InvalidDigest` when it is
/// not base64. A value of the wrong length cannot match and fails the
/// compare (`BadDigest`), as before.
pub(crate) fn parse_content_md5(value: Option<&str>) -> Result<Option<Vec<u8>>, S3Error> {
    use base64::Engine as _;
    value
        .map(|v| {
            base64::engine::general_purpose::STANDARD
                .decode(v.trim())
                .map_err(|_| S3Error::InvalidDigest)
        })
        .transpose()
}

/// The S3 answer to an ingest failure.
pub(crate) fn ingest_failure<E>(e: IngestError<E>) -> S3Error
where
    E: Into<S3Error> + std::fmt::Display,
{
    match e {
        IngestError::Source(e) => e.into(),
        IngestError::TooLarge { declared, limit } => S3Error::EntityTooLarge {
            size: declared,
            max: limit,
        },
        e @ IngestError::SpoolTooSmall { .. } => S3Error::EntityTooLargeReason(e.to_string()),
        e @ IngestError::TooLong { .. } => S3Error::InvalidArgument(e.to_string()),
        e @ IngestError::Incomplete { .. } => S3Error::IncompleteBody(e.to_string()),
        IngestError::Engine(e) => e.into(),
    }
}

/// A failed read of the request body. s3s wraps the body in a reader that
/// checks `x-amz-content-sha256` AS THE BODY STREAMS and fails the read with
/// `UploadStreamError::Sha256Mismatch` (Display `"UploadStreamError:
/// Sha256Mismatch"`): that is the payload-hash check, answered `BadDigest`
/// (400), not a 500. The match is on Display because the type is private to
/// s3s; `test_sigv4_payload_hash_mismatch_rejected` catches a rename.
pub(crate) fn body_read_error(e: s3s::StdError) -> S3Error {
    if e.to_string().contains("Sha256Mismatch") {
        tracing::warn!(
            "request body sha256 doesn't match x-amz-content-sha256 header — rejecting as BadDigest"
        );
        return S3Error::BadDigest;
    }
    tracing::error!(error = ?e, "request body: chunk read failed");
    S3Error::InternalError(format!("failed to read request body: {e}"))
}

/// THE ingest of an S3 object body (PutObject): see the module docs.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn ingest_client_body(
    state: &Arc<AppState>,
    bucket: &str,
    key: &str,
    body: Option<s3s::dto::StreamingBlob>,
    ctx: &BodyContext,
    limit: u64,
    checks: ContentChecks<'_>,
) -> Result<IngestedBody, S3Error> {
    use s3s::stream::ByteStream as _;
    let engine = state.engine.load_full();
    let headers = &ctx.headers;

    // (1) Before any byte.
    let expected_md5 = parse_content_md5(checks.content_md5)?;
    checksum::precheck_headers(headers)?;
    let transport_len = match &body {
        Some(b) => b.remaining_length().exact().map(|n| n as u64),
        None => Some(0),
    };
    let declared =
        declared_len(headers, transport_len).map_err(|_| S3Error::MissingContentLength)?;
    let plan = IngestPlan {
        declared,
        limit,
        known_sha256: checks.s3s_verified_sha256,
        held_spool_mib: 0,
        strict_length: true,
    };
    ingest::admit_declared::<S3Error>(&plan, engine.spool_max_bytes()).map_err(ingest_failure)?;
    crate::api::handlers::object_helpers::check_quota_for_write(state, bucket, key, declared)
        .await?;

    // (2) The body.
    let raw: BoxStream<'static, Result<Bytes, S3Error>> = match body {
        Some(b) => b.map(|r| r.map_err(body_read_error)).boxed(),
        None => futures::stream::empty().boxed(),
    };
    let mut trailer_slot = None;
    let source = if ctx.still_chunked && crate::api::aws_chunked::is_aws_chunked(headers) {
        let decoder = crate::api::aws_chunked::AwsChunkedStream::new(raw, Some(declared));
        trailer_slot = Some(decoder.trailers());
        decoder
            .map(|r| {
                r.map_err(|e| match e {
                    crate::api::aws_chunked::AwsChunkedStreamError::Source(e) => e,
                    crate::api::aws_chunked::AwsChunkedStreamError::Framing(e) => {
                        S3Error::InvalidArgument(format!(
                            "Failed to decode AWS chunked transfer encoding: {e}"
                        ))
                    }
                })
            })
            .boxed()
    } else {
        raw
    };
    let checksum_plan = ChecksumPlan::from_headers(headers);
    let mut sums = (!checksum_plan.is_empty()).then(|| checksum_plan.hasher());
    let observed = source.inspect(|chunk| {
        if let (Ok(chunk), Some(sums)) = (chunk, sums.as_mut()) {
            sums.update(chunk);
        }
    });
    let ingested = ingest::ingest(&engine, bucket, observed, plan)
        .await
        .map_err(ingest_failure)?;

    // (3) After the body.
    let own_trailers = trailer_slot.and_then(|slot| slot.get());
    let s3s_trailers = ctx.s3s_trailers.as_ref().and_then(|t| t.read(Clone::clone));
    let trailers = own_trailers.as_ref().or(s3s_trailers.as_ref());
    if let Some((algorithm, expected)) = checksum::expected_checksum(headers, trailers)? {
        let planned = sums
            .map(|h| h.finalize())
            .and_then(|done| checksum::computed_digest(algorithm, &done));
        let digest = match planned {
            Some(digest) => digest,
            None => checksum::digest_of(algorithm, &ingested)
                .await
                .map_err(|e| S3Error::InternalError(format!("checksum of the body: {e}")))?,
        };
        checksum::verify_digest(algorithm, &expected, &digest)?;
    }
    if checks.s3s_verified_sha256.is_none() {
        if let Some(signed) = checks.signed_payload_hash {
            signed.verify_against_digest(ingested.hashes().sha256())?;
        }
    }
    if let Some(expected) = expected_md5 {
        if hex::encode(expected) != ingested.hashes().md5() {
            return Err(S3Error::BadDigest);
        }
    }
    Ok(ingested)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn declared_len_truth_table() {
        let chunked = ("x-amz-content-sha256", "STREAMING-UNSIGNED-PAYLOAD-TRAILER");
        type Case<'a> = (
            &'a [(&'static str, &'a str)],
            Option<u64>,
            Result<u64, MissingContentLength>,
        );
        let cases: &[Case] = &[
            (&[("content-length", "7")], None, Ok(7)),
            (&[("content-length", "7")], Some(9), Ok(7)),
            // No header: the transport's exact length (an empty body has 0).
            (&[], Some(5), Ok(5)),
            (&[], Some(0), Ok(0)),
            // Plain `Transfer-Encoding: chunked`: no length anywhere.
            (&[], None, Err(MissingContentLength)),
            (
                &[chunked, ("x-amz-decoded-content-length", "3")],
                Some(99),
                Ok(3),
            ),
            // An aws-chunked body never falls back to its framed length.
            (
                &[chunked, ("content-length", "99")],
                Some(99),
                Err(MissingContentLength),
            ),
        ];
        for (pairs, transport, want) in cases {
            assert_eq!(
                declared_len(&headers(pairs), *transport),
                *want,
                "{pairs:?} {transport:?}"
            );
        }
    }

    #[test]
    fn content_md5_is_parsed_before_the_body() {
        use base64::Engine as _;
        use md5::Digest as _;
        let good = base64::engine::general_purpose::STANDARD.encode(md5::Md5::digest(b"abc"));
        assert_eq!(
            parse_content_md5(Some(&good)).unwrap(),
            Some(md5::Md5::digest(b"abc").to_vec())
        );
        assert_eq!(parse_content_md5(None).unwrap(), None);
        assert_eq!(
            parse_content_md5(Some("not base64!")).unwrap_err().code(),
            "InvalidDigest"
        );
    }

    #[test]
    fn ingest_failures_keep_their_s3_codes() {
        let code = |e: IngestError<S3Error>| ingest_failure(e).code();
        assert_eq!(code(IngestError::Source(S3Error::BadDigest)), "BadDigest");
        assert_eq!(
            code(IngestError::TooLarge {
                declared: 2,
                limit: 1
            }),
            "EntityTooLarge"
        );
        let spool = ingest_failure::<S3Error>(IngestError::SpoolTooSmall {
            declared: 2,
            max_bytes: 1,
        });
        assert_eq!(spool.code(), "EntityTooLarge");
        assert!(spool.to_string().contains("DGP_SPOOL_MAX_BYTES"), "{spool}");
        assert_eq!(
            code(IngestError::TooLong { declared: 1 }),
            "InvalidArgument"
        );
        assert_eq!(
            code(IngestError::Incomplete {
                declared: 2,
                received: 1
            }),
            "IncompleteBody"
        );
        assert_eq!(
            code(IngestError::Engine(
                crate::deltaglider::EngineError::Overloaded("x".into())
            )),
            "SlowDown"
        );
    }

    #[test]
    fn a_failed_read_is_bad_digest_only_for_the_payload_hash() {
        let mismatch: s3s::StdError = "UploadStreamError: Sha256Mismatch".into();
        assert_eq!(body_read_error(mismatch).code(), "BadDigest");
        let reset: s3s::StdError = "connection reset".into();
        assert_eq!(body_read_error(reset).code(), "InternalError");
    }
}
