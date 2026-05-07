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

use super::object_helpers::{check_quota, enqueue_object_event};
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
pub(super) fn is_multipart_form_upload(headers: &HeaderMap) -> bool {
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

fn ensure_supported_form_fields(fields_ci: &HashMap<String, String>) -> Result<(), S3Error> {
    if lookup_form_field(fields_ci, "x-amz-security-token").is_some() {
        return Err(S3Error::NotImplemented(
            "POST form uploads with x-amz-security-token are not supported".into(),
        ));
    }
    if lookup_form_field(fields_ci, "acl").is_some() {
        return Err(S3Error::NotImplemented(
            "POST form upload ACL overrides are not supported".into(),
        ));
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

fn policy_lookup_variable(
    variable: &str,
    fields_ci: &HashMap<String, String>,
    bucket: &str,
    key_field: &str,
    resolved_content_type: &str,
) -> Result<String, S3Error> {
    match variable {
        "$bucket" => Ok(bucket.to_string()),
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
        return Err(S3Error::AccessDenied);
    }

    let conditions = policy_json
        .get("conditions")
        .and_then(|v| v.as_array())
        .ok_or_else(|| S3Error::InvalidArgument("Policy is missing conditions".into()))?;

    for cond in conditions {
        if let Some(obj) = cond.as_object() {
            for (k, v) in obj {
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
                        return Err(S3Error::NotImplemented(
                            "POST form upload ACL policy conditions are not supported".into(),
                        ));
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

    let signing_key = derive_v4_signing_key(&secret_access_key, scope_date, scope_region);
    let computed_signature = hex::encode(hmac_sha256(&signing_key, policy_b64.as_bytes()));
    let sig_matches: bool = ConstantTimeEq::ct_eq(
        computed_signature.as_bytes(),
        signature.to_ascii_lowercase().as_bytes(),
    )
    .into();
    if !sig_matches {
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
        return Err(S3Error::AccessDenied);
    }
    Ok(Some(auth_user))
}

/// Run the full presigned-form-POST pipeline.
///
/// Called from `object::delete_objects` when the dispatcher detects a
/// `multipart/form-data` body via [`is_multipart_form_upload`]. Performs
/// bucket-existence + parse + auth + quota checks, hands the file body
/// to `engine.store`, emits the object-created event, and returns a
/// 204 No Content with the persisted object's ETag.
pub(super) async fn handle_form_post_upload(
    state: &Arc<AppState>,
    bucket: &str,
    iam_state: Option<&SharedIamState>,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, S3Error> {
    ensure_bucket_exists(state, bucket).await?;
    let parsed = parse_form_post_upload(headers, body).await?;
    let auth_user = authenticate_form_post(iam_state, bucket, &parsed)?;
    check_quota(state, bucket, parsed.file_data.len() as u64)?;
    let result = state
        .engine
        .load()
        .store(
            bucket,
            &parsed.resolved_key,
            &parsed.file_data,
            parsed.content_type.clone(),
            parsed.user_metadata.clone(),
        )
        .await?;
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
