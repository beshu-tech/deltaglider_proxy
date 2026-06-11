// SPDX-License-Identifier: GPL-3.0-only

//! Admin API for one-off maintenance jobs (bucket re-encryption).
//!
//! Routes:
//!
//! - `POST /_/api/admin/maintenance/reencrypt` `{buckets: [..]}` — create
//!   queued jobs (admin tier). Validation per bucket: must exist, must
//!   route to a backend whose encryption mode the job supports
//!   (none / aes256-gcm-proxy), must not already have an active job.
//!   The write gate arms at CREATION (not at worker start) so there is
//!   no window where a write slips in between create and claim.
//! - `GET  /_/api/admin/maintenance` — recent jobs overview (admin tier).
//! - `POST /_/api/admin/maintenance/jobs/:id/cancel` (admin tier).
//! - `GET  /_/api/admin/maintenance/bucket/:bucket` — the bucket's active
//!   job, if any. Registered on the SESSION-LIGHT tier (S3BrowserLift
//!   included) so non-admin browser users see busy state + progress; the
//!   response carries only status/phase/counts — no config detail.

use super::AdminState;
use crate::maintenance::migrate::{pick_transient_key, MigrateParams};
use crate::maintenance::store::{current_unix_seconds, CancelOutcome, MaintenanceJob};
use crate::maintenance::{progress_percent, resolve_desired};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::info;

/// Public projection of a job row. Safe for the session-light tier.
#[derive(Debug, Serialize)]
pub struct MaintenanceJobView {
    pub id: i64,
    pub kind: String,
    pub bucket: String,
    pub status: String,
    pub phase: String,
    pub objects_total: Option<i64>,
    pub objects_done: i64,
    pub objects_skipped: i64,
    pub objects_failed: i64,
    pub bytes_done: i64,
    /// 0-99 while running (`None` while counting); the UI shows 100 on
    /// `completed`.
    pub percent: Option<u8>,
    pub last_error: Option<String>,
    pub triggered_by: Option<String>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

impl From<MaintenanceJob> for MaintenanceJobView {
    fn from(j: MaintenanceJob) -> Self {
        let percent = match j.status.as_str() {
            "completed" => Some(100),
            _ => progress_percent(&j.phase, j.objects_total, j.objects_done, j.objects_skipped),
        };
        Self {
            id: j.id,
            kind: j.kind,
            bucket: j.bucket,
            status: j.status,
            phase: j.phase,
            objects_total: j.objects_total,
            objects_done: j.objects_done,
            objects_skipped: j.objects_skipped,
            objects_failed: j.objects_failed,
            bytes_done: j.bytes_done,
            percent,
            last_error: j.last_error,
            triggered_by: j.triggered_by,
            created_at: j.created_at,
            started_at: j.started_at,
            finished_at: j.finished_at,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ReencryptRequest {
    pub buckets: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ReencryptStarted {
    pub bucket: String,
    pub job_id: i64,
}

#[derive(Debug, Serialize)]
pub struct ReencryptError {
    pub bucket: String,
    pub error: String,
}

#[derive(Debug, Serialize)]
pub struct ReencryptResponse {
    pub started: Vec<ReencryptStarted>,
    pub errors: Vec<ReencryptError>,
}

/// POST /_/api/admin/maintenance/reencrypt
pub async fn start_reencrypt(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Json(req): Json<ReencryptRequest>,
) -> Result<Json<ReencryptResponse>, (StatusCode, String)> {
    if req.buckets.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "no buckets given".into()));
    }
    if req.buckets.len() > 100 {
        return Err((StatusCode::BAD_REQUEST, "too many buckets (max 100)".into()));
    }
    let db = state
        .config_db
        .as_ref()
        .ok_or((StatusCode::NOT_FOUND, "config DB unavailable".to_string()))?;

    // Real-bucket set from the engine (authoritative across backends).
    let engine = state.s3_state.engine.load().clone();
    let real: std::collections::HashSet<String> = engine
        .list_bucket_origins()
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to list buckets: {e}"),
            )
        })?
        .into_iter()
        .map(|b| b.name.to_ascii_lowercase())
        .collect();

    let cfg = state.config.read().await;
    let mut started = Vec::new();
    let mut errors = Vec::new();
    for bucket in &req.buckets {
        let key = bucket.to_ascii_lowercase();
        if !real.contains(&key) {
            errors.push(ReencryptError {
                bucket: bucket.clone(),
                error: "bucket not found".into(),
            });
            continue;
        }
        if let Err(reason) = resolve_desired(&cfg, &key) {
            errors.push(ReencryptError {
                bucket: bucket.clone(),
                error: reason,
            });
            continue;
        }
        let created = {
            let db = db.lock().await;
            db.maintenance_create_job(
                "reencrypt",
                &key,
                "counting",
                None,
                "admin",
                current_unix_seconds(),
            )
        };
        match created {
            Ok(Some(job_id)) => {
                // Gate from CREATION: no create→claim window for writes.
                state.s3_state.maintenance_gate.set_busy(&key);
                started.push(ReencryptStarted {
                    bucket: bucket.clone(),
                    job_id,
                });
            }
            Ok(None) => errors.push(ReencryptError {
                bucket: bucket.clone(),
                error: "a maintenance job is already active for this bucket".into(),
            }),
            Err(e) => errors.push(ReencryptError {
                bucket: bucket.clone(),
                error: format!("failed to create job: {e}"),
            }),
        }
    }
    drop(cfg);

    if !started.is_empty() {
        state.s3_state.maintenance_notify.notify_one();
        let names: Vec<&str> = started.iter().map(|s| s.bucket.as_str()).collect();
        info!("maintenance: re-encrypt requested for {:?}", names);
        super::audit_log(
            "maintenance_reencrypt_requested",
            "admin",
            &names.join(","),
            &headers,
        );
    }

    Ok(Json(ReencryptResponse { started, errors }))
}

#[derive(Debug, Deserialize)]
pub struct MigrateBucketRequest {
    pub target_backend: String,
    /// Delete the source objects after the flip. Default false — the safe
    /// path leaves the source copy for the operator to remove later.
    #[serde(default)]
    pub delete_source: bool,
}

/// POST /_/api/admin/buckets/:bucket/migrate — create a durable migrate
/// job (replaces the old synchronous in-handler migration: that version
/// had no progress, no resume, and no write gate — a client write racing
/// the copy produced a stale object on the destination post-flip).
pub async fn start_migrate(
    State(state): State<Arc<AdminState>>,
    Path(bucket): Path<String>,
    headers: HeaderMap,
    Json(body): Json<MigrateBucketRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    let bucket = bucket.trim().to_string();
    let bucket_key = bucket.to_ascii_lowercase();
    let target_backend = body.target_backend.trim().to_string();
    if bucket.is_empty() || target_backend.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "bucket and target_backend are required".into(),
        ));
    }
    let db = state
        .config_db
        .as_ref()
        .ok_or((StatusCode::NOT_FOUND, "config DB unavailable".to_string()))?;

    // The bucket must actually exist (the old handler skipped this check).
    let engine = state.s3_state.engine.load().clone();
    let exists = engine
        .list_bucket_origins()
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to list buckets: {e}"),
            )
        })?
        .into_iter()
        .any(|b| b.name.eq_ignore_ascii_case(&bucket_key));
    if !exists {
        return Err((
            StatusCode::NOT_FOUND,
            format!("bucket '{bucket}' not found"),
        ));
    }

    // Resolve source backend + validate target + pick the transient key
    // under one config read.
    let params = {
        let cfg = state.config.read().await;
        if !cfg.backends.is_empty() && !cfg.backends.iter().any(|b| b.name == target_backend) {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("Unknown target backend '{target_backend}'"),
            ));
        }
        let from_backend = cfg
            .buckets
            .get(&bucket_key)
            .and_then(|p| p.backend.clone())
            .or_else(|| cfg.default_backend.clone())
            .unwrap_or_else(|| "default".to_string());
        if from_backend == target_backend {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("Bucket '{bucket}' is already on backend '{target_backend}'"),
            ));
        }
        MigrateParams {
            target_backend,
            delete_source: body.delete_source,
            transient_key: pick_transient_key(&bucket_key, &|k| cfg.buckets.contains_key(k)),
            from_backend,
        }
    };

    let params_json = serde_json::to_string(&params)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let created = {
        let db = db.lock().await;
        db.maintenance_create_job(
            "migrate",
            &bucket_key,
            "stage",
            Some(&params_json),
            "admin",
            current_unix_seconds(),
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    let Some(job_id) = created else {
        return Err((
            StatusCode::CONFLICT,
            format!("a maintenance job is already active for bucket '{bucket}'"),
        ));
    };

    // Gate WRITES from creation — the source write-set freezes through the
    // flip (this is what makes migrate race-free, unlike the old handler).
    state.s3_state.maintenance_gate.set_busy(&bucket_key);
    state.s3_state.maintenance_notify.notify_one();
    info!(
        "maintenance: migrate requested for '{}' → '{}' (job #{job_id})",
        bucket, params.target_backend
    );
    super::audit_log(
        "maintenance_migrate_requested",
        "admin",
        &format!("{bucket}->{}", params.target_backend),
        &headers,
    );

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "job_id": job_id,
            "id": format!("maintenance:{job_id}"),
            "bucket": bucket,
            "from_backend": params.from_backend,
            "to_backend": params.target_backend,
        })),
    ))
}

/// POST /_/api/admin/maintenance/jobs/:id/cancel
pub async fn cancel_job(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let db = state
        .config_db
        .as_ref()
        .ok_or((StatusCode::NOT_FOUND, "config DB unavailable".to_string()))?;
    let (outcome, bucket) = {
        let db = db.lock().await;
        let bucket = db
            .maintenance_job_by_id(id)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map(|j| j.bucket);
        let outcome = db
            .maintenance_request_cancel(id)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        (outcome, bucket)
    };
    match outcome {
        CancelOutcome::CancelledImmediately => {
            // Queued job never ran: release the gate now.
            if let Some(b) = &bucket {
                state.s3_state.maintenance_gate.clear(b);
            }
            super::audit_log(
                "maintenance_job_cancel",
                "admin",
                &format!("job:{id}"),
                &headers,
            );
            Ok(Json(serde_json::json!({ "status": "cancelled" })))
        }
        CancelOutcome::CancelRequested => {
            // Worker settles it (and releases the gate) at the next page.
            super::audit_log(
                "maintenance_job_cancel",
                "admin",
                &format!("job:{id}"),
                &headers,
            );
            Ok(Json(serde_json::json!({ "status": "cancelling" })))
        }
        CancelOutcome::NotActive => Err((
            StatusCode::CONFLICT,
            "job is not active (already finished or unknown id)".into(),
        )),
    }
}

/// GET /_/api/admin/maintenance/bucket/:bucket — session-light tier.
pub async fn bucket_status(
    State(state): State<Arc<AdminState>>,
    Path(bucket): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let job = super::with_config_db(&state, "read bucket maintenance status", |db| {
        db.maintenance_active_job_for_bucket(&bucket.to_ascii_lowercase())
    })
    .await?;
    Ok(Json(serde_json::json!({
        "active": job.map(MaintenanceJobView::from)
    })))
}
