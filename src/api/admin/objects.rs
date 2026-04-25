//! Server-side bulk object operations for the admin UI.
//!
//! Pre-migration the React s3-browser shipped `@aws-sdk/client-s3`
//! and orchestrated bulk copy / move / delete / zip from the client:
//! it would `listObjectsV2` to expand folder selections, build a
//! collision-checked dest plan, then sequentially `copyObject` /
//! `deleteObject` each entry. That meant:
//!
//! - ~250 KB of AWS SDK shipped to every browser.
//! - Network drops mid-loop left orphaned half-copies on `move`.
//! - No cancellation, no progress, no atomicity.
//! - Bulk zip downloaded each object via SDK GET, assembled in
//!   browser memory (capped at 500 MB by `useS3Browser`).
//!
//! This module moves the orchestration into the proxy where the
//! engine is already running. Endpoints are session-gated (admin UI
//! only) and live under `/_/api/admin/objects/*`.
//!
//! Future iterations can stream zip output and add server-side
//! progress reporting; for v1 we match the existing client semantics
//! 1:1 so the migration is risk-free.

use crate::api::handlers::AppState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Request/response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CopyRequest {
    /// Source bucket — every key in `keys` is read from here.
    pub source_bucket: String,
    /// Destination bucket. May be the same as source.
    pub dest_bucket: String,
    /// Optional prefix prepended to every destination key.
    #[serde(default)]
    pub dest_prefix: String,
    /// Pairs of (source_key, relative_dest_suffix). The dest key is
    /// `dest_prefix + relative_suffix`. The client computes relatives
    /// because folder-selection semantics are UI-driven (which prefix
    /// counts as the "common prefix" depends on what the user picked).
    pub items: Vec<CopyItem>,
}

#[derive(Debug, Deserialize)]
pub struct CopyItem {
    pub source_key: String,
    pub relative: String,
}

#[derive(Debug, Serialize)]
pub struct CopyResponse {
    pub succeeded: usize,
    pub failed: usize,
    /// Per-key failures, newest last. Capped at 100 entries.
    pub failures: Vec<CopyFailure>,
}

#[derive(Debug, Serialize)]
pub struct CopyFailure {
    pub source_key: String,
    pub dest_key: String,
    pub error: String,
}

#[derive(Debug, Deserialize)]
pub struct MoveRequest {
    pub source_bucket: String,
    pub dest_bucket: String,
    #[serde(default)]
    pub dest_prefix: String,
    pub items: Vec<CopyItem>,
}

/// Shape mirrors `CopyResponse` plus a `deleted` count — moves are
/// copy-then-delete and the source delete is reported separately.
#[derive(Debug, Serialize)]
pub struct MoveResponse {
    pub succeeded: usize,
    pub failed: usize,
    pub deleted: usize,
    pub failures: Vec<CopyFailure>,
}

#[derive(Debug, Deserialize)]
pub struct DeleteRequest {
    pub bucket: String,
    pub keys: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DeleteResponse {
    pub deleted: usize,
    pub failed: usize,
    pub failures: Vec<DeleteFailure>,
}

#[derive(Debug, Serialize)]
pub struct DeleteFailure {
    pub key: String,
    pub error: String,
}

#[derive(Debug, Deserialize)]
pub struct ZipQuery {
    /// Comma-separated list of fully-qualified `bucket/key` pairs.
    /// Could be a body param too, but a query string lets the client
    /// trigger the response via plain `<a href>` for browser-driven
    /// download UX.
    pub keys: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const MAX_BULK_OBJECTS: usize = 10_000;
const MAX_FAILURE_ENTRIES: usize = 100;
const MAX_ZIP_BYTES: u64 = 500 * 1024 * 1024;

fn dest_key(dest_prefix: &str, relative: &str) -> String {
    if dest_prefix.is_empty() {
        relative.to_string()
    } else {
        format!("{}{}", dest_prefix, relative)
    }
}

/// Detect duplicate destination keys in a copy/move plan. The client
/// already does this; we re-check server-side because trusting the
/// client to validate was the cause of past silent overwrites.
fn detect_collisions(items: &[CopyItem], dest_prefix: &str) -> Vec<String> {
    use std::collections::HashMap;
    let mut counts: HashMap<String, usize> = HashMap::new();
    for it in items {
        *counts.entry(dest_key(dest_prefix, &it.relative)).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .filter_map(|(k, n)| if n > 1 { Some(k) } else { None })
        .collect()
}

// ---------------------------------------------------------------------------
// POST /_/api/admin/objects/copy
// ---------------------------------------------------------------------------

pub async fn copy_objects(
    State(state): State<Arc<crate::api::admin::AdminState>>,
    Json(req): Json<CopyRequest>,
) -> Result<Json<CopyResponse>, (StatusCode, String)> {
    if req.items.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "no items to copy".into()));
    }
    if req.items.len() > MAX_BULK_OBJECTS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "too many items ({} > limit {})",
                req.items.len(),
                MAX_BULK_OBJECTS
            ),
        ));
    }

    let collisions = detect_collisions(&req.items, &req.dest_prefix);
    if !collisions.is_empty() {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "{} destination key(s) would overwrite each other (e.g. {:?})",
                collisions.len(),
                collisions.first().cloned().unwrap_or_default()
            ),
        ));
    }

    let s3 = state.s3_state.clone();
    let res = run_copy_loop(&s3, &req).await;
    info!(
        "bulk copy: src={} dst={}/{} succeeded={} failed={}",
        req.source_bucket, req.dest_bucket, req.dest_prefix, res.succeeded, res.failed
    );
    Ok(Json(res))
}

async fn run_copy_loop(
    s3: &Arc<AppState>,
    req: &CopyRequest,
) -> CopyResponse {
    let engine = s3.engine.load();
    let mut succeeded = 0usize;
    let mut failed = 0usize;
    let mut failures: Vec<CopyFailure> = Vec::new();
    for it in &req.items {
        let dk = dest_key(&req.dest_prefix, &it.relative);
        let result = copy_one(&engine, &req.source_bucket, &it.source_key, &req.dest_bucket, &dk).await;
        match result {
            Ok(()) => succeeded += 1,
            Err(e) => {
                failed += 1;
                if failures.len() < MAX_FAILURE_ENTRIES {
                    failures.push(CopyFailure {
                        source_key: it.source_key.clone(),
                        dest_key: dk,
                        error: e,
                    });
                }
            }
        }
    }
    CopyResponse {
        succeeded,
        failed,
        failures,
    }
}

async fn copy_one(
    engine: &Arc<crate::deltaglider::DynEngine>,
    src_bucket: &str,
    src_key: &str,
    dst_bucket: &str,
    dst_key: &str,
) -> Result<(), String> {
    // Engine-level retrieve+store so encryption/compression stays
    // transparent (matches replication's copy_one design).
    let (data, meta) = engine
        .retrieve(src_bucket, src_key)
        .await
        .map_err(|e| format!("retrieve {}/{}: {}", src_bucket, src_key, e))?;

    if let Some(mp_etag) = meta.multipart_etag.clone() {
        engine
            .store_with_multipart_etag(
                dst_bucket,
                dst_key,
                &data,
                meta.content_type.clone(),
                meta.user_metadata.clone(),
                mp_etag,
            )
            .await
            .map_err(|e| format!("store {}/{}: {}", dst_bucket, dst_key, e))?;
    } else {
        engine
            .store(
                dst_bucket,
                dst_key,
                &data,
                meta.content_type.clone(),
                meta.user_metadata.clone(),
            )
            .await
            .map_err(|e| format!("store {}/{}: {}", dst_bucket, dst_key, e))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// POST /_/api/admin/objects/move
// ---------------------------------------------------------------------------

pub async fn move_objects(
    State(state): State<Arc<crate::api::admin::AdminState>>,
    Json(req): Json<MoveRequest>,
) -> Result<Json<MoveResponse>, (StatusCode, String)> {
    if req.items.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "no items to move".into()));
    }
    if req.items.len() > MAX_BULK_OBJECTS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "too many items ({} > limit {})",
                req.items.len(),
                MAX_BULK_OBJECTS
            ),
        ));
    }

    let collisions = detect_collisions(&req.items, &req.dest_prefix);
    if !collisions.is_empty() {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "{} destination key(s) would overwrite each other (e.g. {:?})",
                collisions.len(),
                collisions.first().cloned().unwrap_or_default()
            ),
        ));
    }

    let s3 = state.s3_state.clone();
    let copy_req = CopyRequest {
        source_bucket: req.source_bucket.clone(),
        dest_bucket: req.dest_bucket.clone(),
        dest_prefix: req.dest_prefix.clone(),
        items: req.items.iter().map(|i| CopyItem {
            source_key: i.source_key.clone(),
            relative: i.relative.clone(),
        }).collect(),
    };
    let copy_result = run_copy_loop(&s3, &copy_req).await;

    // Atomicity rule: only delete sources if EVERY copy succeeded.
    // Pre-migration the client implemented this same rule client-side;
    // doing it here keeps the contract identical and lets us extend
    // to actual transactions later.
    let mut deleted = 0usize;
    if copy_result.failed == 0 {
        let engine = s3.engine.load();
        for it in &req.items {
            match engine.delete(&req.source_bucket, &it.source_key).await {
                Ok(()) => deleted += 1,
                Err(e) => {
                    warn!(
                        "bulk move: delete source {}/{} failed: {}",
                        req.source_bucket, it.source_key, e
                    );
                    // Don't surface as a failure — the copy did succeed.
                    // The source object is just leftover.
                }
            }
        }
    }

    info!(
        "bulk move: src={} dst={}/{} succeeded={} failed={} deleted={}",
        req.source_bucket, req.dest_bucket, req.dest_prefix,
        copy_result.succeeded, copy_result.failed, deleted
    );
    Ok(Json(MoveResponse {
        succeeded: copy_result.succeeded,
        failed: copy_result.failed,
        deleted,
        failures: copy_result.failures,
    }))
}

// ---------------------------------------------------------------------------
// POST /_/api/admin/objects/delete
// ---------------------------------------------------------------------------

pub async fn bulk_delete(
    State(state): State<Arc<crate::api::admin::AdminState>>,
    Json(req): Json<DeleteRequest>,
) -> Result<Json<DeleteResponse>, (StatusCode, String)> {
    if req.keys.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "no keys to delete".into()));
    }
    if req.keys.len() > MAX_BULK_OBJECTS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "too many keys ({} > limit {})",
                req.keys.len(),
                MAX_BULK_OBJECTS
            ),
        ));
    }

    let engine = state.s3_state.engine.load();
    let mut deleted = 0usize;
    let mut failed = 0usize;
    let mut failures: Vec<DeleteFailure> = Vec::new();
    for key in &req.keys {
        match engine.delete(&req.bucket, key).await {
            Ok(()) => deleted += 1,
            Err(e) => {
                // Not-found is treated as deleted (idempotent).
                let s3_err: crate::api::S3Error = e.into();
                if matches!(s3_err, crate::api::S3Error::NoSuchKey(_)) {
                    deleted += 1;
                } else {
                    failed += 1;
                    if failures.len() < MAX_FAILURE_ENTRIES {
                        failures.push(DeleteFailure {
                            key: key.clone(),
                            error: format!("{}", s3_err),
                        });
                    }
                }
            }
        }
    }

    info!(
        "bulk delete: bucket={} deleted={} failed={}",
        req.bucket, deleted, failed
    );
    Ok(Json(DeleteResponse {
        deleted,
        failed,
        failures,
    }))
}

// ---------------------------------------------------------------------------
// GET /_/api/admin/objects/zip?keys=bucket/key1,bucket/key2,...
// ---------------------------------------------------------------------------
//
// In-memory zip assembly mirrors the previous client-side
// implementation (capped at 500 MB total uncompressed). Streaming the
// zip is a future improvement — for now we match the v1 contract so
// the migration is a drop-in replacement.

pub async fn download_zip(
    State(state): State<Arc<crate::api::admin::AdminState>>,
    Query(q): Query<ZipQuery>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let parsed: Vec<(String, String)> = q
        .keys
        .split(',')
        .filter(|s| !s.is_empty())
        .filter_map(|item| {
            // Each entry is `bucket/key`, split on the FIRST '/' so
            // keys with embedded slashes round-trip correctly.
            item.split_once('/').map(|(b, k)| (b.to_string(), k.to_string()))
        })
        .collect();
    if parsed.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "?keys must be a comma-separated list of bucket/key entries".into(),
        ));
    }
    if parsed.len() > MAX_BULK_OBJECTS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "too many keys ({} > limit {})",
                parsed.len(),
                MAX_BULK_OBJECTS
            ),
        ));
    }

    let engine = state.s3_state.engine.load();
    let mut name_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut bytes_total: u64 = 0;
    let mut entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(parsed.len());
    for (bucket, key) in &parsed {
        let basename = key.rsplit('/').next().unwrap_or(key).to_string();
        // Same de-duplication policy the browser used: when two entries
        // share a basename, the LATER one gets prefixed by its full key
        // (slashes-flattened) so it doesn't collide.
        let zip_name = if name_seen.insert(basename.clone()) {
            basename
        } else {
            key.replace('/', "_")
        };
        match engine.retrieve(bucket, key).await {
            Ok((data, _meta)) => {
                bytes_total += data.len() as u64;
                if bytes_total > MAX_ZIP_BYTES {
                    return Err((
                        StatusCode::PAYLOAD_TOO_LARGE,
                        format!(
                            "ZIP would exceed {} bytes; pick fewer files",
                            MAX_ZIP_BYTES
                        ),
                    ));
                }
                entries.push((zip_name, data));
            }
            Err(e) => {
                debug!("zip: skipping {}/{}: {}", bucket, key, e);
                // Skip individual failures to match the previous
                // browser semantics (silent skip; the user gets a
                // partial archive rather than an opaque error).
            }
        }
    }

    // Build an uncompressed zip via the existing `zip` crate. We emit
    // STORED entries (no compression) because the bodies are typically
    // already-compressed binaries; deflate buys little and costs CPU.
    let mut buf = std::io::Cursor::new(Vec::with_capacity((bytes_total + 4096) as usize));
    {
        use zip::write::SimpleFileOptions;
        use zip::ZipWriter;
        let mut zw = ZipWriter::new(&mut buf);
        let opts = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in &entries {
            if let Err(e) = std::io::Write::write_all(
                &mut {
                    let started = zw.start_file(name, opts);
                    if let Err(e) = started {
                        return Err((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("zip start_file: {}", e),
                        ));
                    }
                    &mut zw
                },
                data,
            ) {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("zip write: {}", e),
                ));
            }
        }
        if let Err(e) = zw.finish() {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("zip finish: {}", e),
            ));
        }
    }
    let body = buf.into_inner();

    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let filename = format!("deltaglider-{}.zip", date);
    let headers = [
        ("Content-Type", "application/zip"),
        // String literal won't outlive the response, but the format!
        // result is owned; assemble inline below.
    ];
    let cd = format!("attachment; filename=\"{}\"", filename);

    let mut resp = (StatusCode::OK, headers, body).into_response();
    resp.headers_mut()
        .insert("Content-Disposition", cd.parse().unwrap());
    Ok(resp)
}

// ---------------------------------------------------------------------------
// GET /_/api/admin/objects/list?bucket=...&prefix=...&recursive=true
// ---------------------------------------------------------------------------
//
// Resolves a folder selection into the absolute key list — the server
// equivalent of the browser's `listAllKeys`. Returns up to MAX_BULK_OBJECTS
// keys; truncated=true signals the client to narrow the selection.

#[derive(Debug, Deserialize)]
pub struct ListAllQuery {
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
}

#[derive(Debug, Serialize)]
pub struct ListAllResponse {
    pub keys: Vec<String>,
    pub truncated: bool,
}

pub async fn list_all(
    State(state): State<Arc<crate::api::admin::AdminState>>,
    Query(q): Query<ListAllQuery>,
) -> Result<Json<ListAllResponse>, (StatusCode, String)> {
    if q.prefix.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "prefix is required (refusing whole-bucket recursion)".into(),
        ));
    }

    let engine = state.s3_state.engine.load();
    let mut keys: Vec<String> = Vec::new();
    let mut continuation: Option<String> = None;
    let cap = 1000u32;
    loop {
        let page = engine
            .list_objects(&q.bucket, &q.prefix, None, cap, continuation.as_deref(), false)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;
        for (k, _) in &page.objects {
            keys.push(k.clone());
            if keys.len() >= MAX_BULK_OBJECTS {
                return Ok(Json(ListAllResponse {
                    keys,
                    truncated: true,
                }));
            }
        }
        if !page.is_truncated || page.next_continuation_token.is_none() {
            break;
        }
        continuation = page.next_continuation_token;
    }
    Ok(Json(ListAllResponse {
        keys,
        truncated: false,
    }))
}
