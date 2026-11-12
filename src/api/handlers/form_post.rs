// SPDX-License-Identifier: BUSL-1.1

//! Browser presigned form-POST upload handling.
//!
//! S3 lets a browser upload directly to a bucket via an HTML form whose
//! action is `POST https://<host>/<bucket>` with `multipart/form-data`
//! body, where the SigV4 policy + signature live in form fields rather
//! than headers. This module owns the entire ingest pipeline for that
//! flow.
//!
//! Stages, in the order of the bytes on the wire: `form_post_bucket`
//! picks the requests the router hands here; `read_form_head` streams the
//! multipart body through `multer` up to the file part, holding only the
//! small fields before it (each at most `FORM_FIELD_MAX`) and the file
//! part's headers; `authenticate_form_post` rebuilds the SigV4 signing key,
//! verifies the form signature in constant time, decodes and validates the
//! policy document (`content-length-range` only for its form), and
//! authorises the user against IAM on the resolved key. Only then, after
//! the client-write gates and the bucket-existence check,
//! `ingest_form_file` reads the file once, into memory or a spool file,
//! capped by `form_file_cap`; the fields after the file are read under
//! the same small limit and ignored, as on S3. `handle_form_post_upload`
//! hands the body to `store_client_write` (quota, the object's write lock,
//! the `If-Match`/`If-None-Match` preconditions, the store, the
//! object-created event) and writes the audit log. An unauthenticated
//! form therefore costs the proxy its small fields, never its file.
//!
//! Lives in its own module because none of this code path is shared
//! with the GET/PUT/HEAD/DELETE handlers in `object.rs` — the only
//! coupling is the dispatcher choosing between DeleteObjects and form
//! upload at the top of `delete_objects`.
//!
//! ## No replay guard (deliberate — matches AWS)
//!
//! Native S3 has NO form-POST replay protection: a presigned POST is valid for
//! as many policy-conforming uploads as fit within its expiration (the
//! AWS-intended `starts-with $key` batch pattern). An earlier signature-keyed
//! guard 403'd that legitimate pattern (same-size files signed in the same
//! second share a signature) and was removed; a later write-only ledger added
//! per-request SHA-256 + eviction cost while never rejecting, and was removed
//! too. The policy's own conditions — `starts-with $key`,
//! `content-length-range`, `expiration` — are re-validated on EVERY request
//! (`validate_form_post_policy`) and are the real bound on a leaked signature.

use super::object_helpers::{check_client_write_allowed, store_client_write, ClientWrite};
use super::{audit_log_s3, ensure_bucket_exists, AppState};
use crate::api::errors::S3Error;
use crate::deltaglider::ingest::{Destination, IngestError, IngestPlan, IngestedBody, Sha256Need};
use crate::iam::{AuthenticatedUser, IamState, S3Action, SharedIamState};
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use chrono::Utc;
use futures::{Stream, StreamExt, TryStreamExt};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// The largest form field other than the file (a policy, a signature, a
/// metadata value). The proxy holds these fields in memory.
pub(crate) const FORM_FIELD_MAX: u64 = 64 * 1024;

/// The largest sum of the field values on one side of the file (before
/// it, and after it).
pub(crate) const FORM_FIELDS_MAX: u64 = 1024 * 1024;

/// What a form may carry beyond its file: the fields on both sides of the
/// file, the part headers and the boundaries.
pub(crate) const FORM_OVERHEAD: u64 = 4 * FORM_FIELDS_MAX;

/// The most form bytes the proxy reads on one side of the file (the
/// fields, their names, the part headers and the boundaries), counted on
/// the body stream itself ([`FormMeter`]). The multipart parser keeps what
/// it polled until a part header ends, so without this bound one part
/// header that never ends, or a flood of empty fields, held up to the whole
/// form in memory before the signature check.
pub(crate) const FORM_SIDE_MAX: u64 = FORM_OVERHEAD + FORM_FIELDS_MAX;

/// The most fields on one side of the file. Each one costs a map entry
/// besides its bytes, so an empty field is not free.
pub(crate) const FORM_FIELD_COUNT_MAX: usize = 1000;

/// Counts the bytes of a form body as the parser polls them, and refuses
/// the stream once more than [`FORM_SIDE_MAX`] were polled since
/// [`FormMeter::bound`]. Shared between the stream (which counts) and the
/// handler (which moves the bound around the file).
#[derive(Clone, Default)]
struct FormMeter {
    polled: Arc<std::sync::atomic::AtomicU64>,
    /// Polled-byte count past which the stream fails; `u64::MAX` while the
    /// file is read (the ingest caps the file).
    allowance: Arc<std::sync::atomic::AtomicU64>,
}

/// The stream error of a form side past [`FORM_SIDE_MAX`] (or past the
/// field count, see [`count_field`]).
#[derive(Debug)]
struct FormSideTooLarge;

impl std::fmt::Display for FormSideTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the form fields on one side of the file, with their names and part headers, are \
             larger than {FORM_SIDE_MAX} bytes"
        )
    }
}

impl std::error::Error for FormSideTooLarge {}

impl FormMeter {
    /// From now on, at most [`FORM_SIDE_MAX`] more bytes.
    fn bound(&self) {
        use std::sync::atomic::Ordering::SeqCst;
        let polled = self.polled.load(SeqCst);
        self.allowance
            .store(polled.saturating_add(FORM_SIDE_MAX), SeqCst);
    }

    /// No bound: the file (the ingest caps it).
    fn unbound(&self) {
        self.allowance
            .store(u64::MAX, std::sync::atomic::Ordering::SeqCst);
    }

    /// `body`, counted. A chunk polled after the allowance is used up fails
    /// the stream, so the parser holds at most the allowance plus one chunk.
    fn meter<S, E>(
        &self,
        body: S,
    ) -> impl Stream<Item = Result<Bytes, Box<dyn std::error::Error + Send + Sync>>> + Send + 'static
    where
        S: Stream<Item = Result<Bytes, E>> + Send + 'static,
        E: Into<Box<dyn std::error::Error + Send + Sync>> + 'static,
    {
        use std::sync::atomic::Ordering::SeqCst;
        let meter = self.clone();
        body.map(move |chunk| {
            let chunk = chunk.map_err(Into::into)?;
            let before = meter.polled.fetch_add(chunk.len() as u64, SeqCst);
            if before >= meter.allowance.load(SeqCst) {
                return Err(Box::new(FormSideTooLarge) as _);
            }
            Ok(chunk)
        })
    }
}

/// Count one more field on a side of the file: past
/// [`FORM_FIELD_COUNT_MAX`] the form is refused. Its name is charged to
/// the side's value budget (`budget`), as its value is.
fn count_field(count: &mut usize, name: &str, budget: &mut u64) -> Result<(), S3Error> {
    *count += 1;
    let name_len = name.len() as u64;
    if *count > FORM_FIELD_COUNT_MAX || name_len > *budget {
        return Err(S3Error::InvalidArgument(format!(
            "a POST form may have at most {FORM_FIELD_COUNT_MAX} fields on each side of the \
             file, and their names and values together at most {FORM_FIELDS_MAX} bytes"
        )));
    }
    *budget -= name_len;
    Ok(())
}

/// The form up to its file part: the fields before the file, and what
/// the file part's headers say (its file name and content type).
#[derive(Debug)]
struct FormHead {
    key_field: String,
    resolved_key: String,
    content_type: Option<String>,
    user_metadata: HashMap<String, String>,
    fields_ci: HashMap<String, String>,
}

/// The policy's `content-length-range`: the file length it allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LengthRange {
    min: u64,
    max: u64,
}

impl LengthRange {
    /// Two ranges in one policy: a file must be in both.
    fn and(self, other: Self) -> Self {
        Self {
            min: self.min.max(other.min),
            max: self.max.min(other.max),
        }
    }
}

/// A verified form: its principal (`None` without authentication) and
/// the file length its policy allows.
struct FormAuth {
    user: Option<Arc<AuthenticatedUser>>,
    length_range: Option<LengthRange>,
}

/// What bounds the file of a form ([`form_file_cap`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileBound {
    /// The largest object (`max_object_size`).
    Limit,
    /// The form's `Content-Length`: the file is part of the form.
    Form,
    /// The policy's `content-length-range` maximum.
    Policy,
}

/// The most bytes the file of a form may have, and what sets that bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileCap {
    bytes: u64,
    bound: FileBound,
}

/// Pure: the cap of a form's file, the smallest of the object limit, the
/// form's own length and the policy's `content-length-range` maximum. The
/// ingest reserves this much and stops the file at the first byte past
/// it; a shorter file is the file (its exact size is known after the
/// read). On a tie the policy names the bound, so a file over the
/// policy's maximum is refused as the policy refuses it (`AccessDenied`).
fn form_file_cap(limit: u64, form_len: u64, range_max: Option<u64>) -> FileCap {
    let mut cap = FileCap {
        bytes: limit,
        bound: FileBound::Limit,
    };
    if form_len < cap.bytes {
        cap = FileCap {
            bytes: form_len,
            bound: FileBound::Form,
        };
    }
    if let Some(max) = range_max.filter(|max| *max <= cap.bytes) {
        cap = FileCap {
            bytes: max,
            bound: FileBound::Policy,
        };
    }
    cap
}

/// Pure: the answer to a file longer than its cap.
fn over_cap_refusal(cap: FileCap, bucket: &str, key: &str) -> S3Error {
    match cap.bound {
        FileBound::Policy => {
            tracing::warn!(
                "form-POST DENY | reason=content_length_range | bucket={bucket} key={key} file_len>{}",
                cap.bytes
            );
            S3Error::AccessDenied
        }
        FileBound::Limit => S3Error::EntityTooLarge {
            size: cap.bytes.saturating_add(1),
            max: cap.bytes,
        },
        FileBound::Form => S3Error::InvalidRequest("The file part is longer than the form.".into()),
    }
}

/// Pure: is a file of `len` bytes within the policy's range? The cap
/// enforced the maximum while the file arrived; the minimum needs the
/// whole file.
fn check_file_len(
    range: Option<LengthRange>,
    len: u64,
    bucket: &str,
    key: &str,
) -> Result<(), S3Error> {
    match range {
        Some(LengthRange { min, max }) if len < min || len > max => {
            tracing::warn!(
                "form-POST DENY | reason=content_length_range | bucket={bucket} key={key} file_len={len} range=[{min},{max}]"
            );
            Err(S3Error::AccessDenied)
        }
        _ => Ok(()),
    }
}

/// Pure: the length a form declares (`Content-Length`). A form that
/// declares none gets `411 MissingContentLength`; one longer than the
/// largest object plus [`FORM_OVERHEAD`] gets `400 EntityTooLarge`. Both
/// before a byte of it is read.
fn form_body_len(headers: &HeaderMap, limit: u64) -> Result<u64, S3Error> {
    let len = crate::api::body_ingest::declared_len(headers, None)
        .map_err(|_| S3Error::MissingContentLength)?;
    if len > limit.saturating_add(FORM_OVERHEAD) {
        return Err(S3Error::EntityTooLarge {
            size: len,
            max: limit,
        });
    }
    Ok(len)
}

/// Pure: is this part the file? S3 names it `file`; a browser marks a
/// file input with a file name.
fn is_file_part(lc_name: &str, file_name: Option<&str>) -> bool {
    file_name.is_some() || lc_name == "file"
}

/// The S3 answer to a failed read of the form.
fn form_error(e: multer::Error) -> S3Error {
    match e {
        multer::Error::StreamSizeExceeded { .. } | multer::Error::FieldSizeExceeded { .. } => {
            S3Error::EntityTooLargeReason(e.to_string())
        }
        multer::Error::StreamReadFailed(e) if e.is::<FormSideTooLarge>() => {
            S3Error::InvalidArgument(e.to_string())
        }
        multer::Error::StreamReadFailed(e) => {
            S3Error::IncompleteBody(format!("The form body ended early: {e}"))
        }
        e => S3Error::InvalidRequest(format!("Malformed multipart/form-data body: {e}")),
    }
}

/// Read one field other than the file: at most [`FORM_FIELD_MAX`] bytes,
/// and at most `budget` (what is left of [`FORM_FIELDS_MAX`] on this side
/// of the file), in chunks as they arrive.
async fn read_small_field(
    mut field: multer::Field<'static>,
    name: &str,
    budget: &mut u64,
) -> Result<String, S3Error> {
    let mut value = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(form_error)? {
        let len = value.len() as u64 + chunk.len() as u64;
        if len > FORM_FIELD_MAX || len > *budget {
            return Err(S3Error::InvalidArgument(format!(
                "POST form field '{name}' is larger than {FORM_FIELD_MAX} bytes, or the form \
                 fields together are larger than {FORM_FIELDS_MAX} bytes"
            )));
        }
        value.extend_from_slice(&chunk);
    }
    *budget -= value.len() as u64;
    Ok(String::from_utf8_lossy(&value).into_owned())
}

/// True iff the request looks like an S3 browser form POST
/// (`multipart/form-data` body). Cheap header sniff — the dispatcher in
/// `object::delete_objects` calls this before deciding whether to run
/// the DeleteObjects XML path or the form-upload path.
pub fn is_multipart_form_upload(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.to_ascii_lowercase().starts_with("multipart/form-data"))
        .unwrap_or(false)
}

/// THE form-POST predicate: the bucket of a browser form upload that the
/// router serves with the form handler instead of s3s, or `None`. Shape:
/// `POST /{bucket}` (or `/{bucket}/`), `multipart/form-data`, no query, no
/// `Authorization` header. The SigV4 middleware defers exactly these
/// requests to the handler's policy check, and the router intercepts exactly
/// these, so the two sets cannot drift.
///
/// The path is decoded like s3s decodes it (`RequestTarget`), so admission,
/// the deferral and the handler name ONE bucket: `/pro%64` is `prod`, and
/// `//prod` (empty bucket for admission and s3s) is no form upload at all.
/// A raw-path split served `//prod` as an upload into `prod` that no
/// admission block on `prod` saw.
pub fn form_post_bucket(
    method: &axum::http::Method,
    uri: &axum::http::Uri,
    headers: &HeaderMap,
) -> Option<String> {
    if method != axum::http::Method::POST
        || !is_multipart_form_upload(headers)
        || uri.query().is_some()
        || headers.contains_key(axum::http::header::AUTHORIZATION)
    {
        return None;
    }
    let target = crate::api::request_target::RequestTarget::from_uri(uri).ok()?;
    target.bucket_only().map(str::to_string)
}

fn derive_v4_signing_key(secret_access_key: &str, date: &str, region: &str) -> [u8; 32] {
    let k_date = hmac_sha256(
        format!("AWS4{}", secret_access_key).as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    hmac_sha256(&k_service, b"aws4_request")
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC key init");
    mac.update(data);
    let out = mac.finalize().into_bytes();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    arr
}

fn parse_policy_scope(credential: &str) -> Result<(&str, &str, &str), S3Error> {
    let mut parts = credential.split('/');
    let access_key = parts
        .next()
        .ok_or_else(|| S3Error::InvalidArgument("x-amz-credential is missing access key".into()))?;
    let date = parts
        .next()
        .ok_or_else(|| S3Error::InvalidArgument("x-amz-credential is missing scope date".into()))?;
    let region = parts.next().ok_or_else(|| {
        S3Error::InvalidArgument("x-amz-credential is missing scope region".into())
    })?;
    let service = parts
        .next()
        .ok_or_else(|| S3Error::InvalidArgument("x-amz-credential is missing service".into()))?;
    let term = parts.next().ok_or_else(|| {
        S3Error::InvalidArgument("x-amz-credential is missing terminal scope".into())
    })?;
    if parts.next().is_some() {
        return Err(S3Error::InvalidArgument(
            "x-amz-credential has unexpected extra scope components".into(),
        ));
    }
    if service != "s3" || term != "aws4_request" {
        return Err(S3Error::InvalidArgument(
            "x-amz-credential must use */*/s3/aws4_request scope".into(),
        ));
    }
    Ok((access_key, date, region))
}

/// The form's `x-amz-checksum-*` fields as headers, for
/// `checksum::verify_body_checksum` (a form POST has no trailers).
fn form_checksum_fields(fields_ci: &HashMap<String, String>) -> HeaderMap {
    let mut out = HeaderMap::new();
    for algorithm in crate::api::checksum::ChecksumAlgorithm::ALL {
        let name = algorithm.header();
        let Some(value) = lookup_form_field(fields_ci, name) else {
            continue;
        };
        // A value that is not a header value cannot be base64 either.
        let value = axum::http::HeaderValue::from_str(value)
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("!"));
        out.insert(name, value);
    }
    out
}

fn lookup_form_field<'a>(fields_ci: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    fields_ci
        .get(&name.to_ascii_lowercase())
        .map(std::string::String::as_str)
}

/// Canned ACL values whose semantics match this proxy's default
/// (single-tenant, owner-only access). Accepting them lets clients that
/// auto-include `acl=private` (boto3 default, common SDK builders)
/// succeed without surprising rejections — same as we silently ignore
/// `x-amz-acl: private` on regular PUT object operations.
///
/// `bucket-owner-*` variants only matter in cross-account scenarios;
/// in a single-tenant proxy they're effectively no-ops.
fn is_compatible_canned_acl(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "private" | "bucket-owner-full-control" | "bucket-owner-read"
    )
}

fn ensure_supported_form_fields(fields_ci: &HashMap<String, String>) -> Result<(), S3Error> {
    if lookup_form_field(fields_ci, "x-amz-security-token").is_some() {
        return Err(S3Error::NotImplemented(
            "POST form uploads with x-amz-security-token are not supported".into(),
        ));
    }
    // ACL: accept canned values that match our default (private, bucket-owner-*).
    // Reject public-grant variants (public-read, public-read-write, authenticated-read)
    // because silently accepting them would be a security lie — the proxy doesn't
    // grant non-owner access via canned ACLs.
    if let Some(acl) = lookup_form_field(fields_ci, "acl") {
        if !is_compatible_canned_acl(acl) {
            return Err(S3Error::NotImplemented(format!(
                "POST form upload acl='{}' is not supported (only 'private' and 'bucket-owner-*' canned ACLs are accepted)",
                acl
            )));
        }
    }
    if lookup_form_field(fields_ci, "success_action_redirect").is_some() {
        return Err(S3Error::NotImplemented(
            "POST form upload success_action_redirect is not supported".into(),
        ));
    }
    if lookup_form_field(fields_ci, "success_action_status").is_some() {
        return Err(S3Error::NotImplemented(
            "POST form upload success_action_status override is not supported".into(),
        ));
    }
    Ok(())
}

/// The write preconditions of a form POST: its `If-Match` /
/// `If-None-Match` request headers, judged as on PutObject.
fn form_post_precondition(
    headers: &HeaderMap,
) -> Result<crate::deltaglider::Precondition, S3Error> {
    let parse = |name: axum::http::HeaderName| {
        headers
            .get(&name)
            .map(|v| {
                s3s::dto::ETagCondition::parse_http_header(v.as_bytes())
                    .map_err(|_| S3Error::InvalidArgument(format!("invalid {name} header")))
            })
            .transpose()
    };
    Ok(crate::deltaglider::Precondition {
        if_match: parse(axum::http::header::IF_MATCH)?,
        if_none_match: parse(axum::http::header::IF_NONE_MATCH)?,
    })
}

fn resolve_form_key(key_field: &str, filename: Option<&str>) -> Result<String, S3Error> {
    let mut out = key_field.to_string();
    if out.contains("${filename}") {
        let name = filename.ok_or_else(|| {
            S3Error::InvalidArgument(
                "Form key uses ${filename} but file part has no filename".into(),
            )
        })?;
        out = out.replace("${filename}", name);
    }
    if out.contains("${") {
        return Err(S3Error::NotImplemented(
            "Only ${filename} substitution is supported in form POST key".into(),
        ));
    }
    Ok(out.trim_start_matches('/').to_string())
}

/// Resolve a `$variable` reference inside a policy condition (e.g.
/// `["starts-with", "$key", "alice/"]`) to its concrete value.
///
/// **A-P1-1 known parity drift**: `$key` here evaluates to the
/// pre-`${filename}`-substitution `key_field` (the literal value of
/// the form's `key` field as the client submitted it), NOT the
/// post-substitution `resolved_key` that ultimately gets written to
/// storage. AWS S3's documented behaviour ("policy is evaluated
/// after `${filename}` substitution") suggests this should match
/// `resolved_key` instead.
///
/// In practice the bypass is bounded:
///   1. Path-traversal segments (`..`) in `${filename}` are caught
///      downstream by `engine.validated_key`.
///   2. The user's IAM `auth_user.can(Write, bucket, &resolved_key)`
///      check at the end of `authenticate_form_post` runs against
///      the post-substitution key. So a policy issuer's intent can
///      only "leak" within the requesting IAM user's own permitted
///      scope.
///
/// What WOULD be broken: a third-party policy issuer (external CI
/// signs a presigned policy for a less-trusted IAM user) whose
/// `["starts-with", "$key", "alice/"]` is intended to require an
/// exact one-segment file under `alice/`, but the signed form has
/// `key=alice/${filename}` and the browser substitutes `subdir/x`
/// → resolved_key = `alice/subdir/x`. Policy validates (because we
/// match against `key_field=alice/${filename}`, which starts with
/// `alice/`); IAM allows (because the user has `bucket/alice/*`).
/// Result: deeper nesting than the operator intended.
///
/// We keep the existing semantics here pending behavioural
/// verification against real AWS S3 — the docs are ambiguous, and
/// changing semantics blindly risks breaking real form-POST
/// integrations that match `["starts-with", "$key", "alice/${filename}"]`
/// as a literal string.
fn policy_lookup_variable(
    variable: &str,
    fields_ci: &HashMap<String, String>,
    bucket: &str,
    key_field: &str,
    resolved_content_type: &str,
) -> Result<String, S3Error> {
    match variable {
        "$bucket" => Ok(bucket.to_string()),
        // See A-P1-1 doc-comment above on the pre- vs post-substitution
        // ambiguity. `key_field` = pre-substitution literal. The IAM
        // check at the end of `authenticate_form_post` is the
        // belt-and-braces gate against scope escape.
        "$key" => Ok(key_field.to_string()),
        "$Content-Type" | "$content-type" => Ok(resolved_content_type.to_string()),
        _ => {
            let key = variable.trim_start_matches('$').to_ascii_lowercase();
            fields_ci.get(&key).cloned().ok_or_else(|| {
                S3Error::InvalidArgument(format!(
                    "Policy references form field {} that is missing",
                    variable
                ))
            })
        }
    }
}

/// The policy check that needs no file byte: expiration, every condition
/// against the fields before the file, the coverage of every field, and the
/// FORM of `content-length-range`, whose range it returns (the file is
/// checked against it while and after it arrives).
fn validate_form_post_policy(
    policy_json: &serde_json::Value,
    fields_ci: &HashMap<String, String>,
    bucket: &str,
    key_field: &str,
    resolved_content_type: &str,
) -> Result<Option<LengthRange>, S3Error> {
    let expiration = policy_json
        .get("expiration")
        .and_then(|v| v.as_str())
        .ok_or_else(|| S3Error::InvalidArgument("Policy is missing expiration".into()))?;
    let expires_at = chrono::DateTime::parse_from_rfc3339(expiration)
        .map_err(|_| S3Error::InvalidArgument("Policy expiration is not valid RFC3339".into()))?;
    if Utc::now() > expires_at.with_timezone(&Utc) {
        tracing::warn!(
            "form-POST DENY | reason=policy_expired | bucket={bucket} key={key_field} expired_at={expiration}"
        );
        return Err(S3Error::AccessDenied);
    }

    let conditions = policy_json
        .get("conditions")
        .and_then(|v| v.as_array())
        .ok_or_else(|| S3Error::InvalidArgument("Policy is missing conditions".into()))?;

    // AWS requires EVERY submitted form field to be covered by a policy
    // condition. We collect each field name a condition references, then reject
    // any submitted field not in that set (nor in the always-exempt set below).
    // Without this, a leaked signature over a narrow policy lets an attacker
    // smuggle an unapproved content-type or x-amz-meta-* into the stored object.
    let mut covered: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut length_range: Option<LengthRange> = None;

    for cond in conditions {
        if let Some(obj) = cond.as_object() {
            for (k, v) in obj {
                covered.insert(k.to_ascii_lowercase());
                let expected = v.as_str().ok_or_else(|| {
                    S3Error::InvalidArgument(format!("Policy condition '{}' must be a string", k))
                })?;
                match k.to_ascii_lowercase().as_str() {
                    "bucket" => {
                        if expected != bucket {
                            return Err(S3Error::AccessDenied);
                        }
                    }
                    "key" => {
                        if expected != key_field {
                            return Err(S3Error::AccessDenied);
                        }
                    }
                    "content-type" => {
                        if expected != resolved_content_type {
                            return Err(S3Error::AccessDenied);
                        }
                    }
                    "acl" => {
                        // Mirror ensure_supported_form_fields: accept canned ACLs that
                        // are compatible with our owner-only default. The policy
                        // condition still has to match the form field (already
                        // validated by the field check above), so the only thing
                        // we need to add here is "is the policy-promised value one
                        // we'd actually accept as a form field?"
                        if !is_compatible_canned_acl(expected) {
                            return Err(S3Error::NotImplemented(format!(
                                "POST form upload acl policy condition '{}' is not supported (only 'private' and 'bucket-owner-*' canned ACLs are accepted)",
                                expected
                            )));
                        }
                        // The form field's acl value (if present) must match the
                        // policy expectation — same exact-match semantics as
                        // bucket/key/content-type above.
                        let actual = lookup_form_field(fields_ci, "acl").unwrap_or_default();
                        if actual != expected {
                            return Err(S3Error::AccessDenied);
                        }
                    }
                    x if x.starts_with("x-amz-meta-")
                        || x == "x-amz-algorithm"
                        || x == "x-amz-credential"
                        || x == "x-amz-date"
                        || x == "x-amz-signature"
                        || x == "policy" =>
                    {
                        let actual = lookup_form_field(fields_ci, x).unwrap_or_default();
                        if actual != expected {
                            return Err(S3Error::AccessDenied);
                        }
                    }
                    other => {
                        return Err(S3Error::NotImplemented(format!(
                            "POST form upload policy condition '{}' is not supported",
                            other
                        )));
                    }
                }
            }
            continue;
        }

        let arr = cond.as_array().ok_or_else(|| {
            S3Error::InvalidArgument("Policy conditions entries must be object or array".into())
        })?;
        let op = arr.first().and_then(|v| v.as_str()).ok_or_else(|| {
            S3Error::InvalidArgument("Policy condition array must start with an operator".into())
        })?;
        match op {
            "content-length-range" => {
                if arr.len() != 3 {
                    return Err(S3Error::InvalidArgument(
                        "content-length-range must have exactly 3 elements".into(),
                    ));
                }
                let min = arr[1]
                    .as_u64()
                    .or_else(|| arr[1].as_i64().map(|n| n.max(0) as u64))
                    .ok_or_else(|| {
                        S3Error::InvalidArgument(
                            "content-length-range minimum must be a non-negative integer".into(),
                        )
                    })?;
                let max = arr[2]
                    .as_u64()
                    .or_else(|| arr[2].as_i64().map(|n| n.max(0) as u64))
                    .ok_or_else(|| {
                        S3Error::InvalidArgument(
                            "content-length-range maximum must be a non-negative integer".into(),
                        )
                    })?;
                let range = LengthRange { min, max };
                length_range = Some(length_range.map_or(range, |r| r.and(range)));
            }
            "starts-with" | "eq" => {
                if arr.len() != 3 {
                    return Err(S3Error::InvalidArgument(format!(
                        "{} condition must have exactly 3 elements",
                        op
                    )));
                }
                let variable = arr[1].as_str().ok_or_else(|| {
                    S3Error::InvalidArgument(format!("{} variable must be a string", op))
                })?;
                // `$key` / `${filename}` → field name "key"; a bare "$foo" → "foo".
                covered.insert(variable.trim_start_matches('$').to_ascii_lowercase());
                let expected = arr[2].as_str().ok_or_else(|| {
                    S3Error::InvalidArgument(format!("{} match value must be a string", op))
                })?;
                let actual = policy_lookup_variable(
                    variable,
                    fields_ci,
                    bucket,
                    key_field,
                    resolved_content_type,
                )?;
                let matched = if op == "eq" {
                    actual == expected
                } else {
                    actual.starts_with(expected)
                };
                if !matched {
                    tracing::warn!(
                        "form-POST DENY | reason=policy_condition_{op} | bucket={bucket} key={key_field} variable={variable} expected={expected} actual={actual}"
                    );
                    return Err(S3Error::AccessDenied);
                }
            }
            other => {
                return Err(S3Error::NotImplemented(format!(
                    "POST form upload policy operator '{}' is not supported",
                    other
                )));
            }
        }
    }

    // Every submitted form field must be covered by a condition (AWS rule) —
    // INCLUDING `key`: exempting it would let a leaked minimal-policy signature
    // (no key condition) write to ANY key in the signer's scope. AWS likewise
    // rejects an uncovered key ("Extra input fields: key").
    const EXEMPT: &[&str] = &[
        "policy",
        "x-amz-signature",
        "x-amz-credential",
        "x-amz-algorithm",
        "x-amz-date",
        "x-amz-security-token",
        "file",
    ];
    for field in fields_ci.keys() {
        let f = field.to_ascii_lowercase();
        if EXEMPT.contains(&f.as_str()) || f.starts_with("x-ignore-") || f == "submit" {
            continue;
        }
        if !covered.contains(&f) {
            tracing::warn!(
                "form-POST DENY | reason=uncovered_field | bucket={bucket} key={key_field} field={f}"
            );
            return Err(S3Error::AccessDenied);
        }
    }

    Ok(length_range)
}

/// Read the form up to its file part: the fields before it (each read
/// under [`FORM_FIELD_MAX`]), then the file part's headers, which give
/// `${filename}` and the file's content type. No byte of the file is read.
/// The checks that need only these fields run here, in the order of the
/// collected parser before: metadata size, `key`, the resolved key, the
/// stored header fields, then the file part itself.
async fn read_form_head(
    multipart: &mut multer::Multipart<'static>,
) -> Result<(FormHead, multer::Field<'static>), S3Error> {
    let mut fields_ci = HashMap::new();
    let mut user_metadata = HashMap::new();
    let mut budget = FORM_FIELDS_MAX;
    let mut count = 0;
    let mut file = None;
    while let Some(field) = multipart.next_field().await.map_err(form_error)? {
        let name = field
            .name()
            .ok_or_else(|| S3Error::InvalidRequest("Multipart field is missing name".into()))?
            .to_string();
        let lc_name = name.to_ascii_lowercase();
        if is_file_part(&lc_name, field.file_name()) {
            file = Some(field);
            break;
        }
        count_field(&mut count, &name, &mut budget)?;
        let value = read_small_field(field, &name, &mut budget).await?;
        if let Some(meta_key) = lc_name.strip_prefix("x-amz-meta-") {
            user_metadata.insert(meta_key.to_string(), value.clone());
        }
        fields_ci.insert(lc_name, value);
    }

    if let Err(size) = super::object_helpers::user_metadata_size_check(&user_metadata) {
        return Err(S3Error::MetadataTooLarge(
            super::object_helpers::user_metadata_too_large_message(size),
        ));
    }

    let key_field = lookup_form_field(&fields_ci, "key")
        .ok_or_else(|| {
            S3Error::InvalidArgument("POST form upload is missing required 'key' field".into())
        })?
        .to_string();
    let file_name = file.as_ref().and_then(|f| f.file_name());
    let resolved_key = resolve_form_key(&key_field, file_name)?;
    if resolved_key.is_empty() {
        return Err(S3Error::InvalidArgument(
            "POST form upload resolved object key is empty".into(),
        ));
    }

    let file_content_type = file
        .as_ref()
        .and_then(|f| f.content_type())
        .map(|ct| ct.to_string());
    let content_type = lookup_form_field(&fields_ci, "content-type")
        .map(str::to_string)
        .or(file_content_type);
    super::object_helpers::stored_header_fields_check(&user_metadata, content_type.as_deref())
        .map_err(S3Error::InvalidArgument)?;
    let file = file.ok_or_else(|| {
        S3Error::InvalidArgument("POST form upload requires a multipart file part".into())
    })?;

    Ok((
        FormHead {
            key_field,
            resolved_key,
            content_type,
            user_metadata,
            fields_ci,
        },
        file,
    ))
}

/// Read the fields after the file under the same small limits, and ignore
/// them, as S3 does (its sample form ends with a `submit` field). Reading
/// them checks that the form ends well.
async fn ignore_trailing_fields(multipart: &mut multer::Multipart<'static>) -> Result<(), S3Error> {
    let mut budget = FORM_FIELDS_MAX;
    let mut count = 0;
    while let Some(field) = multipart.next_field().await.map_err(form_error)? {
        let name = field.name().unwrap_or_default().to_string();
        count_field(&mut count, &name, &mut budget)?;
        read_small_field(field, &name, &mut budget).await?;
    }
    Ok(())
}

/// Read the file part once (`deltaglider::ingest`): into memory or a
/// spool file, capped at `cap`, with the checksum of the form's
/// `x-amz-checksum-*` field (only fields before the file count) computed
/// on the way and checked after the last byte. Any failure drops the body:
/// nothing is kept, and the RAM and spool budgets are whole again.
async fn ingest_form_file(
    engine: &crate::deltaglider::DynEngine,
    bucket: &str,
    key: &str,
    file: multer::Field<'static>,
    cap: FileCap,
    checksum_fields: &HeaderMap,
) -> Result<IngestedBody, S3Error> {
    let plan = IngestPlan {
        declared: cap.bytes,
        limit: engine.max_object_size(),
        sha256: Sha256Need::decide(Destination::Object, None, false),
        held_spool_mib: 0,
        // The cap is an upper bound: the file's length is known only
        // after it has arrived.
        strict_length: false,
        destination: Destination::Object,
    };
    crate::api::checksum::ingest_checked(
        engine,
        bucket,
        file.map_err(form_error),
        plan,
        checksum_fields,
        || None,
        |e| match e {
            IngestError::TooLong { .. } => over_cap_refusal(cap, bucket, key),
            e => crate::api::body_ingest::ingest_failure(e),
        },
    )
    .await
}

/// Everything about a form that is checked before its file is read: the
/// signature, the credential scope, the policy (expiration, conditions,
/// field coverage, the form of `content-length-range`) and IAM on the
/// resolved key. Without authentication (open access) nothing is checked
/// and the file has no policy range.
fn authenticate_form_post(
    coordination: &crate::coordination::CoordinationStatus,
    iam_state: Option<&SharedIamState>,
    bucket: &str,
    parsed: &FormHead,
    client_ip: Option<std::net::IpAddr>,
) -> Result<FormAuth, S3Error> {
    let open = FormAuth {
        user: None,
        length_range: None,
    };
    let Some(iam_state) = iam_state else {
        return Ok(open);
    };
    let snapshot = iam_state.load_full();
    if matches!(snapshot.as_ref(), IamState::Disabled) {
        return Ok(open);
    }
    // The users of an IAM database that no sync confirmed are not judged.
    if coordination.credential_view(&snapshot).is_err() {
        return Err(S3Error::ServiceUnavailable(
            "The credential store is not ready yet; retry later.".into(),
        ));
    }

    ensure_supported_form_fields(&parsed.fields_ci)?;

    let policy_b64 = lookup_form_field(&parsed.fields_ci, "policy")
        .ok_or(S3Error::AccessDenied)?
        .to_string();
    let credential = lookup_form_field(&parsed.fields_ci, "x-amz-credential")
        .ok_or(S3Error::AccessDenied)?
        .to_string();
    let algorithm = lookup_form_field(&parsed.fields_ci, "x-amz-algorithm")
        .ok_or(S3Error::AccessDenied)?
        .to_string();
    let signature = lookup_form_field(&parsed.fields_ci, "x-amz-signature")
        .ok_or(S3Error::AccessDenied)?
        .to_string();
    let amz_date = lookup_form_field(&parsed.fields_ci, "x-amz-date")
        .ok_or(S3Error::AccessDenied)?
        .to_string();
    if !algorithm.eq_ignore_ascii_case("AWS4-HMAC-SHA256") {
        return Err(S3Error::InvalidArgument(
            "Only x-amz-algorithm=AWS4-HMAC-SHA256 is supported for POST form uploads".into(),
        ));
    }

    let (access_key, scope_date, scope_region) = parse_policy_scope(&credential)?;
    if !amz_date.starts_with(scope_date) {
        tracing::warn!(
            "form-POST DENY | reason=scope_date_mismatch | bucket={bucket} key={} amz_date={amz_date} scope_date={scope_date}",
            parsed.resolved_key
        );
        return Err(S3Error::AccessDenied);
    }

    let (secret_access_key, auth_user) = match snapshot.as_ref() {
        IamState::Disabled => unreachable!("checked above"),
        IamState::Legacy(auth) => {
            // Constant-time, like the SigV4 path: a plain `!=` leaks how many
            // leading bytes of the bootstrap key a guess got right.
            if !crate::security::secret_eq(access_key.as_bytes(), auth.access_key_id.as_bytes()) {
                return Err(S3Error::AccessDenied);
            }
            (
                auth.secret_access_key.clone(),
                AuthenticatedUser::bootstrap_shared(&auth.access_key_id),
            )
        }
        IamState::Iam(index) => {
            let user = index
                .get(access_key)
                .filter(|u| u.enabled)
                .ok_or(S3Error::AccessDenied)?;
            let principal = index.principal(access_key).ok_or(S3Error::AccessDenied)?;
            (user.secret_access_key.clone(), principal.clone())
        }
    };

    // Validate the supplied signature's SHAPE before any timing-sensitive work.
    // The computed HMAC-SHA256 hex is always 64 lowercase hex chars; the
    // attacker-controlled `signature` is variable-length. Running
    // `to_ascii_lowercase()` (cost ∝ length) and `ct_eq` on a mismatched length
    // both leak the expected length via timing. Reject anything that isn't
    // exactly 64 hex chars up front — this is a cheap, length-independent gate
    // (one length check + a fixed-shape scan) that closes the oracle.
    if signature.len() != 64 || !signature.bytes().all(|b| b.is_ascii_hexdigit()) {
        tracing::warn!(
            "form-POST DENY | reason=signature_bad_shape | bucket={bucket} key={} access_key={access_key} sig_len={}",
            parsed.resolved_key,
            signature.len()
        );
        return Err(S3Error::SignatureDoesNotMatch);
    }
    let signing_key = derive_v4_signing_key(&secret_access_key, scope_date, scope_region);
    let computed_signature = hex::encode(hmac_sha256(&signing_key, policy_b64.as_bytes()));
    let sig_matches: bool = ConstantTimeEq::ct_eq(
        computed_signature.as_bytes(),
        signature.to_ascii_lowercase().as_bytes(),
    )
    .into();
    if !sig_matches {
        // The computed HMAC over the policy doesn't match the client's signature.
        // This is a SIGNING mismatch (wrong secret, wrong scope, or the client
        // signed a different policy than it sent) — NOT a replay. Log enough to
        // tell the two apart in prod (the prefixes only; never the full sig).
        tracing::warn!(
            "form-POST DENY | reason=signature_mismatch | bucket={bucket} key={} access_key={access_key} scope_date={scope_date} computed_prefix={} client_prefix={}",
            parsed.resolved_key,
            crate::security::str_prefix(&computed_signature, 8),
            crate::security::str_prefix(&signature, 8)
        );
        return Err(S3Error::SignatureDoesNotMatch);
    }

    let policy_bytes = base64::engine::general_purpose::STANDARD
        .decode(policy_b64.as_bytes())
        .map_err(|_| S3Error::InvalidArgument("Policy field is not valid base64".into()))?;
    let policy_json: serde_json::Value = serde_json::from_slice(&policy_bytes)
        .map_err(|_| S3Error::InvalidArgument("Policy JSON is malformed".into()))?;
    let resolved_ct = parsed
        .content_type
        .as_deref()
        .unwrap_or("application/octet-stream");
    let length_range = validate_form_post_policy(
        &policy_json,
        &parsed.fields_ci,
        bucket,
        &parsed.key_field,
        resolved_ct,
    )?;

    // Build a policy context with aws:SourceIp so IP/condition-scoped Deny rules
    // on the target prefix fire for browser uploads too — the context-free can()
    // silently ignored them (a SigV4 PUT through the middleware would honor them).
    let mut context = iam_rs::Context::new();
    crate::iam::permissions::insert_source_ip(&mut context, client_ip);
    if !auth_user.can_with_context(S3Action::Write, bucket, &parsed.resolved_key, &context) {
        tracing::warn!(
            "form-POST DENY | reason=iam_no_write | bucket={bucket} key={} user={}",
            parsed.resolved_key,
            auth_user.access_key_id
        );
        return Err(S3Error::AccessDenied);
    }
    Ok(FormAuth {
        user: Some(auth_user),
        length_range,
    })
}

/// Run the full presigned-form-POST pipeline over the streamed form
/// `body`: the length check, the form up to the file, authentication and
/// the policy, the client-write gates and the bucket-existence check, all
/// before a byte of the file is read; then the file's ingest, the
/// policy's minimum length, the trailing fields, and `store_client_write`
/// (quota, write lock, preconditions, store, event). Answers 204 No
/// Content with the stored object's ETag.
pub async fn handle_form_post_upload<S, E>(
    state: &Arc<AppState>,
    bucket: &str,
    iam_state: Option<&SharedIamState>,
    headers: &HeaderMap,
    extensions: &axum::http::Extensions,
    body: S,
    peer_ip: Option<std::net::IpAddr>,
) -> Result<Response, S3Error>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Into<Box<dyn std::error::Error + Send + Sync>> + 'static,
{
    // aws:SourceIp for the policy check: never a client-written XFF.
    let client_ip = crate::rate_limiter::extract_client_ip_with_peer(headers, peer_ip);
    // Validate the bucket name FIRST. The s3s adapter gets AWS name validation
    // for free; this interceptor bypasses s3s, so an unvalidated segment like
    // `..` would otherwise reach `bucket_dir = root.join("..")` and escape the
    // storage root. Also normalises the uppercase/underscore drift vs s3s.
    if let Err(e) = crate::security::validate_bucket_name(bucket) {
        tracing::warn!("form-POST DENY | reason=invalid_bucket_name | bucket={bucket} err={e:?}");
        return Err(S3Error::InvalidBucketName(bucket.to_string()));
    }
    // Read the cap from the (hot-reloadable) engine so a runtime
    // `max_object_size` change applies here too.
    let engine = state.engine.load_full();
    let limit = engine.max_object_size();
    let form_len = form_body_len(headers, limit)?;
    let content_type_header = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| S3Error::InvalidRequest("Missing Content-Type header".into()))?;
    let boundary = multer::parse_boundary(content_type_header)
        .map_err(|e| S3Error::InvalidRequest(format!("Invalid multipart boundary: {}", e)))?;
    let constraints = multer::Constraints::new()
        .size_limit(multer::SizeLimit::new().whole_stream(limit.saturating_add(FORM_OVERHEAD)));
    // The fields before the file, their names and part headers are read
    // under FORM_SIDE_MAX, counted on the stream: the parser buffers what it
    // polls until a part header ends.
    let meter = FormMeter::default();
    meter.bound();
    let mut multipart =
        multer::Multipart::with_constraints(meter.meter(body), boundary, constraints);

    // Authenticate BEFORE the existence check: a 404 to an unauthenticated
    // caller reveals which buckets exist, and the backend round-trip would
    // delay the health gate's fast 503 behind a backend timeout. And before
    // the file: an unauthenticated form costs its small fields only.
    let (head, file) = read_form_head(&mut multipart).await?;
    let auth =
        match authenticate_form_post(&state.coordination, iam_state, bucket, &head, client_ip) {
            Ok(auth) => auth,
            Err(e) => {
                // Mirror the IAM middleware's PUT-denial audit so operators can see
                // form-POST 403s in the admin audit ring (not just stdout warns) —
                // the surface that produced the original hard-to-diagnose prod 403.
                audit_log_s3(
                    "s3_post_denied",
                    "$form_post",
                    headers,
                    bucket,
                    &head.resolved_key,
                );
                return Err(e);
            }
        };
    // The client-write gate (a replication_target_only bucket, a read-only
    // encrypted bucket), after the signature check: its 403 names the
    // bucket's configuration and backend, which an unverified caller must
    // not learn (S23). Still before any backend I/O (403-before-404 parity
    // with the PUT path, commit b1b2249).
    check_client_write_allowed(state, bucket).await?;
    // Only now, with the policy signature verified: the gates' 503 names a
    // busy bucket or a backend's state.
    crate::maintenance::gate::check_verified_request(
        extensions,
        &axum::http::Method::POST,
        &format!("/{bucket}"),
    )?;
    ensure_bucket_exists(state, bucket).await?;
    let checksum_fields = form_checksum_fields(&head.fields_ci);
    crate::api::checksum::precheck_headers(&checksum_fields)?;
    let precondition = form_post_precondition(headers)?;

    // The file, read once: the same ingest as a PutObject body.
    let cap = form_file_cap(limit, form_len, auth.length_range.map(|r| r.max));
    // A form over the quota is refused before its file reaches the spool:
    // the cap is the most the file can be. The exact check runs again on
    // the received size, in `store_client_write`, unless this one was
    // optimistic (`exact_quota_check_after_body`).
    let early_quota = crate::api::handlers::object_helpers::check_quota_for_write(
        state,
        bucket,
        &head.resolved_key,
        cap.bytes,
    )
    .await?;
    meter.unbound();
    let body = ingest_form_file(
        &engine,
        bucket,
        &head.resolved_key,
        file,
        cap,
        &checksum_fields,
    )
    .await?;
    check_file_len(auth.length_range, body.size(), bucket, &head.resolved_key)?;
    meter.bound();
    ignore_trailing_fields(&mut multipart).await?;
    // The same client write as PutObject: quota, the object's write lock,
    // the preconditions, the store, the event.
    let result = store_client_write(
        state,
        ClientWrite {
            bucket,
            key: &head.resolved_key,
            body,
            content_type: head.content_type.clone(),
            user_metadata: head.user_metadata.clone(),
            precondition: &precondition,
            early_quota: Some(early_quota),
        },
    )
    .await?;
    let user_name = auth
        .user
        .as_ref()
        .map(|u| u.name.as_str())
        .unwrap_or("anonymous");
    audit_log_s3("s3_post", user_name, headers, bucket, &head.resolved_key);
    let mut response = (
        StatusCode::NO_CONTENT,
        [("ETag", result.metadata.etag())],
        "",
    )
        .into_response();
    let writer = crate::s3_adapter_s3s::Reader::for_form_writer(
        auth.user.as_deref(),
        bucket,
        &head.resolved_key,
    );
    for (name, value) in crate::s3_adapter_s3s::write_fact_headers(&result, writer) {
        response.headers_mut().insert(name, value);
    }
    Ok(response)
}

#[cfg(test)]
#[path = "form_post_tests.rs"]
mod stream_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// A form's checksum field is checked like the PUT header.
    #[test]
    fn form_checksum_fields_feed_the_checksum_check() {
        let check = |fields: &[(&str, &str)], body: &[u8]| {
            let fields_ci: HashMap<String, String> = fields
                .iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
                .collect();
            crate::api::checksum::verify_body_checksum(
                &form_checksum_fields(&fields_ci),
                None,
                body,
            )
            .map_err(|e| S3Error::from(e).code())
        };
        assert_eq!(check(&[("key", "k")], b"hello"), Ok(()));
        assert_eq!(
            check(&[("X-Amz-Checksum-CRC32", "NhCmhg==")], b"hello"),
            Ok(())
        );
        assert_eq!(
            check(&[("x-amz-checksum-crc32", "NhCmhg==")], b"hellO"),
            Err("BadDigest")
        );
        assert_eq!(
            check(&[("x-amz-checksum-crc32", "bad\nvalue")], b"hello"),
            Err("InvalidRequest")
        );
    }

    /// H18-adjacent: the form-POST write check must honor an IP-scoped Deny.
    /// Mirrors the exact decision the handler now makes (can_with_context with
    /// aws:SourceIp), proving a context-free can() would wrongly admit the write.
    #[test]
    fn form_post_write_check_honors_source_ip_scoped_deny() {
        use crate::iam::permissions::permission_to_iam_policy;
        use crate::iam::Permission;

        let bucket = "uploads-bkt";
        let key = "uploads/quarantine/payload.bin";
        let allow = Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["write".into()],
            resources: vec![format!("{bucket}/uploads/*")],
            conditions: None,
        };
        // Deny write to quarantine/* unless the request comes from 10.0.0.0/8.
        let deny = Permission {
            id: 0,
            effect: "Deny".into(),
            actions: vec!["write".into()],
            resources: vec![format!("{bucket}/uploads/quarantine/*")],
            conditions: Some(serde_json::json!({
                "NotIpAddress": { "aws:SourceIp": ["10.0.0.0/8"] }
            })),
        };
        let user = AuthenticatedUser::from_parts(
            "u".into(),
            "AK".into(),
            vec![],
            vec![
                permission_to_iam_policy(&allow),
                permission_to_iam_policy(&deny),
            ],
        );

        // Context-free (the OLD, buggy path): the conditioned Deny doesn't fire
        // because aws:SourceIp is absent → the Allow wins. This is the bypass.
        assert!(
            user.can(S3Action::Write, bucket, key),
            "context-free can() admits the write (the bug the fix closes)"
        );

        // External IP in context (the FIXED path): the Deny fires → blocked.
        let mut ctx_ext = iam_rs::Context::new();
        ctx_ext.insert(
            "aws:SourceIp".to_string(),
            iam_rs::ContextValue::String("1.2.3.4".to_string()),
        );
        assert!(
            !user.can_with_context(S3Action::Write, bucket, key, &ctx_ext),
            "an external IP must trip the IP-scoped Deny"
        );

        // Internal IP: the Deny's NotIpAddress condition is false → write allowed.
        let mut ctx_int = iam_rs::Context::new();
        ctx_int.insert(
            "aws:SourceIp".to_string(),
            iam_rs::ContextValue::String("10.1.2.3".to_string()),
        );
        assert!(
            user.can_with_context(S3Action::Write, bucket, key, &ctx_int),
            "an in-CIDR IP must be allowed"
        );
    }

    /// Pure-function truth table for the canned-ACL compatibility check.
    /// Boto3 default is `acl=private`; many SDK presigned-POST builders
    /// auto-include it. Accepting it (and the bucket-owner-* variants
    /// that are no-ops in single-tenant) keeps regular SDK usage working
    /// without surprise 501s. Rejecting public-grant variants prevents
    /// the proxy from silently lying about object visibility.
    #[test]
    fn canned_acl_compat_truth_table() {
        // Accepted — match owner-only default.
        for ok in [
            "private",
            "PRIVATE",
            "Private",
            " private ",
            "bucket-owner-full-control",
            "bucket-owner-read",
            "BUCKET-OWNER-FULL-CONTROL",
        ] {
            assert!(
                is_compatible_canned_acl(ok),
                "expected '{}' to be compatible",
                ok
            );
        }
        // Rejected — would imply non-owner access we can't grant.
        for bad in [
            "public-read",
            "public-read-write",
            "authenticated-read",
            "log-delivery-write",
            "aws-exec-read",
            "",
            "garbage",
        ] {
            assert!(
                !is_compatible_canned_acl(bad),
                "expected '{}' to be rejected",
                bad
            );
        }
    }

    #[test]
    fn ensure_supported_form_fields_accepts_private_acl() {
        let mut fields = HashMap::new();
        fields.insert("acl".to_string(), "private".to_string());
        ensure_supported_form_fields(&fields).expect("acl=private should be accepted");

        let mut fields = HashMap::new();
        fields.insert("acl".to_string(), "bucket-owner-full-control".to_string());
        ensure_supported_form_fields(&fields)
            .expect("acl=bucket-owner-full-control should be accepted");

        // No acl field at all — also fine.
        let fields = HashMap::new();
        ensure_supported_form_fields(&fields).expect("missing acl should be accepted");
    }

    #[test]
    fn ensure_supported_form_fields_rejects_public_acl() {
        let mut fields = HashMap::new();
        fields.insert("acl".to_string(), "public-read".to_string());
        let err =
            ensure_supported_form_fields(&fields).expect_err("acl=public-read must be rejected");
        match err {
            S3Error::NotImplemented(msg) => {
                assert!(
                    msg.contains("public-read"),
                    "expected msg to cite the value, got: {}",
                    msg
                );
            }
            other => panic!("expected NotImplemented, got {:?}", other),
        }
    }

    #[test]
    fn ensure_supported_form_fields_still_rejects_security_token() {
        let mut fields = HashMap::new();
        fields.insert("x-amz-security-token".to_string(), "tok".to_string());
        match ensure_supported_form_fields(&fields) {
            Err(S3Error::NotImplemented(_)) => {}
            other => panic!(
                "expected NotImplemented for security-token, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn ensure_supported_form_fields_still_rejects_success_action_redirect() {
        let mut fields = HashMap::new();
        fields.insert(
            "success_action_redirect".to_string(),
            "https://example.com/done".to_string(),
        );
        match ensure_supported_form_fields(&fields) {
            Err(S3Error::NotImplemented(_)) => {}
            other => panic!(
                "expected NotImplemented for success_action_redirect, got {:?}",
                other
            ),
        }
    }
}
