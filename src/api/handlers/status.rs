//! Health-check and aggregate statistics handlers.

use super::{AppState, S3Error};
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Query parameters for /stats endpoint
#[derive(Debug, Deserialize, Default)]
pub struct StatsQuery {
    pub bucket: Option<String>,
}

/// Aggregate storage statistics
#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub total_objects: u64,
    pub total_original_size: u64,
    pub total_stored_size: u64,
    pub savings_percentage: f64,
}

/// Stats handler
/// GET /stats — aggregate stats across all buckets
/// GET /stats?bucket=NAME — stats for a specific bucket
pub async fn get_stats(
    State(state): State<Arc<AppState>>,
    Query(query): Query<StatsQuery>,
) -> Result<Json<StatsResponse>, S3Error> {
    let buckets_to_scan: Vec<String> = if let Some(ref bucket) = query.bucket {
        vec![bucket.clone()]
    } else {
        // Aggregate across all real buckets from storage
        state.engine.load().list_buckets().await.unwrap_or_default()
    };

    let mut total_objects: u64 = 0;
    let mut total_original_size: u64 = 0;
    let mut total_stored_size: u64 = 0;

    for bucket in &buckets_to_scan {
        let page = state
            .engine
            .load()
            .list_objects(bucket, "", None, u32::MAX, None)
            .await?;
        for (_key, meta) in &page.objects {
            total_objects += 1;
            total_original_size += meta.file_size;
            total_stored_size += meta.delta_size().unwrap_or(meta.file_size);
        }
    }

    let savings_percentage = if total_original_size > 0 {
        (1.0 - total_stored_size as f64 / total_original_size as f64) * 100.0
    } else {
        0.0
    };

    Ok(Json(StatsResponse {
        total_objects,
        total_original_size,
        total_stored_size,
        savings_percentage,
    }))
}

/// Health check response
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub backend: String,
    pub peak_rss_bytes: u64,
}

/// Return the process-lifetime peak RSS (high-water mark) in bytes.
/// Uses `getrusage(RUSAGE_SELF)` which captures even microsecond-lived allocations.
fn get_peak_rss_bytes() -> u64 {
    // SAFETY: `libc::getrusage` is a POSIX syscall that writes into a caller-provided
    // `rusage` struct. We zero-initialise it first, and the call is infallible for
    // RUSAGE_SELF. No aliasing or lifetime issues — `usage` is a local stack variable.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) == 0 {
            let ru_maxrss = usage.ru_maxrss as u64;
            // macOS reports ru_maxrss in bytes; Linux reports in KB
            if cfg!(target_os = "macos") {
                ru_maxrss
            } else {
                ru_maxrss * 1024
            }
        } else {
            0
        }
    }
}

/// S3 root HEAD handler — connection probe used by Cyberduck and other S3 clients
/// HEAD /
pub async fn head_root() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("server", "DeltaGliderProxy")
        .header("x-amz-request-id", "0")
        .body(Body::empty())
        .unwrap()
}

/// Health check handler
/// GET /health
pub async fn health_check() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "healthy".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        backend: "ready".to_string(),
        peak_rss_bytes: get_peak_rss_bytes(),
    })
}
