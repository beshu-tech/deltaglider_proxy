// SPDX-License-Identifier: BUSL-1.1

//! Differential contract: our pre-s3s layers against s3s's own parse.
//!
//! Two parsers read every S3 request. Admission, the SigV4 identity
//! resolver and the IAM authorization middleware decide on OUR parse of
//! the raw request; s3s then parses the same bytes again, verifies the
//! signature and picks the operation. Each past bypass (S1, S2, S4, C1) was
//! a request on which the two parsers disagreed: the policy checked one
//! principal or resource, s3s served another.
//!
//! This module drives raw requests through the production S3 router
//! (`api::s3_router::build_s3_router_with`: every layer, both
//! interceptors, the production s3s auth and config) with a recording
//! wrapper around the production access hook and a no-op `S3` impl. For every
//! request that reaches the access hook it asserts that both sides agree on
//! the identity, the bucket, the key, the list prefix and the action. A
//! request that either side refuses before that point is fine: it is never
//! served.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use axum::body::Body;
use axum::http::{HeaderValue, Method, Request, StatusCode};
use axum::Router;
use proptest::prelude::*;
use s3s::access::{S3Access, S3AccessContext};
use s3s::dto::{ListObjectsInput, ListObjectsOutput, ListObjectsV2Input, ListObjectsV2Output};
use s3s::{S3Request, S3Response, S3Result};
use tower::ServiceExt;

use crate::api::request_target::RequestTarget;
use crate::api::s3s_hooks::{DeltaGliderS3sAuth, VerifiedIdentityS3sAccess};
use crate::iam::{AuthenticatedUser, IamIndex, IamState, IamUser, Permission, S3Action};

const HOST: &str = "localhost:9000";
const PUBLIC_BUCKET: &str = "pub-bucket";
const PUBLIC_PREFIX: &str = "open/";

#[derive(Clone, Copy, Debug)]
struct User {
    ak: &'static str,
    sk: &'static str,
}

const ALICE: User = User {
    ak: "AKALICE0000000000001",
    sk: "alice-secret-0000000000000000000000001",
};
const BOB: User = User {
    ak: "AKBOB00000000000002",
    sk: "bob-secret-00000000000000000000000002",
};

// ── what each side saw ──────────────────────────────────────────────────

/// s3s's parsed path (`s3s::path::S3Path` is not `Clone`).
#[derive(Debug, Clone, PartialEq)]
enum S3Path {
    Root,
    Bucket { bucket: String },
    Object { bucket: String, key: String },
}

impl S3Path {
    fn from_s3s(p: &s3s::path::S3Path) -> Self {
        match p {
            s3s::path::S3Path::Root => Self::Root,
            s3s::path::S3Path::Bucket { bucket } => Self::Bucket {
                bucket: bucket.to_string(),
            },
            s3s::path::S3Path::Object { bucket, key } => Self::Object {
                bucket: bucket.to_string(),
                key: key.to_string(),
            },
        }
    }

    fn get_bucket_name(&self) -> Option<&str> {
        match self {
            Self::Root => None,
            Self::Bucket { bucket } | Self::Object { bucket, .. } => Some(bucket),
        }
    }
}

/// Recorded at the s3s access hook: s3s's parse, and our parse of the same
/// request (the URI and headers the middleware saw).
#[derive(Debug, Clone)]
struct Seen {
    op: String,
    s3s_path: S3Path,
    s3s_identity: Option<String>,
    /// `Some(access key)`, `Some("$anonymous")`, or `None` (no principal).
    our_identity: Option<String>,
    hook_admitted: bool,
    authz_action: S3Action,
    authz_bucket: String,
    authz_key: String,
    admission_bucket: String,
    admission_key: String,
    admission_prefix: String,
    our_list_prefix: Option<String>,
    /// s3s's parsed `prefix` input, when the op reached a LIST handler.
    s3s_list_prefix: Option<Option<String>>,
}

/// Per-request slot, carried as a request extension to the hook.
#[derive(Clone, Default)]
struct Slot(Arc<Mutex<Option<Seen>>>);

struct RecordingAccess;

#[async_trait::async_trait]
impl S3Access for RecordingAccess {
    async fn check(&self, cx: &mut S3AccessContext<'_>) -> S3Result<()> {
        let verdict = VerifiedIdentityS3sAccess.check(cx).await;
        let target = RequestTarget::from_uri(cx.uri()).expect("s3s decoded this path");
        let authz = crate::iam::middleware::authz_target(cx.method(), &target);
        let admission = crate::admission::middleware::OwnedRequestInfo::from_raw(
            cx.method().as_str(),
            cx.uri().path(),
            cx.uri().query().unwrap_or(""),
            false,
            None,
        );
        let our_identity = cx.extensions_mut().get::<AuthenticatedUser>().map(|u| {
            if u.is_anonymous() {
                "$anonymous".to_string()
            } else {
                u.access_key_id.clone()
            }
        });
        let seen = Seen {
            op: cx.s3_op().name().to_string(),
            s3s_path: S3Path::from_s3s(cx.s3_path()),
            s3s_identity: cx.credentials().map(|c| c.access_key.clone()),
            our_identity,
            hook_admitted: verdict.is_ok(),
            authz_action: authz.action,
            authz_bucket: authz.bucket.to_string(),
            authz_key: authz.key.to_string(),
            admission_bucket: admission.bucket.clone(),
            admission_key: admission.key.clone(),
            admission_prefix: admission.list_prefix.clone(),
            our_list_prefix: target.query_value("prefix").map(str::to_string),
            s3s_list_prefix: None,
        };
        if let Some(slot) = cx.extensions_mut().get::<Slot>() {
            *slot.0.lock().unwrap() = Some(seen);
        }
        verdict
    }
}

/// Every operation answers `NotImplemented`, except LIST, which first
/// records the `prefix` s3s parsed.
struct NopS3;

fn record_prefix(ext: &axum::http::Extensions, prefix: Option<String>) {
    if let Some(slot) = ext.get::<Slot>() {
        if let Some(seen) = slot.0.lock().unwrap().as_mut() {
            seen.s3s_list_prefix = Some(prefix);
        }
    }
}

#[async_trait::async_trait]
impl s3s::S3 for NopS3 {
    async fn list_objects_v2(
        &self,
        req: S3Request<ListObjectsV2Input>,
    ) -> S3Result<S3Response<ListObjectsV2Output>> {
        record_prefix(&req.extensions, req.input.prefix.clone());
        Err(s3s::s3_error!(NotImplemented))
    }

    async fn list_objects(
        &self,
        req: S3Request<ListObjectsInput>,
    ) -> S3Result<S3Response<ListObjectsOutput>> {
        record_prefix(&req.extensions, req.input.prefix.clone());
        Err(s3s::s3_error!(NotImplemented))
    }
}

fn iam_user(id: i64, name: &str, u: User) -> IamUser {
    IamUser {
        id,
        name: name.into(),
        access_key_id: u.ak.into(),
        secret_access_key: u.sk.into(),
        enabled: true,
        created_at: String::new(),
        permissions: vec![Permission {
            id,
            effect: "Allow".into(),
            actions: vec!["*".into()],
            resources: vec!["*".into()],
            conditions: None,
        }],
        group_ids: vec![],
        auth_source: "local".into(),
        iam_policies: vec![],
    }
}

/// Both routers of one test: `prod` is the production S3 router
/// (`s3_router::build_s3_router_with`, every layer and interceptor) over the
/// recording hook and a no-op `S3`; `bare` is the s3s service alone, which
/// tells what s3s would make of a request the production router serves
/// without s3s (the form-POST interceptor).
struct Harness {
    prod: Router,
    bare: Router,
    _data: tempfile::TempDir,
}

async fn harness() -> Harness {
    use s3s::service::S3ServiceBuilder;

    let iam: crate::iam::SharedIamState = Arc::new(arc_swap::ArcSwap::from_pointee(IamState::Iam(
        IamIndex::from_users(vec![iam_user(1, "alice", ALICE), iam_user(2, "bob", BOB)]),
    )));
    let mut config = crate::config::Config::default();
    config.buckets.insert(
        PUBLIC_BUCKET.to_string(),
        crate::bucket_policy::BucketPolicyConfig {
            public_prefixes: vec![PUBLIC_PREFIX.to_string()],
            ..Default::default()
        },
    );
    let snapshot: crate::bucket_policy::SharedPublicPrefixSnapshot =
        Arc::new(arc_swap::ArcSwap::from_pointee(
            crate::bucket_policy::PublicPrefixSnapshot::from_config(&config.buckets),
        ));
    let chain = crate::admission::build_shared_chain_from_parts(&config.buckets, &[]);

    let data = tempfile::tempdir().unwrap();
    let backend: Box<dyn crate::storage::StorageBackend> = Box::new(
        crate::storage::FilesystemBackend::new(data.path().to_path_buf())
            .await
            .unwrap(),
    );
    let engine =
        crate::deltaglider::DeltaGliderEngine::new_with_backend(Arc::new(backend), &config, None);
    let metrics = Arc::new(crate::metrics::Metrics::new());
    let state = Arc::new(crate::api::handlers::AppState {
        engine: arc_swap::ArcSwap::from_pointee(engine),
        multipart: Arc::new(crate::multipart::MultipartStore::new(
            config.max_object_size,
        )),
        metrics: metrics.clone(),
        usage_scanner: Arc::new(crate::usage_scanner::UsageScanner::new()),
        bucket_usage: None,
        reference_lock: None,
        config_db: None,
        maintenance_gate: Arc::new(crate::maintenance::gate::MaintenanceGate::new()),
        maintenance_notify: Arc::new(tokio::sync::Notify::new()),
        backend_capabilities: Default::default(),
        backend_health: Default::default(),
    });
    let rate_limiter = crate::rate_limiter::RateLimiter::new(
        100,
        Duration::from_secs(300),
        Duration::from_secs(600),
    );
    let replay_cache: crate::api::auth::ReplayCache = Default::default();
    let shared_config: crate::config::SharedConfig =
        Arc::new(tokio::sync::RwLock::new(config.clone()));
    let prod = crate::api::s3_router::build_s3_router_with(
        &state,
        &iam,
        &metrics,
        &rate_limiter,
        &replay_cache,
        &config,
        false,
        &snapshot,
        &chain,
        &shared_config,
        NopS3,
        RecordingAccess,
    );

    let mut builder = S3ServiceBuilder::new(NopS3);
    builder.set_auth(DeltaGliderS3sAuth { iam_state: iam });
    builder.set_access(RecordingAccess);
    builder.set_config(crate::api::s3s_hooks::s3s_config());
    let service =
        axum::error_handling::HandleError::new(builder.build(), |e: s3s::HttpError| async move {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:?}"))
        });
    let bare = Router::new().fallback_service(service);
    Harness {
        prod,
        bare,
        _data: data,
    }
}

// ── request construction ────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum Auth {
    None,
    /// SigV4 `Authorization` header, with this body hash (signed as given).
    V4Header(User, &'static str),
    V4Presigned(User),
    /// A valid presigned query for the first user AND a valid header for
    /// the second.
    V4PresignedAndHeader(User, User),
    /// Two `Authorization` headers, each valid for its user.
    DuplicateHeaders(User, User),
    V2Header(User),
    V2Presigned(User),
    /// A SigV2 presigned query for the first user, a SigV4 header for the
    /// second.
    V2PresignedAndV4Header(User, User),
}

const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(Clone, Debug)]
struct RawRequest {
    method: Method,
    /// Raw path and query, as sent on the wire.
    path_and_query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    auth: Auth,
}

fn full_url(pq: &str) -> String {
    format!("http://{HOST}{pq}")
}

fn v4_settings(presigned: bool) -> aws_sigv4::http_request::SigningSettings {
    use aws_sigv4::http_request::{
        PayloadChecksumKind, PercentEncodingMode, SignatureLocation, SigningSettings,
        UriPathNormalizationMode,
    };
    let mut settings = SigningSettings::default();
    settings.percent_encoding_mode = PercentEncodingMode::Single;
    settings.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
    if presigned {
        settings.signature_location = SignatureLocation::QueryParams;
        settings.expires_in = Some(Duration::from_secs(600));
    } else {
        settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
    }
    settings
}

/// SigV4-sign; returns the headers (header mode) or query params (presigned).
fn sign_v4(
    method: &Method,
    pq: &str,
    headers: &[(String, String)],
    user: User,
    payload: &str,
    presigned: bool,
) -> Option<Vec<(String, String)>> {
    use aws_sigv4::http_request::{sign, SignableBody, SignableRequest};
    use aws_sigv4::sign::v4;
    let identity =
        aws_credential_types::Credentials::new(user.ak, user.sk, None, None, "test").into();
    let settings = v4_settings(presigned);
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region("us-east-1")
        .name("s3")
        .time(SystemTime::now())
        .settings(settings)
        .build()
        .ok()?
        .into();
    // s3s verifies SigV4 over its own canonical form of the path: the
    // DECODED path, re-encoded (`uri_encode`, `/` kept). A caller signs that
    // form and may put any other spelling of the same path on the wire.
    let (path, query) = pq.split_once('?').map_or((pq, None), |(p, q)| (p, Some(q)));
    let canonical = s3s_canonical_path(path)?;
    let url = full_url(&match query {
        Some(q) => format!("{canonical}?{q}"),
        None => canonical,
    });
    let body = if presigned {
        SignableBody::UnsignedPayload
    } else {
        SignableBody::Precomputed(payload.to_string())
    };
    let signable = SignableRequest::new(
        method.as_str(),
        &url,
        headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        body,
    )
    .ok()?;
    let (instructions, _) = sign(signable, &params).ok()?.into_parts();
    Some(if presigned {
        instructions
            .params()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    } else {
        instructions
            .headers()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    })
}

/// s3s's SigV4 canonical path: decode, then encode every byte outside
/// `A-Za-z0-9-_.~/` as `%XX` (uppercase). `None`: the path does not decode.
fn s3s_canonical_path(raw: &str) -> Option<String> {
    let decoded = urlencoding::decode(raw).ok()?;
    let mut out = String::with_capacity(decoded.len());
    for b in decoded.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    Some(out)
}

fn hmac_sha1_b64(secret: &str, data: &str) -> String {
    use base64::Engine as _;
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(ring::hmac::sign(&key, data.as_bytes()))
}

fn raw_path(pq: &str) -> &str {
    pq.split_once('?').map_or(pq, |(p, _)| p)
}

fn append_query(pq: &str, params: &[(String, String)]) -> String {
    let mut out = pq.to_string();
    for (k, v) in params {
        out.push(if out.contains('?') { '&' } else { '?' });
        out.push_str(&urlencoding::encode(k));
        out.push('=');
        out.push_str(&urlencoding::encode(v));
    }
    out
}

/// Build the wire request. `None` when the raw target is not a valid URI
/// (hyper would refuse it before any layer runs).
fn build(req: &RawRequest) -> Option<Request<Body>> {
    let mut headers = req.headers.clone();
    headers.push(("host".into(), HOST.into()));
    // A streaming payload needs its framing headers, or s3s refuses it
    // before the access hook and the case tests nothing.
    if let Auth::V4Header(_, payload) = &req.auth {
        if payload.starts_with("STREAMING-") {
            headers.retain(|(k, _)| k != "content-encoding");
            headers.push(("content-encoding".into(), "aws-chunked".into()));
            headers.push(("x-amz-decoded-content-length".into(), "5".into()));
            if payload.ends_with("-TRAILER") {
                headers.push(("x-amz-trailer".into(), "x-amz-checksum-crc32".into()));
            }
        }
    }
    let mut pq = req.path_and_query.clone();
    let mut auth_headers: Vec<(String, String)> = Vec::new();
    let v4_header = |u: User, payload: &str, pq: &str, headers: &[(String, String)]| {
        let mut hs = headers.to_vec();
        hs.push(("x-amz-content-sha256".into(), payload.to_string()));
        sign_v4(&req.method, pq, &hs, u, payload, false).map(|signed| {
            let mut out = vec![("x-amz-content-sha256".to_string(), payload.to_string())];
            out.extend(
                signed
                    .into_iter()
                    .filter(|(k, _)| k != "x-amz-content-sha256"),
            );
            out
        })
    };
    let v2_expires = || (chrono::Utc::now().timestamp() + 600).to_string();
    match &req.auth {
        Auth::None => {}
        Auth::V4Header(u, payload) => auth_headers = v4_header(*u, payload, &pq, &headers)?,
        Auth::V4Presigned(u) => {
            let params = sign_v4(&req.method, &pq, &headers, *u, "", true)?;
            pq = append_query(&pq, &params);
        }
        Auth::V4PresignedAndHeader(q, h) => {
            let params = sign_v4(&req.method, &pq, &headers, *q, "", true)?;
            pq = append_query(&pq, &params);
            auth_headers = v4_header(*h, EMPTY_SHA256, &pq, &headers)?;
        }
        Auth::DuplicateHeaders(a, b) => {
            let first = v4_header(*a, EMPTY_SHA256, &pq, &headers)?;
            let second = v4_header(*b, EMPTY_SHA256, &pq, &headers)?;
            auth_headers = first;
            auth_headers.extend(second.into_iter().filter(|(k, _)| k == "authorization"));
        }
        Auth::V2Header(u) => {
            let date = httpdate(SystemTime::now());
            let sts = format!("{}\n\n\n{date}\n{}", req.method, raw_path(&pq));
            auth_headers = vec![
                ("date".into(), date),
                (
                    "authorization".into(),
                    format!("AWS {}:{}", u.ak, hmac_sha1_b64(u.sk, &sts)),
                ),
            ];
        }
        Auth::V2Presigned(u) | Auth::V2PresignedAndV4Header(u, _) => {
            let expires = v2_expires();
            let sts = format!("{}\n\n\n{expires}\n{}", req.method, raw_path(&pq));
            let params = vec![
                ("AWSAccessKeyId".to_string(), u.ak.to_string()),
                ("Expires".to_string(), expires),
                ("Signature".to_string(), hmac_sha1_b64(u.sk, &sts)),
            ];
            pq = append_query(&pq, &params);
            if let Auth::V2PresignedAndV4Header(_, h) = &req.auth {
                auth_headers = v4_header(*h, EMPTY_SHA256, &pq, &headers)?;
            }
        }
    }
    let mut builder = Request::builder().method(req.method.clone()).uri(&pq);
    for (k, v) in headers.iter().chain(auth_headers.iter()) {
        builder = builder.header(k.as_str(), HeaderValue::from_str(v).ok()?);
    }
    builder.body(Body::from(req.body.clone())).ok()
}

fn httpdate(t: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Utc> = t.into();
    dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

// ── the contract ────────────────────────────────────────────────────────

/// The action our middleware must derive for what s3s resolved: the
/// operation name plus the path level s3s parsed.
fn expected_action(op: &str, path: &S3Path, method: &Method) -> Option<S3Action> {
    let read = ["Get", "Head", "List"].iter().any(|p| op.starts_with(p));
    let delete = op.starts_with("Delete") || op.starts_with("Abort");
    let write = [
        "Put", "Create", "Upload", "Complete", "Copy", "Restore", "Select", "Write",
    ]
    .iter()
    .any(|p| op.starts_with(p));
    match path {
        S3Path::Root => (op == "ListBuckets").then_some(S3Action::List),
        S3Path::Object { .. } => {
            // Every POST on an object is a write for our middleware
            // (SelectObjectContent, RestoreObject, multipart lifecycle).
            if *method == Method::POST {
                Some(S3Action::Write)
            } else if read {
                Some(S3Action::Read)
            } else if delete {
                Some(S3Action::Delete)
            } else if write {
                Some(S3Action::Write)
            } else {
                None
            }
        }
        S3Path::Bucket { .. } => {
            if op == "DeleteObjects" {
                Some(S3Action::Delete)
            } else if read {
                Some(S3Action::List)
            } else if delete || write {
                Some(S3Action::Admin)
            } else {
                None
            }
        }
    }
}

/// The key the engine serves for an s3s key (`ObjectKey::parse` drops the
/// leading slashes).
fn engine_key(s3s_key: &str) -> &str {
    s3s_key.trim_start_matches('/')
}

/// Every disagreement between the two parses of one request.
fn violations(method: &Method, seen: &Seen) -> Vec<String> {
    let mut v = Vec::new();
    if !seen.hook_admitted {
        // The production hook refused the request: never served. Nothing
        // below applies (this is how S1/S2 are closed).
        return v;
    }
    // Identity. Our principal must be the one s3s verified. `$anonymous`
    // is the least-privileged principal: authorizing as it and serving a
    // verified caller grants nothing extra.
    match (&seen.our_identity, &seen.s3s_identity) {
        (Some(ours), _) if ours == "$anonymous" => {}
        (Some(ours), Some(theirs)) if ours == theirs => {}
        (None, _) if seen.op == "PostObject" => {} // checked by the form contract
        (ours, theirs) => v.push(format!("identity: ours {ours:?}, s3s {theirs:?}")),
    }
    let (s3s_bucket, s3s_key) = match &seen.s3s_path {
        S3Path::Root => ("", None),
        S3Path::Bucket { bucket } => (bucket.as_str(), None),
        S3Path::Object { bucket, key } => (bucket.as_str(), Some(engine_key(key))),
    };
    if seen.authz_bucket != s3s_bucket {
        v.push(format!(
            "bucket: authz {:?}, s3s {s3s_bucket:?}",
            seen.authz_bucket
        ));
    }
    if seen.admission_bucket != s3s_bucket {
        v.push(format!(
            "bucket: admission {:?}, s3s {s3s_bucket:?}",
            seen.admission_bucket
        ));
    }
    // An s3s key that is only slashes is the empty key, which the engine
    // refuses (`validate_object`): never served, whatever we classified.
    let served_key = s3s_key.filter(|k| !k.is_empty());
    if let Some(key) = served_key {
        if seen.authz_key != key {
            v.push(format!("key: authz {:?}, s3s {key:?}", seen.authz_key));
        }
        if seen.admission_key != key {
            v.push(format!(
                "key: admission {:?}, s3s {key:?}",
                seen.admission_key
            ));
        }
    } else if s3s_key.is_none() && !seen.authz_key.is_empty() {
        v.push(format!(
            "key: authz {:?} on an s3s {} request",
            seen.authz_key, seen.op
        ));
    }
    if s3s_key.is_none() || served_key.is_some() {
        match expected_action(&seen.op, &seen.s3s_path, method) {
            Some(want) if want == seen.authz_action => {}
            Some(want) => v.push(format!(
                "action: ours {:?}, s3s {} needs {want:?}",
                seen.authz_action, seen.op
            )),
            None if seen.op == "PostObject" => {}
            None => v.push(format!(
                "unmapped s3s op {} on {:?}",
                seen.op, seen.s3s_path
            )),
        }
    }
    if let Some(theirs) = &seen.s3s_list_prefix {
        let theirs = theirs.clone().unwrap_or_default();
        let ours = seen.our_list_prefix.clone().unwrap_or_default();
        if ours != theirs {
            v.push(format!("list prefix: ours {ours:?}, s3s {theirs:?}"));
        }
        if seen.admission_prefix != theirs {
            v.push(format!(
                "list prefix: admission {:?}, s3s {theirs:?}",
                seen.admission_prefix
            ));
        }
    }
    v
}

#[derive(Debug)]
struct Outcome {
    status: StatusCode,
    /// What reached the s3s access hook of the production router.
    seen: Option<Seen>,
    /// Our form-POST predicate claimed the request (the router serves it
    /// with the form handler, never with s3s).
    form_bucket: Option<String>,
    /// For a claimed form POST: what bare s3s makes of the same request.
    s3s_form_view: Option<Seen>,
}

async fn send(router: &Router, raw: &RawRequest) -> Option<(StatusCode, Option<Seen>)> {
    let mut request = build(raw)?;
    let slot = Slot::default();
    request.extensions_mut().insert(slot.clone());
    let response = router.clone().oneshot(request).await.unwrap();
    let seen = slot.0.lock().unwrap().take();
    Some((response.status(), seen))
}

async fn run(h: &Harness, raw: &RawRequest) -> Option<Outcome> {
    let request = build(raw)?;
    let form_bucket = crate::api::handlers::form_post::form_post_bucket(
        request.method(),
        request.uri(),
        request.headers(),
    );
    let (status, seen) = send(&h.prod, raw).await?;
    let s3s_form_view = match &form_bucket {
        Some(_) => send(&h.bare, raw).await?.1,
        None => None,
    };
    Some(Outcome {
        status,
        seen,
        form_bucket,
        s3s_form_view,
    })
}

/// The form-POST contract: the router intercepts exactly the requests s3s
/// would parse as `PostObject`, on the same bucket. The form handler then
/// authorizes against the bucket admission matched.
fn form_violations(raw: &RawRequest, out: &Outcome) -> Vec<String> {
    let mut v = Vec::new();
    let Some(ours) = &out.form_bucket else {
        return v;
    };
    // The form handler refuses an invalid bucket name before anything else
    // (`handle_form_post_upload`): never served.
    if crate::security::validate_bucket_name(ours).is_err() {
        return v;
    }
    // The production router serves it with the form handler: s3s never runs.
    if let Some(seen) = &out.seen {
        v.push(format!("form bucket {ours:?}, but s3s ran {}", seen.op));
    }
    match &out.s3s_form_view {
        Some(seen) if seen.op == "PostObject" => {
            if seen.s3s_path.get_bucket_name() != Some(ours.as_str()) {
                v.push(format!(
                    "form bucket: ours {ours:?}, s3s {:?}",
                    seen.s3s_path
                ));
            }
            let admission = crate::admission::middleware::OwnedRequestInfo::from_raw(
                raw.method.as_str(),
                raw_path(&raw.path_and_query),
                "",
                false,
                None,
            );
            if &admission.bucket != ours {
                v.push(format!(
                    "form bucket: ours {ours:?}, admission {:?}",
                    admission.bucket
                ));
            }
        }
        Some(seen) => v.push(format!(
            "form bucket {ours:?}, but s3s resolved {}",
            seen.op
        )),
        None => v.push(format!("form bucket {ours:?}, but s3s refuses the request")),
    }
    v
}

fn get(pq: &str, auth: Auth) -> RawRequest {
    RawRequest {
        method: Method::GET,
        path_and_query: pq.into(),
        headers: vec![],
        body: vec![],
        auth,
    }
}

fn req(method: Method, pq: &str, headers: &[(&str, &str)], auth: Auth) -> RawRequest {
    RawRequest {
        method,
        path_and_query: pq.into(),
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        body: vec![],
        auth,
    }
}

fn v4(u: User) -> Auth {
    Auth::V4Header(u, EMPTY_SHA256)
}

fn form_body() -> (String, Vec<u8>) {
    let boundary = "XBOUNDARYX";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"key\"\r\n\r\nk.txt\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"k.txt\"\r\n\
         Content-Type: text/plain\r\n\r\nhello\r\n--{boundary}--\r\n"
    );
    (
        format!("multipart/form-data; boundary={boundary}"),
        body.into_bytes(),
    )
}

fn form_post(pq: &str) -> RawRequest {
    let (ct, body) = form_body();
    RawRequest {
        method: Method::POST,
        path_and_query: pq.into(),
        headers: vec![("content-type".into(), ct)],
        body,
        auth: Auth::None,
    }
}

// ── the table ───────────────────────────────────────────────────────────

/// How far a table case must get. `Served` cases prove the harness signs
/// what s3s verifies, so the contract assertions are not vacuous.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Expect {
    /// Reaches the hook and the hook admits it.
    Served,
    /// Refused somewhere (our layer, s3s, or the hook).
    Refused,
    /// Either; only the contract is asserted.
    Any,
}

fn table() -> Vec<(&'static str, RawRequest, Expect)> {
    use Expect::*;
    let chunked = [
        ("content-encoding", "aws-chunked"),
        ("x-amz-decoded-content-length", "5"),
    ];
    let mut t = vec![
        ("plain get", get("/alpha-bucket/k.txt", v4(ALICE)), Served),
        (
            "list v2",
            get("/alpha-bucket?list-type=2&prefix=a%2Fb", v4(ALICE)),
            Served,
        ),
        (
            "list v1 plus prefix",
            get("/alpha-bucket?prefix=a+b", v4(ALICE)),
            Served,
        ),
        (
            "encoded prefix name",
            get("/alpha-bucket?list-type=2&%70refix=secret", v4(ALICE)),
            Served,
        ),
        (
            "encoding-type",
            get(
                "/alpha-bucket?list-type=2&encoding-type=url&prefix=x",
                v4(ALICE),
            ),
            Served,
        ),
        ("list buckets", get("/", v4(ALICE)), Served),
        (
            "encoded separator",
            get("/alpha-bucket%2Fsecret.txt", v4(ALICE)),
            Any,
        ),
        (
            "encoded letter",
            get("/alpha-bucke%74/secre%74.txt", v4(ALICE)),
            Any,
        ),
        (
            "unicode key",
            get("/alpha-bucket/%E6%97%A5%E6%9C%AC.txt", v4(ALICE)),
            Any,
        ),
        ("plus in key", get("/alpha-bucket/a+b.txt", v4(ALICE)), Any),
        (
            "encoded plus",
            get("/alpha-bucket/a%2Bb.txt", v4(ALICE)),
            Any,
        ),
        (
            "double slash key",
            get("/alpha-bucket//secret.txt", v4(ALICE)),
            Any,
        ),
        (
            "encoded double slash",
            get("/alpha-bucket/%2F%2Fsecret.txt", v4(ALICE)),
            Any,
        ),
        (
            "trailing double slash",
            get("/alpha-bucket//", v4(ALICE)),
            Any,
        ),
        ("empty bucket", get("//alpha-bucket/k", v4(ALICE)), Any),
        ("dot segment", get("/alpha-bucket/./k", v4(ALICE)), Any),
        (
            "dotdot segment",
            get("/alpha-bucket/../other/k", v4(ALICE)),
            Any,
        ),
        (
            "presigned get",
            get("/alpha-bucket/k.txt", Auth::V4Presigned(ALICE)),
            Served,
        ),
        (
            "presigned encoded",
            get("/alpha-bucket/secre%74.txt", Auth::V4Presigned(ALICE)),
            Any,
        ),
        (
            "presigned + header (other user)",
            get("/alpha-bucket/k", Auth::V4PresignedAndHeader(BOB, ALICE)),
            Any,
        ),
        (
            "presigned + header (same user)",
            get("/alpha-bucket/k", Auth::V4PresignedAndHeader(ALICE, ALICE)),
            Any,
        ),
        (
            "duplicate authorization",
            get("/alpha-bucket/k", Auth::DuplicateHeaders(BOB, ALICE)),
            Refused,
        ),
        (
            "duplicate authorization same user",
            get("/alpha-bucket/k", Auth::DuplicateHeaders(ALICE, ALICE)),
            Refused,
        ),
        (
            "sigv2 header",
            get("/alpha-bucket/k", Auth::V2Header(ALICE)),
            Any,
        ),
        (
            "sigv2 presigned",
            get("/alpha-bucket/k", Auth::V2Presigned(ALICE)),
            Any,
        ),
        (
            "sigv2 presigned + v4 header",
            get("/alpha-bucket/k", Auth::V2PresignedAndV4Header(BOB, ALICE)),
            Refused,
        ),
        (
            "sigv2 presigned on public prefix",
            get(
                &format!("/{PUBLIC_BUCKET}/{PUBLIC_PREFIX}k"),
                Auth::V2Presigned(BOB),
            ),
            Any,
        ),
        (
            "anonymous public read",
            get(&format!("/{PUBLIC_BUCKET}/{PUBLIC_PREFIX}k"), Auth::None),
            Served,
        ),
        (
            "anonymous public encoded",
            get(&format!("/{PUBLIC_BUCKET}/ope%6E/k"), Auth::None),
            Any,
        ),
        (
            "anonymous public list",
            get(
                &format!("/{PUBLIC_BUCKET}?list-type=2&prefix={PUBLIC_PREFIX}"),
                Auth::None,
            ),
            Any,
        ),
        (
            "anonymous private",
            get("/alpha-bucket/k", Auth::None),
            Refused,
        ),
        (
            "virtual host style",
            req(
                Method::GET,
                "/k.txt",
                &[("x-forwarded-host", "alpha-bucket.localhost")],
                v4(ALICE),
            ),
            Any,
        ),
        (
            "put object",
            req(Method::PUT, "/alpha-bucket/k.txt", &[], v4(ALICE)),
            Served,
        ),
        (
            "put tagging",
            req(Method::PUT, "/alpha-bucket/k.txt?tagging", &[], v4(ALICE)),
            Any,
        ),
        (
            "upload part",
            req(
                Method::PUT,
                "/alpha-bucket/k.txt?partNumber=1&uploadId=u1",
                &[],
                v4(ALICE),
            ),
            Any,
        ),
        (
            "create bucket",
            req(Method::PUT, "/alpha-bucket", &[], v4(ALICE)),
            Served,
        ),
        (
            "delete bucket",
            req(Method::DELETE, "/alpha-bucket", &[], v4(ALICE)),
            Served,
        ),
        (
            "delete object",
            req(Method::DELETE, "/alpha-bucket/k", &[], v4(ALICE)),
            Served,
        ),
        (
            "abort multipart",
            req(
                Method::DELETE,
                "/alpha-bucket/k?uploadId=u1",
                &[],
                v4(ALICE),
            ),
            Any,
        ),
        (
            "batch delete",
            req(Method::POST, "/alpha-bucket?delete", &[], v4(ALICE)),
            Any,
        ),
        (
            "encoded batch delete",
            req(Method::POST, "/alpha-bucket?%64elete", &[], v4(ALICE)),
            Any,
        ),
        (
            "create multipart",
            req(Method::POST, "/alpha-bucket/k?uploads", &[], v4(ALICE)),
            Any,
        ),
        (
            "copy plain",
            req(
                Method::PUT,
                "/alpha-bucket/dst",
                &[("x-amz-copy-source", "/alpha-bucket/src")],
                v4(ALICE),
            ),
            Served,
        ),
        (
            "copy encoded",
            req(
                Method::PUT,
                "/alpha-bucket/dst",
                &[("x-amz-copy-source", "alpha-bucket/secre%74%2Fx")],
                v4(ALICE),
            ),
            Any,
        ),
        (
            "copy version",
            req(
                Method::PUT,
                "/alpha-bucket/dst",
                &[("x-amz-copy-source", "alpha-bucket/src?versionId=v1")],
                v4(ALICE),
            ),
            Any,
        ),
        (
            "unsigned payload",
            req(
                Method::PUT,
                "/alpha-bucket/k",
                &[],
                Auth::V4Header(ALICE, "UNSIGNED-PAYLOAD"),
            ),
            Any,
        ),
        (
            "chunked signed",
            req(
                Method::PUT,
                "/alpha-bucket/k",
                &chunked,
                Auth::V4Header(ALICE, "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"),
            ),
            Any,
        ),
        (
            "chunked unsigned trailer",
            req(
                Method::PUT,
                "/alpha-bucket/k",
                &[
                    ("content-encoding", "aws-chunked"),
                    ("x-amz-decoded-content-length", "5"),
                    ("x-amz-trailer", "x-amz-checksum-crc32"),
                ],
                Auth::V4Header(ALICE, "STREAMING-UNSIGNED-PAYLOAD-TRAILER"),
            ),
            Any,
        ),
        (
            "chunked signed trailer",
            req(
                Method::PUT,
                "/alpha-bucket/k",
                &[
                    ("content-encoding", "aws-chunked"),
                    ("x-amz-decoded-content-length", "5"),
                    ("x-amz-trailer", "x-amz-checksum-crc32"),
                ],
                Auth::V4Header(ALICE, "STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER"),
            ),
            Any,
        ),
        ("form post", form_post("/alpha-bucket"), Any),
        ("form post trailing slash", form_post("/alpha-bucket/"), Any),
        (
            "form post encoded bucket",
            form_post("/alpha-bucke%74"),
            Any,
        ),
        ("form post empty bucket", form_post("//alpha-bucket"), Any),
        (
            "form post double trailing",
            form_post("/alpha-bucket//"),
            Any,
        ),
        (
            "form post encoded slash",
            form_post("/alpha-bucket%2F"),
            Any,
        ),
    ];
    let mut signed_form = form_post("/alpha-bucket");
    signed_form.auth = v4(ALICE);
    t.push(("form post with authorization header", signed_form, Any));
    t
}

#[tokio::test]
async fn table_cases_honour_the_contract() {
    let h = harness().await;
    let mut failures = Vec::new();
    for (name, raw, expect) in table() {
        let out = run(&h, &raw)
            .await
            .unwrap_or_else(|| panic!("{name}: the harness could not build the request"));
        let served = out.seen.as_ref().is_some_and(|s| s.hook_admitted);
        match expect {
            Expect::Served if !served => failures.push(format!(
                "{name}: expected to be served, got {} (seen: {:?})",
                out.status, out.seen
            )),
            Expect::Refused if served => {
                failures.push(format!("{name}: expected a refusal, but it was served"))
            }
            _ => {}
        }
        let mut v: Vec<String> = out
            .seen
            .as_ref()
            .map(|s| violations(&raw.method, s))
            .unwrap_or_default();
        v.extend(form_violations(&raw, &out));
        for violation in v {
            failures.push(format!(
                "{name} ({} {}): {violation}",
                raw.method, raw.path_and_query
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "contract violations:\n{}",
        failures.join("\n")
    );
}

// ── the generator ───────────────────────────────────────────────────────

fn segment() -> impl Strategy<Value = String> {
    prop_oneof![
        6 => Just("k".to_string()),
        3 => Just("secret.txt".to_string()),
        3 => Just("open".to_string()),
        2 => Just(String::new()),
        1 => Just(".".to_string()),
        1 => Just("..".to_string()),
        1 => Just("%2E".to_string()),
        2 => Just("%2F".to_string()),
        1 => Just("%2f".to_string()),
        2 => Just("%74".to_string()),
        1 => Just("%25".to_string()),
        1 => Just("%2B".to_string()),
        2 => Just("a+b".to_string()),
        1 => Just("%20".to_string()),
        2 => Just("%E6%97%A5".to_string()),
        1 => Just("%C3%A9".to_string()),
        1 => Just("a=b&c".to_string()),
        1 => Just("%".to_string()),
        1 => Just("%zz".to_string()),
        1 => Just("%ff".to_string()),
        4 => "[a-z0-9._~-]{1,6}",
    ]
}

fn bucket_segment() -> impl Strategy<Value = String> {
    prop_oneof![
        8 => Just("alpha-bucket".to_string()),
        4 => Just(PUBLIC_BUCKET.to_string()),
        2 => Just("alph%61-bucket".to_string()),
        1 => Just("Alpha-Bucket".to_string()),
        1 => Just(String::new()),
        2 => Just("alpha-bucket%2Fk".to_string()),
        1 => Just("pub-bucke%74".to_string()),
    ]
}

fn query_param() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("list-type=2".to_string()),
        Just("prefix=open%2F".to_string()),
        Just("prefix=a+b".to_string()),
        Just("%70refix=secret".to_string()),
        Just("prefix=".to_string()),
        Just("delimiter=%2F".to_string()),
        Just("encoding-type=url".to_string()),
        Just("max-keys=5".to_string()),
        Just("start-after=x".to_string()),
        Just("uploadId=u1".to_string()),
        Just("partNumber=1".to_string()),
        Just("uploads".to_string()),
        Just("delete".to_string()),
        Just("%64elete".to_string()),
        Just("tagging".to_string()),
        Just("acl".to_string()),
        Just("versionId=v1".to_string()),
        Just("versioning".to_string()),
        Just("location".to_string()),
    ]
}

fn user() -> impl Strategy<Value = User> {
    prop_oneof![Just(ALICE), Just(BOB)]
}

fn auth() -> impl Strategy<Value = Auth> {
    prop_oneof![
        2 => Just(Auth::None),
        8 => user().prop_map(v4),
        2 => user().prop_map(|u| Auth::V4Header(u, "UNSIGNED-PAYLOAD")),
        1 => user().prop_map(|u| Auth::V4Header(u, "STREAMING-AWS4-HMAC-SHA256-PAYLOAD")),
        1 => user().prop_map(|u| Auth::V4Header(u, "STREAMING-UNSIGNED-PAYLOAD-TRAILER")),
        1 => user().prop_map(|u| Auth::V4Header(u, "STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER")),
        4 => user().prop_map(Auth::V4Presigned),
        1 => (user(), user()).prop_map(|(a, b)| Auth::V4PresignedAndHeader(a, b)),
        1 => (user(), user()).prop_map(|(a, b)| Auth::DuplicateHeaders(a, b)),
        2 => user().prop_map(Auth::V2Header),
        2 => user().prop_map(Auth::V2Presigned),
        1 => (user(), user()).prop_map(|(a, b)| Auth::V2PresignedAndV4Header(a, b)),
    ]
}

fn raw_request() -> impl Strategy<Value = RawRequest> {
    let method = prop_oneof![
        Just(Method::GET),
        Just(Method::HEAD),
        Just(Method::PUT),
        Just(Method::POST),
        Just(Method::DELETE),
    ];
    let extra_header = prop_oneof![
        Just(None),
        Just(Some(("x-amz-copy-source", "/alpha-bucket/src"))),
        Just(Some(("x-amz-copy-source", "alpha-bucket/secre%74%2Fx"))),
        Just(Some((
            "x-amz-copy-source",
            "pub-bucket/open/%E6%97%A5?versionId=v"
        ))),
        Just(Some(("content-encoding", "aws-chunked"))),
        Just(Some((
            "content-type",
            "multipart/form-data; boundary=XBOUNDARYX"
        ))),
    ];
    (
        method,
        bucket_segment(),
        proptest::collection::vec(segment(), 0..4),
        prop::bool::ANY,
        proptest::collection::vec(query_param(), 0..3),
        auth(),
        extra_header,
    )
        .prop_map(|(method, bucket, segs, trailing, query, auth, extra)| {
            let mut path = format!("/{bucket}");
            for s in &segs {
                path.push('/');
                path.push_str(s);
            }
            if trailing {
                path.push('/');
            }
            let pq = if query.is_empty() {
                path
            } else {
                format!("{path}?{}", query.join("&"))
            };
            let mut headers = Vec::new();
            let mut body = Vec::new();
            if let Some((k, v)) = extra {
                if k == "content-type" {
                    let (ct, b) = form_body();
                    headers.push(("content-type".to_string(), ct));
                    body = b;
                } else {
                    headers.push((k.to_string(), v.to_string()));
                }
            }
            RawRequest {
                method,
                path_and_query: pq,
                headers,
                body,
                auth,
            }
        })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// Random raw requests: whatever reaches the s3s access hook, both
    /// parsers agree on it.
    #[test]
    fn generated_requests_honour_the_contract(raw in raw_request()) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let out = rt.block_on(async { run(&harness().await, &raw).await });
        prop_assume!(out.is_some());
        let out = out.unwrap();
        let mut v: Vec<String> = out
            .seen
            .as_ref()
            .map(|s| violations(&raw.method, s))
            .unwrap_or_default();
        v.extend(form_violations(&raw, &out));
        prop_assert!(v.is_empty(), "{} {} ({:?}): {:?}", raw.method, raw.path_and_query, raw.auth, v);
    }
}

/// The generator is not vacuous: a fixed sample of its requests gets past
/// both parsers often enough, over enough distinct operations, for the
/// property above to mean something.
#[tokio::test]
async fn generated_requests_reach_the_hook() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let h = harness().await;
    let mut runner = TestRunner::deterministic();
    let strategy = raw_request();
    let (mut built, mut served) = (0, 0);
    let mut ops = std::collections::BTreeSet::new();
    let mut failures = Vec::new();
    for _ in 0..400 {
        let raw = strategy.new_tree(&mut runner).unwrap().current();
        let Some(out) = run(&h, &raw).await else {
            continue;
        };
        built += 1;
        let mut v: Vec<String> = out
            .seen
            .as_ref()
            .map(|s| violations(&raw.method, s))
            .unwrap_or_default();
        v.extend(form_violations(&raw, &out));
        for violation in v {
            failures.push(format!(
                "{} {}: {violation}",
                raw.method, raw.path_and_query
            ));
        }
        if let Some(seen) = out.seen.filter(|s| s.hook_admitted) {
            served += 1;
            ops.insert(seen.op);
        }
    }
    assert!(
        failures.is_empty(),
        "contract violations:\n{}",
        failures.join("\n")
    );
    assert!(
        built >= 300,
        "only {built}/400 generated requests were valid HTTP"
    );
    assert!(
        served * 3 >= built,
        "only {served}/{built} generated requests reached the handler"
    );
    assert!(
        ops.len() >= 15,
        "only these operations were exercised: {ops:?}"
    );
}
