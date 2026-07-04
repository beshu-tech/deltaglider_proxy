// SPDX-License-Identifier: GPL-3.0-only

//! Browser presigned form-POST upload handling.
//!
//! S3 lets a browser upload directly to a bucket via an HTML form whose
//! action is `POST https://<host>/<bucket>` with `multipart/form-data`
//! body, where the SigV4 policy + signature live in form fields rather
//! than headers. This module owns the entire ingest pipeline for that
//! flow.
//!
//! Stages: `is_multipart_form_upload` sniffs the request so the
//! dispatcher in `object::delete_objects` can fork cleanly between
//! DeleteObjects XML and form upload; `parse_form_post_upload` consumes
//! the multipart body via `multer`, extracting form fields + the single
//! file part; `authenticate_form_post` rebuilds the SigV4 signing key,
//! verifies the form signature in constant time, decodes and validates
//! the policy document, and authorises the user against IAM;
//! `handle_form_post_upload` runs the bucket-existence + quota gates,
//! hands the file body to `engine.store`, and emits the object-created
//! event + audit log.
//!
//! Lives in its own module because none of this code path is shared
//! with the GET/PUT/HEAD/DELETE handlers in `object.rs` — the only
//! coupling is the dispatcher choosing between DeleteObjects and form
//! upload at the top of `delete_objects`.
//!
//! ## Replay-guard blast-radius trade-off (documented)
//!
//! `form_post_replay_record` is now an idempotency/observability ledger, not a
//! gate — it always allows. This MATCHES native AWS S3, which has no form-POST
//! replay protection: a presigned POST is valid for as many policy-conforming
//! uploads as fit within its expiration. The earlier guard capped a captured
//! signature at ONE object; removing that cap widens the blast radius of a
//! *leaked* signature to "any key matching the policy's `starts-with $key`, until
//! `expiration`." We accept this because (a) it was breaking the legitimate
//! AWS-intended batch pattern (the ROR CI uploads `.zip`/`.sha512`/`.sha1` under
//! one signature), and (b) the policy's own conditions — `starts-with $key`,
//! `content-length-range`, `expiration` — are re-validated on EVERY request
//! (`validate_form_post_policy`) and are the real bound. A future tightening
//! could re-impose the one-object cap ONLY for policies with an exact
//! `{"key": "..."}` condition (where reuse genuinely is an attack) while leaving
//! `starts-with` policies permissive; not done here.

use super::object_helpers::{check_client_write_allowed, check_quota, enqueue_object_event};
use super::{audit_log_s3, ensure_bucket_exists, AppState};
use crate::api::errors::S3Error;
use crate::event_outbox::{current_unix_seconds, EventKind, EventSource, NewEvent};
use crate::iam::{AuthenticatedUser, IamState, Permission, S3Action, SharedIamState};
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use chrono::Utc;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug)]
struct ParsedFormPost {
    key_field: String,
    resolved_key: String,
    content_type: Option<String>,
    user_metadata: HashMap<String, String>,
    file_data: Bytes,
    fields_ci: HashMap<String, String>,
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

fn validate_form_post_policy(
    policy_json: &serde_json::Value,
    fields_ci: &HashMap<String, String>,
    bucket: &str,
    key_field: &str,
    resolved_content_type: &str,
    file_len: u64,
) -> Result<(), S3Error> {
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
                if file_len < min || file_len > max {
                    tracing::warn!(
                        "form-POST DENY | reason=content_length_range | bucket={bucket} key={key_field} file_len={file_len} range=[{min},{max}]"
                    );
                    return Err(S3Error::AccessDenied);
                }
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

    // Every submitted form field must be covered by a condition (AWS rule).
    // Exempt: the signing/auth fields (the signature covers them intrinsically),
    // `file`/`key`/`content-type` (`file` is the payload; key/content-type are
    // handled by their own conditions or the resolved-key path), and the
    // browser-only `submit`/`x-ignore-*` helpers AWS also ignores.
    const EXEMPT: &[&str] = &[
        "policy",
        "x-amz-signature",
        "x-amz-credential",
        "x-amz-algorithm",
        "x-amz-date",
        "x-amz-security-token",
        "file",
        "key",
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

    Ok(())
}

async fn parse_form_post_upload(
    headers: &HeaderMap,
    body: Bytes,
) -> Result<ParsedFormPost, S3Error> {
    let content_type_header = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| S3Error::InvalidRequest("Missing Content-Type header".into()))?;
    let boundary = multer::parse_boundary(content_type_header)
        .map_err(|e| S3Error::InvalidRequest(format!("Invalid multipart boundary: {}", e)))?;
    let stream = futures::stream::once(async move { Ok::<Bytes, std::io::Error>(body) });
    let mut multipart = multer::Multipart::new(stream, boundary);

    let mut fields_ci = HashMap::new();
    let mut user_metadata = HashMap::new();
    let mut file_data: Option<Bytes> = None;
    let mut file_name: Option<String> = None;
    let mut file_content_type: Option<String> = None;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        S3Error::InvalidRequest(format!("Malformed multipart/form-data body: {}", e))
    })? {
        let name = field
            .name()
            .ok_or_else(|| S3Error::InvalidRequest("Multipart field is missing name".into()))?
            .to_string();
        let lc_name = name.to_ascii_lowercase();
        let part_filename = field.file_name().map(str::to_string);
        let part_content_type = field.content_type().map(|ct| ct.to_string());
        if part_filename.is_some() || lc_name == "file" {
            if file_data.is_some() {
                return Err(S3Error::InvalidRequest(
                    "POST form upload expects exactly one file part".into(),
                ));
            }
            let bytes = field.bytes().await.map_err(|e| {
                S3Error::InvalidRequest(format!("Failed reading multipart file part: {}", e))
            })?;
            file_data = Some(bytes);
            file_name = part_filename;
            file_content_type = part_content_type;
            continue;
        }
        let value = field.text().await.map_err(|e| {
            S3Error::InvalidRequest(format!(
                "Failed reading multipart form field '{}': {}",
                name, e
            ))
        })?;
        if let Some(meta_key) = lc_name.strip_prefix("x-amz-meta-") {
            user_metadata.insert(meta_key.to_string(), value.clone());
        }
        fields_ci.insert(lc_name, value);
    }

    let key_field = lookup_form_field(&fields_ci, "key")
        .ok_or_else(|| {
            S3Error::InvalidArgument("POST form upload is missing required 'key' field".into())
        })?
        .to_string();
    let resolved_key = resolve_form_key(&key_field, file_name.as_deref())?;
    if resolved_key.is_empty() {
        return Err(S3Error::InvalidArgument(
            "POST form upload resolved object key is empty".into(),
        ));
    }

    let content_type = lookup_form_field(&fields_ci, "content-type")
        .map(str::to_string)
        .or(file_content_type);
    let file_data = file_data.ok_or_else(|| {
        S3Error::InvalidArgument("POST form upload requires a multipart file part".into())
    })?;

    Ok(ParsedFormPost {
        key_field,
        resolved_key,
        content_type,
        user_metadata,
        file_data,
        fields_ci,
    })
}

fn authenticate_form_post(
    iam_state: Option<&SharedIamState>,
    bucket: &str,
    parsed: &ParsedFormPost,
) -> Result<Option<AuthenticatedUser>, S3Error> {
    let Some(iam_state) = iam_state else {
        return Ok(None);
    };
    let snapshot = iam_state.load_full();
    if matches!(snapshot.as_ref(), IamState::Disabled) {
        return Ok(None);
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
            if auth.access_key_id != access_key {
                return Err(S3Error::AccessDenied);
            }
            let bootstrap_perms = vec![Permission {
                id: 0,
                effect: "Allow".to_string(),
                actions: vec!["*".to_string()],
                resources: vec!["*".to_string()],
                conditions: None,
            }];
            let bootstrap_policies: Vec<iam_rs::IAMPolicy> = bootstrap_perms
                .iter()
                .map(crate::iam::permissions::permission_to_iam_policy)
                .collect();
            (
                auth.secret_access_key.clone(),
                AuthenticatedUser {
                    name: "$bootstrap".to_string(),
                    access_key_id: auth.access_key_id.clone(),
                    permissions: bootstrap_perms,
                    iam_policies: bootstrap_policies,
                },
            )
        }
        IamState::Iam(index) => {
            let user = index
                .get(access_key)
                .filter(|u| u.enabled)
                .ok_or(S3Error::AccessDenied)?;
            (
                user.secret_access_key.clone(),
                AuthenticatedUser {
                    name: user.name.clone(),
                    access_key_id: user.access_key_id.clone(),
                    permissions: user.permissions.clone(),
                    iam_policies: user.iam_policies.clone(),
                },
            )
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
            &computed_signature[..computed_signature.len().min(8)],
            &signature[..signature.len().min(8)]
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
    validate_form_post_policy(
        &policy_json,
        &parsed.fields_ci,
        bucket,
        &parsed.key_field,
        resolved_ct,
        parsed.file_data.len() as u64,
    )?;

    if !auth_user.can(S3Action::Write, bucket, &parsed.resolved_key) {
        tracing::warn!(
            "form-POST DENY | reason=iam_no_write | bucket={bucket} key={} user={}",
            parsed.resolved_key,
            auth_user.access_key_id
        );
        return Err(S3Error::AccessDenied);
    }
    Ok(Some(auth_user))
}

/// Replay-cache TTL for a form-POST signature: the policy's own
/// expiration capped at [`MAX_FORM_POST_REPLAY_TTL_SECS`] so an
/// attacker uploading a multi-day-expiry policy can't pin a cache
/// slot indefinitely. Pure: no I/O, no global state.
fn form_post_replay_ttl(
    policy_b64: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> std::time::Duration {
    use std::time::Duration;
    // Decode → parse JSON → extract `expiration`. Any failure short-
    // circuits to the floor TTL (sig is still cached, just for a
    // short window).
    let parsed = base64::engine::general_purpose::STANDARD
        .decode(policy_b64.as_bytes())
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|j| {
            j.get("expiration")
                .and_then(|e| e.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc))
        });
    let max = Duration::from_secs(MAX_FORM_POST_REPLAY_TTL_SECS);
    let floor = Duration::from_secs(5);
    let raw = match parsed {
        Some(exp) if exp > now => exp.signed_duration_since(now).to_std().unwrap_or(floor),
        _ => floor,
    };
    raw.min(max).max(floor)
}

/// Cap on the form-POST replay-cache TTL — no single entry can pin a
/// slot for more than 24 h regardless of the policy's claimed expiry.
const MAX_FORM_POST_REPLAY_TTL_SECS: u64 = 24 * 60 * 60;

/// A live entry in the form-POST idempotency ledger.
///
/// `expiry` is the policy's expiration `Instant` (capped at 24 h), used only for
/// TTL eviction. The entry is keyed by `(signature, fingerprint)` in the cache
/// (see [`form_post_replay_cache_key`]), so distinct files under one signature
/// are distinct entries and a resend of the same file just refreshes its expiry.
#[derive(Clone, Copy, Debug)]
pub struct ReplayEntry {
    pub expiry: std::time::Instant,
    // The file fingerprint is encoded in the cache KEY (`{sig}:{fp}`), not stored
    // here — the entry only needs its expiry for TTL eviction.
}

/// Stable fingerprint of the (resolved key, body) a form-POST writes.
/// Sha256 over `key`, a domain separator, and the body, folded to a
/// `u64` — collision-resistant enough to tell "same object re-sent" from
/// "signature reused for different content" without storing the full
/// body. Pure.
fn form_post_fingerprint(resolved_key: &str, body: &[u8]) -> u64 {
    use sha2::Digest;
    let mut hasher = Sha256::new();
    hasher.update((resolved_key.len() as u64).to_le_bytes());
    hasher.update(resolved_key.as_bytes());
    hasher.update([0u8]); // domain separator between key and body
    hasher.update(body);
    let digest = hasher.finalize();
    u64::from_le_bytes(digest[..8].try_into().expect("sha256 >= 8 bytes"))
}

/// Cap on the replay-cache total entries. A flood of unique-signature
/// form-POSTs (an attacker minting fresh policies) must not be able
/// to OOM the proxy.
const MAX_FORM_POST_REPLAY_ENTRIES: usize = 50_000;

/// Fraction of the hard cap to shed in one eviction sweep once the cap
/// is breached, so a sustained flood can't pin the cache at the ceiling
/// by re-filling a single freed slot per insert. 10% gives the next
/// ~5,000 legitimate signatures breathing room before the next sweep.
const FORM_POST_REPLAY_EVICT_FRACTION: usize = 10;

/// Pick the keys to evict when the replay cache is over its hard cap:
/// the `evict_count` entries with the **soonest** expiry (closest to
/// being pruned anyway), so eviction prefers the least-valuable slots
/// and never touches a live entry before an about-to-expire one. Pure:
/// operates on a borrowed snapshot of `(key, expiry)` pairs, no I/O.
fn form_post_replay_evict_keys(
    entries: &[(String, std::time::Instant)],
    evict_count: usize,
) -> Vec<String> {
    // Operates on (signature, expiry) pairs — the caller projects each
    // `ReplayEntry` down to its `expiry` before calling, so the eviction
    // policy stays purely a function of remaining TTL.
    if evict_count == 0 || entries.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<&(String, std::time::Instant)> = entries.iter().collect();
    sorted.sort_by_key(|(_, exp)| *exp);
    sorted
        .into_iter()
        .take(evict_count.min(entries.len()))
        .map(|(k, _)| k.clone())
        .collect()
}

/// Record a form-POST in the idempotency/observability ledger and enforce the
/// cache's bounded-size invariant. This does NOT reject replays — AWS presigned
/// POSTs are reusable until the policy expires; the ledger is keyed on
/// `(signature, key+body fingerprint)` so a true replay just refreshes its entry
/// (see [`form_post_replay_record`]). Its security job is the hard-cap eviction
/// that stops a unique-signature flood from OOMing the cache.
fn enforce_form_post_replay(state: &Arc<AppState>, parsed: &ParsedFormPost) -> Result<(), S3Error> {
    let cache = &state.form_post_replay;
    let now = chrono::Utc::now();
    let now_instant = std::time::Instant::now();

    // Bounded prune: drop expired entries and, if still above cap,
    // evict a BATCH of the soonest-to-expire entries. Evicting only one
    // entry per insert lets a sustained unique-signature flood hold the
    // cache pinned at the ceiling indefinitely (every freed slot is
    // immediately refilled by the next insert), so we shed
    // `FORM_POST_REPLAY_EVICT_FRACTION`% of the cap in one sweep to
    // create real headroom. Cheaper than a background sweeper task and
    // bounded by `MAX_FORM_POST_REPLAY_ENTRIES`.
    cache.retain(|_, entry| entry.expiry > now_instant);
    if cache.len() > MAX_FORM_POST_REPLAY_ENTRIES {
        let evict_count = (MAX_FORM_POST_REPLAY_ENTRIES / FORM_POST_REPLAY_EVICT_FRACTION).max(1);
        // DashMap has no batch `pop`; snapshot (key, expiry) pairs,
        // pick the soonest-to-expire keys via a pure helper, then
        // remove them.
        let snapshot: Vec<(String, std::time::Instant)> = cache
            .iter()
            .map(|e| (e.key().clone(), e.value().expiry))
            .collect();
        let evicted = form_post_replay_evict_keys(&snapshot, evict_count);
        for k in &evicted {
            cache.remove(k);
        }
        tracing::warn!(
            "SECURITY | form_post_replay_cache hard-cap reached — evicted {} of {} entries (possible flood)",
            evicted.len(),
            snapshot.len()
        );
    }

    let Some(sig) = lookup_form_field(&parsed.fields_ci, "x-amz-signature") else {
        // Unsigned form-POST (authentication=none mode) skips the
        // replay check — there's nothing to replay against. The
        // un-auth path is gated elsewhere.
        return Ok(());
    };
    let Some(policy_b64) = lookup_form_field(&parsed.fields_ci, "policy") else {
        return Ok(());
    };

    let key = sig.to_ascii_lowercase();
    let ttl = form_post_replay_ttl(policy_b64, now);
    let new_expiry = now_instant + ttl;
    let fingerprint = form_post_fingerprint(&parsed.resolved_key, &parsed.file_data);
    form_post_replay_record(cache, &key, fingerprint, new_expiry);
    Ok(())
}

/// Cache key for the replay guard: the request signature COMBINED with the
/// `(key, body)` fingerprint. Keying on the pair is what distinguishes a true
/// replay from a legitimate batch upload — see [`form_post_replay_record`].
fn form_post_replay_cache_key(sig: &str, fingerprint: u64) -> String {
    format!("{sig}:{fingerprint:016x}")
}

/// Record a form-POST attempt in the idempotency/observability ledger. There is
/// no reject path: the ledger is keyed on `(signature, fingerprint)`, so a true
/// replay (same sig + same body) just refreshes its entry, and a different file
/// under the same signature (the AWS-intended `starts-with $key` batch pattern —
/// the ROR CI uploads .zip/.sha512/.sha1 under one signature) is a distinct
/// entry, NOT a rejection.
///
/// History: the guard used to key ONLY on the signature and 403 any reuse with a
/// DIFFERENT `(key, body)` fingerprint, on the theory that a captured signature
/// rewriting a different object is an attack. But two files of the SAME SIZE
/// (every `.sha1` is 41 bytes, every `.sha512` is 129) signed in the same second
/// get a byte-identical signature, so that "different fingerprint" was the
/// LEGITIMATE batch case — and it 403'd intermittently. The policy's own
/// conditions (`starts-with $key`, `content-length-range`, `expiration`) are
/// re-validated on every request and are the real bound on a captured signature;
/// the replay guard couldn't add to that without breaking the batch pattern.
fn form_post_replay_record(
    cache: &dashmap::DashMap<String, ReplayEntry>,
    key: &str,
    fingerprint: u64,
    new_expiry: std::time::Instant,
) {
    let cache_key = form_post_replay_cache_key(key, fingerprint);
    cache
        .entry(cache_key)
        .and_modify(|existing| existing.expiry = new_expiry)
        .or_insert(ReplayEntry { expiry: new_expiry });
}

/// Run the full presigned-form-POST pipeline.
///
/// Called from `object::delete_objects` when the dispatcher detects a
/// `multipart/form-data` body via [`is_multipart_form_upload`]. Performs
/// bucket-existence + parse + auth + quota checks, hands the file body
/// to `engine.store`, emits the object-created event, and returns a
/// 204 No Content with the persisted object's ETag.
pub async fn handle_form_post_upload(
    state: &Arc<AppState>,
    bucket: &str,
    iam_state: Option<&SharedIamState>,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, S3Error> {
    // Validate the bucket name FIRST. The s3s adapter gets AWS name validation
    // for free; this interceptor bypasses s3s, so an unvalidated segment like
    // `..` would otherwise reach `bucket_dir = root.join("..")` and escape the
    // storage root. Also normalises the uppercase/underscore drift vs s3s.
    if let Err(e) = crate::security::validate_bucket_name(bucket) {
        tracing::warn!("form-POST DENY | reason=invalid_bucket_name | bucket={bucket} err={e:?}");
        return Err(S3Error::InvalidBucketName(bucket.to_string()));
    }
    // Refuse a replication_target_only bucket before any backend I/O or body
    // parse (403-before-404 parity with the PUT path, commit b1b2249).
    check_client_write_allowed(state, bucket)?;
    ensure_bucket_exists(state, bucket).await?;
    let parsed = parse_form_post_upload(headers, body).await?;
    let auth_user = match authenticate_form_post(iam_state, bucket, &parsed) {
        Ok(u) => u,
        Err(e) => {
            // Mirror the IAM middleware's PUT-denial audit so operators can see
            // form-POST 403s in the admin audit ring (not just stdout warns) —
            // the surface that produced the original hard-to-diagnose prod 403.
            audit_log_s3(
                "s3_post_denied",
                "$form_post",
                headers,
                bucket,
                &parsed.resolved_key,
            );
            return Err(e);
        }
    };
    // Form-POST replay is an observability ledger, NOT a rejection: AWS presigned
    // POSTs are intentionally reusable until the policy expires, so we record the
    // (signature, key+body-fingerprint) rather than block re-sends. See the ledger
    // doc on `form_post_replay_record`.
    enforce_form_post_replay(state, &parsed)?;
    check_quota(state, bucket, parsed.file_data.len() as u64)?;
    let engine = state.engine.load();
    let size = parsed.file_data.len() as u64;
    // Large delta-eligible POST uploads: route through the streaming spool store
    // (Phase 4) so the delta encode runs with bounded memory — same path the s3s
    // PUT uses. (Like PUT, the body is already collected here for parsing; full
    // streaming intake is Phase 4.1.)
    let result = if size > engine.spool_store_threshold()
        && engine.is_delta_eligible_key(&parsed.resolved_key)
    {
        let spool = engine.spool_acquire(size).await?;
        tokio::fs::write(spool.path(), &parsed.file_data)
            .await
            .map_err(|e| {
                crate::deltaglider::EngineError::Storage(crate::storage::StorageError::from(e))
            })?;
        engine
            .store_spooled_delta(
                bucket,
                &parsed.resolved_key,
                &spool,
                size,
                parsed.content_type.clone(),
                parsed.user_metadata.clone(),
                None,
            )
            .await?
    } else {
        engine
            .store(
                bucket,
                &parsed.resolved_key,
                &parsed.file_data,
                parsed.content_type.clone(),
                parsed.user_metadata.clone(),
            )
            .await?
    };
    let storage_type = result.metadata.storage_info.label();
    enqueue_object_event(
        state,
        NewEvent::new(
            EventKind::ObjectCreated,
            bucket,
            &parsed.resolved_key,
            EventSource::S3Api,
            current_unix_seconds(),
            serde_json::json!({
                "content_length": parsed.file_data.len(),
                "storage_type": storage_type,
                "etag": result.metadata.etag(),
            }),
        ),
    )
    .await;
    let user_name = auth_user
        .as_ref()
        .map(|u| u.name.as_str())
        .unwrap_or("anonymous");
    audit_log_s3("s3_post", user_name, headers, bucket, &parsed.resolved_key);
    Ok((
        StatusCode::NO_CONTENT,
        [
            ("ETag", result.metadata.etag()),
            ("x-amz-storage-type", storage_type.to_string()),
        ],
        "",
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// TTL is the policy's own remaining expiry, capped at 24h with
    /// a 5-second floor. Pure function; no test infrastructure
    /// needed.
    #[test]
    fn form_post_replay_ttl_follows_policy_expiry() {
        use chrono::{Duration as Cd, Utc};
        use std::time::Duration;

        // Policy expiring in 30 min → TTL ≈ 30 min.
        let now = Utc::now();
        let policy = serde_json::json!({
            "expiration": (now + Cd::minutes(30)).to_rfc3339(),
        });
        let policy_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            serde_json::to_vec(&policy).unwrap(),
        );
        let ttl = form_post_replay_ttl(&policy_b64, now);
        assert!(ttl > Duration::from_secs(60 * 25));
        assert!(ttl < Duration::from_secs(60 * 35));

        // Policy expiring in 10 days → capped at 24 h.
        let policy = serde_json::json!({
            "expiration": (now + Cd::days(10)).to_rfc3339(),
        });
        let policy_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            serde_json::to_vec(&policy).unwrap(),
        );
        let ttl = form_post_replay_ttl(&policy_b64, now);
        assert_eq!(ttl, Duration::from_secs(MAX_FORM_POST_REPLAY_TTL_SECS));

        // Already-expired policy → floor (5s) — caller will still
        // reject on the expiration check; this is the safe fallback.
        let policy = serde_json::json!({
            "expiration": (now - Cd::minutes(1)).to_rfc3339(),
        });
        let policy_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            serde_json::to_vec(&policy).unwrap(),
        );
        assert_eq!(
            form_post_replay_ttl(&policy_b64, now),
            Duration::from_secs(5)
        );

        // Garbage policy → floor.
        assert_eq!(
            form_post_replay_ttl("not-base64-at-all", now),
            Duration::from_secs(5)
        );
        // Valid base64 but not JSON → floor.
        let bogus = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"not json");
        assert_eq!(form_post_replay_ttl(&bogus, now), Duration::from_secs(5));
    }

    /// Adversarial: a second insertion for the same signature within
    /// the TTL window must observe the first entry as live so the
    /// caller (`enforce_form_post_replay`) rejects the replay. Tests
    /// the cache-shape contract directly — the full route-layer flow
    /// is covered by integration tests.
    #[test]
    fn form_post_replay_cache_marks_signature_as_seen() {
        let cache: std::sync::Arc<dashmap::DashMap<String, ReplayEntry>> =
            std::sync::Arc::new(dashmap::DashMap::new());
        let key = "deadbeef".to_string();
        let now = std::time::Instant::now();
        // Insert with 10-min TTL.
        cache.insert(
            key.clone(),
            ReplayEntry {
                expiry: now + std::time::Duration::from_secs(600),
            },
        );
        // A freshly-inserted entry within its TTL must read as live (the
        // expiry-window mechanic the replay ledger relies on).
        let live = cache
            .get(&key)
            .map(|v| v.expiry > std::time::Instant::now())
            .unwrap_or(false);
        assert!(live, "a within-TTL entry must report as still live");
    }

    /// The fix's core invariant: ONE presigned signature may upload a BATCH of
    /// distinct files (the AWS-intended `starts-with $key` pattern, used by the
    /// ROR CI for `.zip`/`.sha512`/`.sha1`). Previously the guard 403'd the 2nd+
    /// distinct file under a live signature; now every (sig, body) pair gets its
    /// own ledger entry, and an exact resend stays idempotent.
    #[test]
    fn form_post_replay_allows_batch_under_one_signature() {
        use std::time::{Duration, Instant};
        let cache: dashmap::DashMap<String, ReplayEntry> = dashmap::DashMap::new();
        let sig = "deadbeefcafef00d";
        let now = Instant::now();
        let exp = now + Duration::from_secs(3600);

        // Two SAME-SIZE files (e.g. two .sha1, both 41 bytes) under one signing
        // second get a byte-identical signature `sig` — the exact 403 trigger.
        // Different bodies → different fingerprints → distinct ledger keys.
        let fp_a = form_post_fingerprint("ror/builds/1.71/a.zip.sha1", b"aaaaaaaa\n");
        let fp_b = form_post_fingerprint("ror/builds/1.71/b.zip.sha1", b"bbbbbbbb\n");
        let fp_c = form_post_fingerprint("ror/builds/1.71/x.zip.sha512", b"sha512hash\n");

        // A batch of 3 distinct files under ONE signature — the ledger records
        // each without rejecting (the old guard 403'd the 2nd/3rd).
        form_post_replay_record(&cache, sig, fp_a, exp);
        form_post_replay_record(&cache, sig, fp_b, exp);
        form_post_replay_record(&cache, sig, fp_c, exp);
        // Exact resend (CI retry) refreshes its entry, does not add one.
        form_post_replay_record(&cache, sig, fp_c, exp);

        // Each distinct (sig, body) is its own entry — three files = three keys
        // (NOT one key with rejections). This is the property that fixes the 403.
        assert_eq!(cache.len(), 3, "one ledger entry per distinct (sig, body)");
        assert!(
            cache.contains_key(&form_post_replay_cache_key(sig, fp_a))
                && cache.contains_key(&form_post_replay_cache_key(sig, fp_b))
                && cache.contains_key(&form_post_replay_cache_key(sig, fp_c)),
            "same-signature, different-body files must each get a distinct key"
        );
    }

    /// Fingerprint distinguishes (key, body) tuples and is stable for
    /// identical input — and changing EITHER the key or the body changes
    /// it, so a signature can't be repointed at a different key with the
    /// same body (or the same key with different body).
    #[test]
    fn form_post_fingerprint_is_stable_and_discriminating() {
        let base = form_post_fingerprint("k/one", b"body");
        assert_eq!(base, form_post_fingerprint("k/one", b"body"), "stable");
        assert_ne!(base, form_post_fingerprint("k/two", b"body"), "key matters");
        assert_ne!(
            base,
            form_post_fingerprint("k/one", b"other"),
            "body matters"
        );
        // Domain separation: ("ab","c") must not collide with ("a","bc").
        assert_ne!(
            form_post_fingerprint("ab", b"c"),
            form_post_fingerprint("a", b"bc"),
            "key/body boundary must be unambiguous"
        );
    }

    /// Adversarial: a signature whose TTL has elapsed must be
    /// purged on access so the slot can be reused — but a legitimate
    /// retry doesn't "leak" indefinitely.
    #[test]
    fn form_post_replay_cache_expired_entries_are_drained() {
        let cache: std::sync::Arc<dashmap::DashMap<String, ReplayEntry>> =
            std::sync::Arc::new(dashmap::DashMap::new());
        let key = "deadbeef".to_string();
        let now = std::time::Instant::now();
        // Insert with a NEGATIVE TTL (already expired).
        cache.insert(
            key.clone(),
            ReplayEntry {
                expiry: now
                    .checked_sub(std::time::Duration::from_secs(60))
                    .unwrap_or(now),
            },
        );
        // Prune: same `retain` shape the enforcer uses.
        cache.retain(|_, entry| entry.expiry > std::time::Instant::now());
        assert!(
            !cache.contains_key(&key),
            "expired entry must be evicted by the retain sweep"
        );
    }

    /// Over-cap eviction sheds a BATCH of the soonest-to-expire
    /// entries (not one), and never picks a longer-lived slot before a
    /// shorter-lived one. Guards the OOM-under-flood regression where a
    /// single-eviction-per-insert strategy let the cache stay pinned at
    /// the ceiling.
    #[test]
    fn form_post_replay_evict_picks_soonest_to_expire_batch() {
        use std::time::{Duration, Instant};
        let base = Instant::now();
        // 10 entries with strictly increasing expiry: "k0" expires
        // first, "k9" last.
        let entries: Vec<(String, Instant)> = (0..10)
            .map(|i| (format!("k{i}"), base + Duration::from_secs(i + 1)))
            .collect();

        // Evict 3 → the three soonest-to-expire keys.
        let mut evicted = form_post_replay_evict_keys(&entries, 3);
        evicted.sort();
        assert_eq!(evicted, vec!["k0", "k1", "k2"]);

        // evict_count larger than the cache size is clamped, not OOB.
        assert_eq!(form_post_replay_evict_keys(&entries, 100).len(), 10);

        // Degenerate inputs return nothing.
        assert!(form_post_replay_evict_keys(&entries, 0).is_empty());
        assert!(form_post_replay_evict_keys(&[], 5).is_empty());
    }
}
