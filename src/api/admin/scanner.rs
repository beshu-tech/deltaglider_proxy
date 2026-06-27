// SPDX-License-Identifier: GPL-3.0-only

//! Usage scanner handlers: scan_usage, get_usage, migrate_legacy, plus the
//! O(1) bucket-usage COUNTER (`get_bucket_usage`) and its full-scan
//! `refresh_bucket_usage`.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use std::sync::Arc;

use super::AdminState;
use crate::deltaglider::savings::SavingsTotals;

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
            // "Not cached yet" is an expected state (no scan has run, or the
            // result expired), NOT an error. Returning 404 made the browser log
            // a red network error for a benign condition. Return 200 with
            // `cached: false` so the client can treat it as "no data yet"
            // without a console trace. (Mirrors the reasoning behind
            // delta_efficiency's 202 — 404/"not found" is the wrong semantic.)
            let scanning = state.usage_scanner.is_scanning(&q.bucket, &prefix);
            (
                StatusCode::OK,
                Json(serde_json::json!({"cached": false, "scanning": scanning})),
            )
                .into_response()
        }
    }
}

/// JSON body of the O(1) bucket-usage counter.
fn usage_json(bucket: &str, row: Option<crate::bucket_usage::BucketUsageRow>) -> serde_json::Value {
    match row {
        Some(r) => {
            // savings% from the counter's logical vs stored (same math as the scan).
            let savings = if r.logical_bytes > 0 {
                let pct = (1.0 - (r.stored_bytes as f64 / r.logical_bytes as f64)) * 100.0;
                Some(pct.clamp(0.0, 99.99))
            } else {
                None
            };
            serde_json::json!({
                "bucket": bucket,
                "object_count": r.object_count,
                "logical_bytes": r.logical_bytes,
                "stored_bytes": r.stored_bytes,
                "savings_percentage": savings,
                "last_scan_at": r.last_scan_at,
                "never_scanned": r.last_scan_at.is_none(),
            })
        }
        // No row yet: report zeros + never_scanned so the UI nudges a Refresh.
        None => serde_json::json!({
            "bucket": bucket,
            "object_count": 0,
            "logical_bytes": 0,
            "stored_bytes": 0,
            "savings_percentage": serde_json::Value::Null,
            "last_scan_at": serde_json::Value::Null,
            "never_scanned": true,
        }),
    }
}

/// GET /_/api/admin/usage/bucket/:bucket — O(1) counter read (no scan).
pub async fn get_bucket_usage(
    State(state): State<Arc<AdminState>>,
    Path(bucket): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = crate::security::validate_bucket_name(&bucket) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("invalid bucket name: {}", e)})),
        )
            .into_response();
    }
    let Some(usage) = state.s3_state.bucket_usage.as_ref() else {
        return (
            StatusCode::OK,
            Json(serde_json::json!({"bucket": bucket, "disabled": true})),
        )
            .into_response();
    };
    match usage.read(&bucket) {
        Ok(row) => (StatusCode::OK, Json(usage_json(&bucket, row))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// POST /_/api/admin/usage/refresh?bucket=X — run an UNCAPPED full scan and
/// overwrite the counter with ground truth. The only O(n) path left.
pub async fn refresh_bucket_usage(
    State(state): State<Arc<AdminState>>,
    axum::extract::Query(q): axum::extract::Query<UsageQuery>,
) -> impl IntoResponse {
    if let Err(e) = crate::security::validate_bucket_name(&q.bucket) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("invalid bucket name: {}", e)})),
        )
            .into_response();
    }
    let Some(usage) = state.s3_state.bucket_usage.as_ref() else {
        return (
            StatusCode::OK,
            Json(serde_json::json!({"bucket": q.bucket, "disabled": true})),
        )
            .into_response();
    };
    let totals = match scan_bucket_totals(&state.s3_state, &q.bucket).await {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e})),
            )
                .into_response()
        }
    };
    let now = crate::replication::current_unix_seconds();
    if let Err(e) = usage.overwrite_from_scan(&q.bucket, &totals, now) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response();
    }
    match usage.read(&q.bucket) {
        Ok(row) => (StatusCode::OK, Json(usage_json(&q.bucket, row))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Full, UNCAPPED bucket scan -> `SavingsTotals` (logical + stored + counts,
/// references included). This is the authoritative ground truth the Refresh
/// endpoint writes into the counter. Unlike the savings/stats panels it has NO
/// object cap — Refresh is explicit and may be slow on huge buckets by design.
async fn scan_bucket_totals(
    s3_state: &Arc<crate::api::handlers::AppState>,
    bucket: &str,
) -> Result<SavingsTotals, String> {
    let engine = s3_state.engine.load();
    let mut totals = SavingsTotals::default();
    let mut continuation: Option<String> = None;
    loop {
        let page = engine
            .list_objects(bucket, "", None, 1000, continuation.as_deref(), true)
            .await
            .map_err(|e| e.to_string())?;
        for (_key, meta) in &page.objects {
            totals.accumulate(meta);
        }
        if !page.is_truncated {
            break;
        }
        continuation = page.next_continuation_token;
        if continuation.is_none() {
            break;
        }
    }
    // Fold in every reference baseline (no cap) so stored_bytes is exact.
    let ref_scan = engine
        .list_deltaspace_references(bucket, "", None)
        .await
        .map_err(|e| e.to_string())?;
    for meta in &ref_scan.references {
        totals.accumulate(meta);
    }
    Ok(totals)
}
