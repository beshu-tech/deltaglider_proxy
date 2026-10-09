// SPDX-License-Identifier: BUSL-1.1

//! Admin API for one-off maintenance jobs (bucket re-encryption).
//!
//! Routes:
//!
//! - `POST /_/api/admin/jobs/reencrypt` `{buckets: [..]}` — create
//!   queued jobs (admin tier). Validation per bucket: must exist, must
//!   route to a backend whose encryption mode the job supports
//!   (none / aes256-gcm-proxy), must not already have an active job.
//!   The write gate arms at CREATION (not at worker start) so there is
//!   no window where a write slips in between create and claim.
//! - `POST /_/api/admin/jobs/backfill-metadata`
//!   `{buckets: [..], refresh_last_modified?: bool}` — create
//!   metadata-backfill jobs (admin tier). Same gating and one-active-job
//!   rule; no config-mode precondition.
//! - `POST /_/api/admin/jobs/maintenance:<id>/cancel` (admin tier, via jobs.rs).
//! - `GET  /_/api/admin/jobs/bucket/:bucket` — the bucket's active
//!   job, if any. Registered on the SESSION-LIGHT tier (S3BrowserLift
//!   included) so non-admin browser users see busy state + progress; the
//!   response carries only status/phase/counts — no config detail.

use super::{AdminError, AdminState, Bare};
use crate::api::admin::extract::AdminJson;
use crate::maintenance::migrate::{pick_transient_key, MigrateParams, MigrateTarget};
use crate::maintenance::store::{current_unix_seconds, CancelOutcome, MaintenanceJob};
use crate::maintenance::{display_percent, resolve_desired};
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
    /// 0-99 while running (`None` while counting); 100 on `completed`.
    pub percent: Option<u8>,
    // NO `last_error` here: this view is readable by non-admin browser
    // sessions, and worker errors can embed object keys + raw backend
    // error strings. The busy banner needs status/phase/counts only;
    // admins read errors via the admin-tier jobs API.
    pub triggered_by: Option<String>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

impl From<MaintenanceJob> for MaintenanceJobView {
    fn from(j: MaintenanceJob) -> Self {
        let percent = display_percent(&j);
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
            triggered_by: j.triggered_by,
            created_at: j.created_at,
            started_at: j.started_at,
            finished_at: j.finished_at,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ReencryptRequest {
    pub buckets: Vec<super::path_guard::AdminBucket>,
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

/// The request gate that the reencrypt and backfill starts share: 1 to
/// 100 buckets, a config DB, and the real bucket set from the engine
/// (authoritative across backends), lowercased.
async fn check_job_request<'a>(
    state: &'a AdminState,
    buckets: &[super::path_guard::AdminBucket],
) -> Result<
    (
        &'a Arc<tokio::sync::Mutex<crate::config_db::ConfigDb>>,
        std::collections::HashSet<String>,
    ),
    AdminError,
> {
    if buckets.is_empty() {
        return Err(AdminError::invalid("no buckets given"));
    }
    if buckets.len() > 100 {
        return Err(AdminError::invalid("too many buckets (max 100)"));
    }
    for bucket in buckets {
        super::reject_reserved_bucket(state, bucket.as_str())?;
    }
    let db = state
        .config_db
        .as_ref()
        .ok_or_else(AdminError::no_config_db)?;
    let engine = state.s3_state.engine.load().clone();
    let real = engine
        .list_bucket_origins()
        .await
        .map_err(|e| AdminError::internal(format!("failed to list buckets: {e}")))?
        .into_iter()
        .map(|b| b.name.to_ascii_lowercase())
        .collect();
    Ok((db, real))
}

/// POST /_/api/admin/jobs/reencrypt
pub async fn start_reencrypt(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    AdminJson(req): AdminJson<ReencryptRequest>,
) -> Result<Json<ReencryptResponse>, AdminError> {
    let (db, real) = check_job_request(&state, &req.buckets).await?;

    let cfg = state.config.read().await;
    let mut started = Vec::new();
    let mut errors = Vec::new();
    for bucket in &req.buckets {
        let key = bucket.to_ascii_lowercase();
        if !real.contains(&key) {
            errors.push(ReencryptError {
                bucket: bucket.to_string(),
                error: "bucket not found".into(),
            });
            continue;
        }
        if let Err(reason) = resolve_desired(&cfg, &key) {
            errors.push(ReencryptError {
                bucket: bucket.to_string(),
                error: reason,
            });
            continue;
        }
        let created = {
            let db = db.lock().await;
            let created = db.maintenance_create_job(
                "reencrypt",
                &key,
                "counting",
                None,
                "admin",
                current_unix_seconds(),
            );
            // Gate from CREATION: no create→claim window for writes.
            state.s3_state.maintenance_gate.sync_from(&db);
            created
        };
        match created {
            Ok(Some(job_id)) => {
                started.push(ReencryptStarted {
                    bucket: bucket.to_string(),
                    job_id,
                });
            }
            Ok(None) => errors.push(ReencryptError {
                bucket: bucket.to_string(),
                error: "a maintenance job is already active for this bucket".into(),
            }),
            Err(e) => errors.push(ReencryptError {
                bucket: bucket.to_string(),
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
pub struct BackfillRequest {
    pub buckets: Vec<super::path_guard::AdminBucket>,
    /// `false` (default): the LastModified the proxy serves is unchanged
    /// (`dg-created-at` pinned to the pre-job value). `true`: backfilled
    /// objects read as modified at rewrite time.
    #[serde(default)]
    pub refresh_last_modified: bool,
}

/// POST /_/api/admin/jobs/backfill-metadata — create metadata-backfill
/// jobs. Same shape and gating as re-encrypt: per-bucket validation, the
/// write gate arms at CREATION, one active job per bucket. No
/// config-mode precondition — the job derives everything from the
/// objects themselves.
pub async fn start_backfill(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    AdminJson(req): AdminJson<BackfillRequest>,
) -> Result<Json<ReencryptResponse>, AdminError> {
    let (db, real) = check_job_request(&state, &req.buckets).await?;

    let params = serde_json::to_string(&crate::maintenance::backfill::BackfillParams {
        refresh_last_modified: req.refresh_last_modified,
    })
    .expect("params serialize");

    let mut started = Vec::new();
    let mut errors = Vec::new();
    for bucket in &req.buckets {
        let key = bucket.to_ascii_lowercase();
        if !real.contains(&key) {
            errors.push(ReencryptError {
                bucket: bucket.to_string(),
                error: "bucket not found".into(),
            });
            continue;
        }
        let created = {
            let db = db.lock().await;
            let created = db.maintenance_create_job(
                crate::maintenance::backfill::KIND,
                &key,
                "counting",
                Some(&params),
                "admin",
                current_unix_seconds(),
            );
            // Gate from CREATION: no create→claim window for writes.
            state.s3_state.maintenance_gate.sync_from(&db);
            created
        };
        match created {
            Ok(Some(job_id)) => {
                started.push(ReencryptStarted {
                    bucket: bucket.to_string(),
                    job_id,
                });
            }
            Ok(None) => errors.push(ReencryptError {
                bucket: bucket.to_string(),
                error: "a maintenance job is already active for this bucket".into(),
            }),
            Err(e) => errors.push(ReencryptError {
                bucket: bucket.to_string(),
                error: format!("failed to create job: {e}"),
            }),
        }
    }

    if !started.is_empty() {
        state.s3_state.maintenance_notify.notify_one();
        let names: Vec<&str> = started.iter().map(|s| s.bucket.as_str()).collect();
        info!("maintenance: metadata backfill requested for {:?}", names);
        super::audit_log(
            "maintenance_backfill_requested",
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
    /// `empty` (default): refuse a destination that already holds objects.
    /// `mirror`: make the destination an exact copy (extras deleted).
    #[serde(default)]
    pub target: MigrateTarget,
}

/// POST /_/api/admin/buckets/:bucket/migrate — create a durable migrate
/// job (replaces the old synchronous in-handler migration: that version
/// had no progress, no resume, and no write gate — a client write racing
/// the copy produced a stale object on the destination post-flip).
pub async fn start_migrate(
    State(state): State<Arc<AdminState>>,
    Path(bucket): Path<String>,
    headers: HeaderMap,
    AdminJson(body): AdminJson<MigrateBucketRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), AdminError> {
    let bucket = bucket.trim().to_string();
    let bucket_key = bucket.to_ascii_lowercase();
    let target_backend = body.target_backend.trim().to_string();
    if bucket.is_empty() || target_backend.is_empty() {
        return Err(AdminError::invalid(
            "bucket and target_backend are required",
        ));
    }
    super::path_guard::check_bucket(&bucket).map_err(AdminError::invalid)?;
    super::reject_reserved_bucket(&state, &bucket)?;
    let db = state
        .config_db
        .as_ref()
        .ok_or_else(AdminError::no_config_db)?;

    // The bucket must actually exist (the old handler skipped this check).
    let engine = state.s3_state.engine.load().clone();
    let exists = engine
        .list_bucket_origins()
        .await
        .map_err(|e| AdminError::internal(format!("failed to list buckets: {e}")))?
        .into_iter()
        .any(|b| b.name.eq_ignore_ascii_case(&bucket_key));
    if !exists {
        return Err(AdminError::not_found(format!(
            "bucket '{bucket}' not found"
        )));
    }

    // Resolve source backend + validate target + pick the transient key
    // under one config read.
    let params = {
        let cfg = state.config.read().await;
        if let Some(reason) =
            crate::maintenance::migrate::multi_instance_refusal(cfg.config_sync_bucket.as_deref())
        {
            return Err(AdminError::conflict(reason));
        }
        if cfg.backend_by_name(&target_backend).is_none() {
            return Err(AdminError::invalid(format!(
                "Unknown target backend '{target_backend}'"
            )));
        }
        let from_backend = cfg
            .effective_backend_for_bucket(&bucket_key)
            .map(|(name, _)| name)
            .unwrap_or_else(|| cfg.default_backend_name());
        if from_backend == target_backend {
            return Err(AdminError::invalid(format!(
                "Bucket '{bucket}' is already on backend '{target_backend}'"
            )));
        }
        MigrateParams {
            target_backend,
            delete_source: body.delete_source,
            transient_key: pick_transient_key(&bucket_key, &|k| cfg.buckets.contains_key(k)),
            from_backend,
            target: body.target,
        }
    };

    // Capability boundary: the migrate flip persists routing WITHOUT the
    // hot-apply gate, so enforce the non-CAS refusal here (else the next boot
    // exit(1)s on the persisted config — a crash loop).
    {
        // Clone-and-drop: the gate may probe for up to 15s — never hold the
        // config read lock across that await (write-lock starvation).
        let cfg = state.config.read().await.clone();
        crate::coordination::capability::migrate_target_capability_gate(
            &cfg,
            &state.s3_state.backend_capabilities,
            &bucket_key,
            &params.target_backend,
        )
        .await
        .map_err(AdminError::conflict)?;
    }

    let params_json =
        serde_json::to_string(&params).map_err(|e| AdminError::internal(e.to_string()))?;
    let created = {
        let db = db.lock().await;
        let created = db.maintenance_create_job(
            "migrate",
            &bucket_key,
            "stage",
            Some(&params_json),
            "admin",
            current_unix_seconds(),
        )?;
        // Gate WRITES from creation — the source write-set freezes through
        // the flip (this is what makes migrate race-free, unlike the old
        // handler). The transient staging route is gated too: admin
        // copy/move endpoints could otherwise write through it mid-copy.
        state.s3_state.maintenance_gate.sync_from(&db);
        created
    };
    let Some(job_id) = created else {
        return Err(AdminError::conflict(format!(
            "a maintenance job is already active for bucket '{bucket}'"
        )));
    };

    state.s3_state.maintenance_notify.notify_one();
    info!(
        "maintenance: migrate requested for '{}' → '{}' (job #{job_id})",
        bucket, params.target_backend
    );
    super::audit_log(
        "maintenance_migrate_requested",
        "admin",
        &format!(
            "{bucket}->{} (target: {})",
            params.target_backend,
            params.target.as_str()
        ),
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
            "target": params.target.as_str(),
        })),
    ))
}

/// POST /_/api/admin/jobs/maintenance:<id>/cancel (routed via jobs.rs)
pub async fn cancel_job(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, AdminError> {
    let db = state
        .config_db
        .as_ref()
        .ok_or_else(AdminError::no_config_db)?;
    let (outcome, job) = {
        let db = db.lock().await;
        let job = db.maintenance_job_by_id(id)?;
        let outcome = db.maintenance_request_cancel(id)?;
        // A queued job that never ran settles now, and its gate (bucket AND,
        // for migrate, the staging route) goes with the row.
        state.s3_state.maintenance_gate.sync_from(&db);
        (outcome, job)
    };
    match outcome {
        CancelOutcome::CancelledImmediately => {
            if let Some(j) = &job {
                info!(
                    "maintenance: job #{id} on '{}' cancelled before it ran",
                    j.bucket
                );
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
        CancelOutcome::NotActive => Err(AdminError::conflict(
            "job is not active (already finished or unknown id)",
        )),
    }
}

/// GET /_/api/admin/jobs/bucket/:bucket — session-light tier.
pub async fn bucket_status(
    State(state): State<Arc<AdminState>>,
    connect_info: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>,
    headers: axum::http::HeaderMap,
    Path(bucket): Path<super::path_guard::AdminBucket>,
) -> Result<Json<serde_json::Value>, AdminError<Bare>> {
    // Session-light, but bucket-scoped: only a principal that may list the
    // bucket learns its maintenance state (403, as the S3 LIST answers).
    let client_ip = crate::rate_limiter::extract_client_ip_with_peer(
        &headers,
        connect_info.map(|ci| ci.0.ip()),
    );
    let may = super::auth::extract_session_token(&headers)
        .is_some_and(|t| super::auth::session_may_list_bucket(&state, &t, client_ip, &bucket));
    if !may {
        return Err(AdminError::forbidden(
            "the session may not list this bucket",
        ));
    }
    let job = super::with_config_db(&state, "read bucket maintenance status", |db| {
        db.maintenance_active_job_for_bucket(&bucket.to_ascii_lowercase())
    })
    .await?;
    Ok(Json(serde_json::json!({
        "active": job.map(MaintenanceJobView::from)
    })))
}
