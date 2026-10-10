// SPDX-License-Identifier: BUSL-1.1

//! Usage scanner handlers: scan_usage, get_usage, migrate_legacy, plus the
//! O(1) bucket-usage COUNTER (`get_bucket_usage`) and its full-scan
//! `refresh_bucket_usage` (the dashboard bucket scan, joined or started).

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
) -> axum::response::Response {
    let bucket = match req.bucket.admit::<JsonError>(&state) {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let prefix = req.prefix.unwrap_or_default().into_string();
    let started =
        state
            .usage_scanner
            .enqueue_scan(bucket.into_string(), prefix, state.s3_state.clone());
    if started {
        (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"status": "scan_started"})),
        )
            .into_response()
    } else {
        (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"status": "scan_already_running"})),
        )
            .into_response()
    }
}

/// POST /_/api/admin/migrate — batch-migrate legacy reference objects.
/// Converts old-format references (original_name != "__reference__") to the new format.
/// This is a potentially long-running operation — runs synchronously and returns results.
pub async fn migrate_legacy(
    State(state): State<Arc<AdminState>>,
    AdminJson(req): AdminJson<MigrateRequest>,
) -> Result<Json<serde_json::Value>, AdminError<JsonError>> {
    let bucket = req.bucket.admit(&state)?;
    let engine = state.s3_state.engine.load();
    let (migrated, skipped, errors) = engine
        .migrate_legacy_references(&bucket)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;
    Ok(Json(serde_json::json!({
        "bucket": bucket,
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
    let bucket = match q.bucket.admit::<JsonError>(&state) {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let prefix = q.prefix.unwrap_or_default();
    match state.usage_scanner.get(&bucket, &prefix) {
        Some(entry) => (StatusCode::OK, Json(serde_json::json!(entry))).into_response(),
        None => {
            // "Not cached yet" is an expected state (no scan has run, or the
            // result expired), NOT an error. Returning 404 made the browser log
            // a red network error for a benign condition. Return 200 with
            // `cached: false` so the client can treat it as "no data yet"
            // without a console trace. (Mirrors the reasoning behind
            // delta_efficiency's 202 — 404/"not found" is the wrong semantic.)
            let scanning = state.usage_scanner.is_scanning(&bucket, &prefix);
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
    let bucket = bucket.admit(&state)?;
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

/// POST /_/api/admin/usage/refresh?bucket=X — scan the whole bucket and
/// overwrite the counter with ground truth. The scan is the dashboard
/// bucket scan (`bucket_scan.rs`): single-flight per bucket, cancellable
/// (`/diagnostics/scan/stop`) and in its own task, so a second Refresh, a
/// reload or a second admin joins the running scan instead of starting
/// another. Answers when the scan ends.
pub async fn refresh_bucket_usage(
    State(state): State<Arc<AdminState>>,
    AdminQuery(q): AdminQuery<UsageQuery>,
) -> Result<Json<serde_json::Value>, AdminError<JsonError>> {
    let bucket = q.bucket.admit(&state)?;
    if state.s3_state.bucket_usage.is_none() {
        return Ok(Json(
            serde_json::json!({"bucket": bucket, "disabled": true}),
        ));
    }
    let body = refresh_counter(&state.bucket_scanner, &state.s3_state, &bucket)
        .await
        .map_err(AdminError::internal)?;
    Ok(Json(body))
}

/// The Refresh: run (or join) the bucket scan, which overwrites the counter
/// when every size is known, and answer the counter row. An `estimated`
/// scan (a size it could not read) keeps the counter: the answer is the
/// kept row with `estimated: true` and the scan's numbers in `estimate`.
async fn refresh_counter(
    scanner: &super::bucket_scan::BucketScanner,
    s3_state: &Arc<crate::api::handlers::AppState>,
    bucket: &str,
) -> Result<serde_json::Value, String> {
    let usage = s3_state
        .bucket_usage
        .as_ref()
        .ok_or("the usage counter is disabled")?;
    let end = scanner
        .run_to_end(bucket.to_string(), s3_state.clone())
        .await;
    if let Some(e) = end.error {
        return Err(e);
    }
    let row = usage.read(bucket).map_err(|e| e.to_string())?;
    let mut body = usage_json(bucket, row);
    body["estimated"] = serde_json::Value::Bool(end.estimated);
    if end.estimated {
        body["estimate"] = serde_json::json!({
            "object_count": end.objects,
            "logical_bytes": end.original_bytes,
            "stored_bytes": end.stored_bytes,
        });
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::handlers::AppState;
    use crate::bucket_usage::BucketUsage;
    use crate::storage::FakeS3;
    use crate::usage_scanner::test_support::{fake_s3_engine, put_raw, scope_lists};

    /// An app state on a fake S3 with bucket `b` and a usage counter.
    async fn counted_state() -> (Arc<AppState>, Arc<FakeS3>, String, Arc<BucketUsage>) {
        let (engine, fake, endpoint) = fake_s3_engine().await;
        let usage = Arc::new(BucketUsage::in_memory().unwrap());
        let engine = engine.with_bucket_usage(Some(usage.clone()));
        let mut state = Arc::try_unwrap(AppState::for_tests(engine))
            .ok()
            .expect("a new state has one owner");
        state.bucket_usage = Some(usage.clone());
        (Arc::new(state), fake, endpoint, usage)
    }

    fn scanner() -> (
        tempfile::TempDir,
        Arc<super::super::bucket_scan::BucketScanner>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let scanner = super::super::bucket_scan::BucketScanner::load(dir.path().join("scans"));
        (dir, scanner)
    }

    /// Cockroach scan (ui.md D, limits #9): Refresh ran its own full scan in
    /// the request, so a second click (or a second admin) listed the whole
    /// bucket again next to the first.
    #[tokio::test]
    async fn two_refreshes_at_once_share_one_listing() {
        let (state, fake, _, _) = counted_state().await;
        let engine = state.engine.load_full();
        crate::deltaglider::store_deltas(&engine, "r", 3).await;
        engine
            .store("b", "r/notes.txt", b"plain", None, Default::default())
            .await
            .unwrap();
        let (_dir, scanner) = scanner();
        fake.set_delay_ms("LIST", 100);
        fake.clear();
        refresh_counter(&scanner, &state, "b").await.unwrap();
        let one = scope_lists(&fake).len();
        fake.clear();
        let (a, b) = tokio::join!(
            refresh_counter(&scanner, &state, "b"),
            refresh_counter(&scanner, &state, "b")
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        let two = scope_lists(&fake).len();
        assert_eq!(
            two, one,
            "two refreshes at once sent {two} LIST requests, one alone {one}"
        );
        assert_eq!(a["object_count"], 4, "{a}");
        assert_eq!(a["estimated"], false, "{a}");
        assert_eq!(a, b);
    }

    /// Cockroach scan (accounting.md "Refresh saves HEAD-failure stubs"):
    /// a Refresh whose HEADs failed wrote the stored sizes of the deltas as
    /// their logical sizes, over a counter that was roughly right.
    #[tokio::test]
    async fn a_refresh_that_cannot_read_sizes_keeps_the_counter() {
        let (state, fake, endpoint, usage) = counted_state().await;
        // Deltas this process never wrote: no listing-size cache entry and
        // no listing facts, so only a HEAD knows their logical size.
        for i in 0..5 {
            put_raw(&endpoint, &format!("x/build-{i}.zip.delta"), &[1u8; 100]).await;
        }
        usage.apply_delta("b", 5, 5_000_000, 500);
        usage.flush_pending();
        let before = usage.read("b").unwrap().unwrap();
        fake.fail("HEAD", "", 503, "SlowDown", u32::MAX);
        let (_dir, scanner) = scanner();
        let body = refresh_counter(&scanner, &state, "b").await;
        let after = usage.read("b").unwrap().unwrap();
        assert_eq!(
            (after.object_count, after.logical_bytes, after.stored_bytes),
            (
                before.object_count,
                before.logical_bytes,
                before.stored_bytes
            ),
            "a Refresh that could not read the sizes overwrote the counter: {body:?}"
        );
        let body = body.expect("an estimate is an answer, not an error");
        assert_eq!(body["estimated"], true, "{body}");
        assert_eq!(body["estimate"]["object_count"], 5, "{body}");
        assert_eq!(body["object_count"], 5, "the kept counter: {body}");
        assert_eq!(body["logical_bytes"], 5_000_000, "the kept counter: {body}");
    }
}
