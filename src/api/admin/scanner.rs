//! Usage scanner handlers: scan_usage, get_usage, migrate_legacy.

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;
use std::sync::Arc;

use super::AdminState;

#[derive(Deserialize)]
pub struct ScanUsageRequest {
    bucket: String,
    prefix: Option<String>,
}

#[derive(Deserialize)]
pub struct UsageQuery {
    bucket: String,
    prefix: Option<String>,
}

/// POST /_/api/admin/usage/scan — trigger a background usage scan.
pub async fn scan_usage(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<ScanUsageRequest>,
) -> impl IntoResponse {
    let prefix = req.prefix.unwrap_or_default();
    let started = state
        .usage_scanner
        .enqueue_scan(req.bucket, prefix, state.s3_state.clone());
    if started {
        (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"status": "scan_started"})),
        )
    } else {
        (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"status": "scan_already_running"})),
        )
    }
}

/// POST /_/api/admin/migrate — batch-migrate legacy reference objects.
/// Converts old-format references (original_name != "__reference__") to the new format.
/// This is a potentially long-running operation — runs synchronously and returns results.
pub async fn migrate_legacy(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<MigrateRequest>,
) -> impl IntoResponse {
    let engine = state.s3_state.engine.load();
    match engine.migrate_legacy_references(&req.bucket).await {
        Ok((migrated, skipped, errors)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "bucket": req.bucket,
                "migrated": migrated,
                "skipped": skipped,
                "errors": errors,
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct MigrateRequest {
    bucket: String,
}

/// GET /_/api/admin/usage?bucket=X&prefix=Y — return cached usage entry.
pub async fn get_usage(
    State(state): State<Arc<AdminState>>,
    axum::extract::Query(q): axum::extract::Query<UsageQuery>,
) -> impl IntoResponse {
    let prefix = q.prefix.unwrap_or_default();
    match state.usage_scanner.get(&q.bucket, &prefix) {
        Some(entry) => (StatusCode::OK, Json(serde_json::json!(entry))).into_response(),
        None => {
            let scanning = state.usage_scanner.is_scanning(&q.bucket, &prefix);
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_cached", "scanning": scanning})),
            )
                .into_response()
        }
    }
}
