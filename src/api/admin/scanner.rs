// SPDX-License-Identifier: BUSL-1.1

//! Usage scanner handlers: scan_usage, get_usage, migrate_legacy, plus the
//! O(1) bucket-usage COUNTER (`get_bucket_usage`) and its full-scan
//! `refresh_bucket_usage`.

use super::path_guard::{AdminBucket, AdminObjectPath};
use crate::api::admin::extract::{AdminJson, AdminQuery};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use std::sync::Arc;

use super::{AdminError, AdminState, JsonError};
use crate::deltaglider::savings::SavingsTotals;

#[derive(Deserialize)]
pub struct ScanUsageRequest {
    bucket: AdminBucket,
    prefix: Option<AdminObjectPath>,
}

#[derive(Deserialize)]
pub struct UsageQuery {
    bucket: AdminBucket,
    prefix: Option<AdminObjectPath>,
}

/// POST /_/api/admin/usage/scan — trigger a background usage scan.
pub async fn scan_usage(
    State(state): State<Arc<AdminState>>,
    AdminJson(req): AdminJson<ScanUsageRequest>,
) -> impl IntoResponse {
    let prefix = req.prefix.unwrap_or_default().into_string();
    let started =
        state
            .usage_scanner
            .enqueue_scan(req.bucket.into_string(), prefix, state.s3_state.clone());
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
    AdminJson(req): AdminJson<MigrateRequest>,
) -> Result<Json<serde_json::Value>, AdminError<JsonError>> {
    let engine = state.s3_state.engine.load();
    let (migrated, skipped, errors) = engine
        .migrate_legacy_references(&req.bucket)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;
    Ok(Json(serde_json::json!({
        "bucket": req.bucket,
        "migrated": migrated,
        "skipped": skipped,
        "errors": errors,
    })))
}

#[derive(Deserialize)]
pub struct MigrateRequest {
    bucket: AdminBucket,
}

/// GET /_/api/admin/usage?bucket=X&prefix=Y — return cached usage entry.
pub async fn get_usage(
    State(state): State<Arc<AdminState>>,
    AdminQuery(q): AdminQuery<UsageQuery>,
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
            serde_json::json!({
                "bucket": bucket,
                "object_count": r.object_count,
                "logical_bytes": r.logical_bytes,
                "stored_bytes": r.stored_bytes,
                "savings_percentage": r.savings_pct(),
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
    Path(bucket): Path<AdminBucket>,
) -> Result<Json<serde_json::Value>, AdminError<JsonError>> {
    let Some(usage) = state.s3_state.bucket_usage.as_ref() else {
        return Ok(Json(
            serde_json::json!({"bucket": bucket, "disabled": true}),
        ));
    };
    let row = usage
        .read(&bucket)
        .map_err(|e| AdminError::internal(e.to_string()))?;
    Ok(Json(usage_json(&bucket, row)))
}

/// POST /_/api/admin/usage/refresh?bucket=X — run an UNCAPPED full scan and
/// overwrite the counter with ground truth. The only O(n) path left.
pub async fn refresh_bucket_usage(
    State(state): State<Arc<AdminState>>,
    AdminQuery(q): AdminQuery<UsageQuery>,
) -> Result<Json<serde_json::Value>, AdminError<JsonError>> {
    let Some(usage) = state.s3_state.bucket_usage.as_ref() else {
        return Ok(Json(
            serde_json::json!({"bucket": q.bucket, "disabled": true}),
        ));
    };
    // The ticket marks the scan start: a write that lands while the scan
    // runs is kept on top of the scan result (H14b).
    let ticket = usage.begin_scan(&q.bucket);
    let totals = scan_bucket_totals(&state.s3_state, &q.bucket)
        .await
        .map_err(AdminError::internal)?;
    let now = crate::replication::current_unix_seconds();
    usage
        .overwrite_from_scan(ticket, &totals, now)
        .map_err(|e| AdminError::internal(e.to_string()))?;
    let row = usage
        .read(&q.bucket)
        .map_err(|e| AdminError::internal(e.to_string()))?;
    Ok(Json(usage_json(&q.bucket, row)))
}

/// Full, UNCAPPED bucket scan -> `SavingsTotals` (logical + stored + counts,
/// references included). This is the authoritative ground truth the Refresh
/// endpoint writes into the counter. Unlike the savings/stats panels it has NO
/// object cap — Refresh is explicit and may be slow on huge buckets by design.
async fn scan_bucket_totals(
    s3_state: &Arc<crate::api::handlers::AppState>,
    bucket: &str,
) -> Result<SavingsTotals, String> {
    use super::savings::{scan_totals, TotalsScanError, TotalsScanOpts};
    let opts = TotalsScanOpts {
        prefix: "",
        object_cap: None,
        ref_limit: None,
        cancel: None,
    };
    match scan_totals(s3_state, bucket, opts, |_, _, _| {}).await {
        Ok((totals, _)) => Ok(totals),
        Err(TotalsScanError::Failed(e)) => Err(e),
        Err(TotalsScanError::Cancelled) => Err("scan cancelled".into()),
    }
}
