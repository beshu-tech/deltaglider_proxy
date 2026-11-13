// SPDX-License-Identifier: BUSL-1.1

//! Admin API for one-off maintenance jobs (bucket re-encryption).
//!
//! Routes:
//!
//! - `POST /_/api/admin/jobs/reencrypt` `{buckets: [..], open_window?,
//!   purge_plaintext_versions?}` — create
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

use super::{AdminError, AdminState};
use crate::api::admin::extract::AdminJson;
use crate::maintenance::migrate::{pick_transient_key, MigrateParams, MigrateTarget};
use crate::maintenance::store::{current_unix_seconds, CancelOutcome, MaintenanceJob};
use crate::maintenance::{display_percent, resolve_desired};
use crate::storage::encrypting::conversion::OpenMode;
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
    /// Re-encrypt: of `objects_skipped`, the objects that a client wrote
    /// after the job read them; each keeps the client's version.
    pub objects_skipped_changed: i64,
    pub bytes_done: i64,
    /// 0-99 while running (`None` while counting); 100 on `completed`.
    pub percent: Option<u8>,
    /// Whether the write gate refuses writes to the bucket now. Set only by
    /// the bucket status, which reads the gate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub writes_blocked: Option<bool>,
    /// Whether the asking session may cancel this job. Set only by the
    /// bucket status, for its delete jobs, from the stored principal
    /// (`maintenance::delete::may_cancel`): the browser shows Cancel from
    /// this flag and never decides it from a name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub may_cancel: Option<bool>,
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
            objects_skipped_changed: j.objects_changed,
            bytes_done: j.bytes_done,
            percent,
            writes_blocked: None,
            may_cancel: None,
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
    /// Open the bucket's conversion window first (or reuse the valid one):
    /// clients read its plaintext history until the job closes it.
    #[serde(default)]
    pub open_window: bool,
    /// After a run that converts every object, delete the noncurrent
    /// plaintext versions of the bucket's encrypted keys (a versioned
    /// backend; elsewhere nothing to do). They cannot be restored.
    #[serde(default)]
    pub purge_plaintext_versions: bool,
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
    /// Refused for lack of atomic writes on the bucket's backend (N2): the
    /// backend, the probe result in plain words and the doc link.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub atomic_writes: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct ReencryptResponse {
    pub started: Vec<ReencryptStarted>,
    pub errors: Vec<ReencryptError>,
}

/// The request gate that the reencrypt and backfill starts share: 1 to
/// 100 buckets, a jobs DB, and the real bucket set from the engine
/// (authoritative across backends), lowercased.
async fn check_job_request(
    state: &AdminState,
    buckets: Vec<super::path_guard::AdminBucket>,
) -> Result<
    (
        &Arc<tokio::sync::Mutex<crate::config_db::JobsDb>>,
        std::collections::HashSet<String>,
        Vec<crate::bucket_policy::ClientBucket>,
    ),
    AdminError,
> {
    if buckets.is_empty() {
        return Err(AdminError::invalid("no buckets given"));
    }
    if buckets.len() > 100 {
        return Err(AdminError::invalid("too many buckets (max 100)"));
    }
    let buckets = buckets
        .into_iter()
        .map(|b| b.admit(state))
        .collect::<Result<Vec<_>, _>>()?;
    let db = state.jobs_db.as_ref().ok_or_else(AdminError::no_jobs_db)?;
    let engine = state.s3_state.engine.load().clone();
    let real = engine
        .list_bucket_origins()
        .await
        .map_err(|e| AdminError::internal(format!("failed to list buckets: {e}")))?
        .into_iter()
        .map(|b| b.name.to_ascii_lowercase())
        .collect();
    Ok((db, real, buckets))
}

/// POST /_/api/admin/jobs/reencrypt
///
/// A bucket whose job would encrypt with a proxy key gets a fresh
/// atomicity probe of its backend first (N2), before a window opens; a
/// backend without atomic writes refuses it (`errors[].atomic_writes`).
/// When no job starts and a bucket was refused that way, the answer is
/// 409 (the same body).
pub async fn start_reencrypt(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    AdminJson(req): AdminJson<ReencryptRequest>,
) -> Result<(StatusCode, Json<ReencryptResponse>), AdminError> {
    use crate::maintenance::conversion::{
        close_opened, open_window, queue_reencrypt, require_reencrypt_atomic, NotQueued,
        OpenedWindow, QueuedReencrypt, ReencryptParams,
    };
    let (db, real, buckets) = check_job_request(&state, req.buckets).await?;

    let engine = state.s3_state.engine.load_full();
    let cfg = state.config.read().await;
    let mut errors = Vec::new();
    // The jobs to queue, with the bucket as the request named it and the
    // window that this request opened for it.
    let mut requests = Vec::new();
    let mut origins: Vec<(String, Option<OpenedWindow>)> = Vec::new();
    for bucket in &buckets {
        let key = bucket.to_ascii_lowercase();
        let refuse = |error: String| ReencryptError {
            bucket: bucket.to_string(),
            error,
            atomic_writes: None,
        };
        if !real.contains(&key) {
            errors.push(refuse("bucket not found".into()));
            continue;
        }
        let atomic = match resolve_desired(&cfg, &key) {
            Err(reason) => {
                errors.push(refuse(reason));
                continue;
            }
            Ok(crate::maintenance::DesiredEncryption::Proxy { .. }) => {
                match require_reencrypt_atomic(&state.s3_state, &cfg, &key).await {
                    Ok(proof) => Some(proof),
                    Err(r) => {
                        errors.push(ReencryptError {
                            bucket: bucket.to_string(),
                            error: r.message(),
                            atomic_writes: Some(r.details()),
                        });
                        continue;
                    }
                }
            }
            Ok(_) => None,
        };
        let mut opened = None;
        if req.open_window {
            // An active job first: never open a window for a job that
            // cannot be queued.
            match db.lock().await.maintenance_active_job_for_bucket(&key) {
                Ok(None) => {}
                Ok(Some(_)) => {
                    errors.push(refuse(ALREADY_ACTIVE.into()));
                    continue;
                }
                Err(e) => {
                    errors.push(refuse(format!("could not read the active jobs: {e}")));
                    continue;
                }
            }
            match open_window(&engine, &key, OpenMode::Resume, "admin").await {
                Ok(w) => opened = Some(w),
                Err(e) => {
                    errors.push(refuse(format!("could not open the conversion window: {e}")));
                    continue;
                }
            }
        }
        requests.push(QueuedReencrypt {
            bucket: key,
            params: ReencryptParams {
                purge_plaintext_versions: req.purge_plaintext_versions,
                // Only a job with the marker's run extends and closes it.
                run: opened.map(|w| w.run),
                ..Default::default()
            },
            atomic,
        });
        origins.push((bucket.to_string(), opened));
    }
    let results = queue_reencrypt(&state.s3_state, &cfg, db, &requests, "admin", &headers).await;
    drop(cfg);

    let mut started = Vec::new();
    for ((result, (bucket, opened)), q) in results.into_iter().zip(origins).zip(&requests) {
        let (error, atomic_writes) = match result {
            Ok(job_id) => {
                started.push(ReencryptStarted { bucket, job_id });
                continue;
            }
            Err(NotQueued::Active) => (ALREADY_ACTIVE.to_string(), None),
            Err(NotQueued::Failed(e)) => (format!("failed to create job: {e}"), None),
            Err(NotQueued::NotAtomic(r)) => (r.message(), Some(r.details())),
        };
        // The window this request opened is for a job that does not exist.
        if let Some(w) = opened.filter(|w| w.created) {
            close_opened(&engine, &q.bucket, w.run, "the job could not be queued").await;
        }
        errors.push(ReencryptError {
            bucket,
            error,
            atomic_writes,
        });
    }
    let status = if started.is_empty() && errors.iter().any(|e| e.atomic_writes.is_some()) {
        StatusCode::CONFLICT
    } else {
        StatusCode::OK
    };
    Ok((status, Json(ReencryptResponse { started, errors })))
}

const ALREADY_ACTIVE: &str = "a maintenance job is already active for this bucket";

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
    let (db, real, buckets) = check_job_request(&state, req.buckets).await?;

    // A backfill moves the stored time of the objects it stamps: in a
    // bucket in conversion it would push plaintext history past the cutoff.
    // The config read lock is held from the marker check to the job
    // creation: an apply opens a marker under the write lock, then queues
    // its job; it either finds this backfill job active (409) or this
    // check finds its marker.
    let config_guard = state.config.read().await;
    let engine = state.s3_state.engine.load().clone();
    for bucket in &buckets {
        let key = bucket.to_ascii_lowercase();
        if !real.contains(&key) {
            continue;
        }
        // A backfill rewrites metadata in place: an encrypted bucket on a
        // backend without atomic writes is read-only (N2).
        if let Some(b) = state.s3_state.atomic_write_block(&key).await {
            return Err(AdminError::write_block(&b, &key));
        }
        match crate::maintenance::conversion::marker_exists(&engine, &key).await {
            Ok(false) => {}
            Ok(true) => {
                return Err(AdminError::conflict(
                    crate::maintenance::conversion::backfill_refusal(&key),
                ))
            }
            Err(e) => return Err(AdminError::unavailable(e)),
        }
    }

    let params = serde_json::to_string(&crate::maintenance::backfill::BackfillParams {
        refresh_last_modified: req.refresh_last_modified,
    })
    .expect("params serialize");

    let mut started = Vec::new();
    let mut errors = Vec::new();
    for bucket in &buckets {
        let key = bucket.to_ascii_lowercase();
        if !real.contains(&key) {
            errors.push(ReencryptError {
                bucket: bucket.to_string(),
                error: "bucket not found".into(),
                atomic_writes: None,
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
                atomic_writes: None,
            }),
            Err(e) => errors.push(ReencryptError {
                bucket: bucket.to_string(),
                error: format!("failed to create job: {e}"),
                atomic_writes: None,
            }),
        }
    }
    drop(config_guard);

    if !started.is_empty() {
        crate::maintenance::wake_workers(&state.s3_state.maintenance_notify);
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
    let bucket = super::admit_bucket(&state, &bucket)?.into_string();
    let db = state.jobs_db.as_ref().ok_or_else(AdminError::no_jobs_db)?;

    // NEW-10: another name of the same storage is neither gated nor copied,
    // and the cleanup or the prune would delete its objects. Config only,
    // so it runs before the existence check.
    {
        let cfg = state.config.read().await;
        let from_backend = cfg.storage_key_of(&bucket_key).backend;
        let aliases = crate::maintenance::migrate::aliases_of(
            &cfg,
            &bucket_key,
            &from_backend,
            &target_backend,
        );
        if let Some(reason) = crate::maintenance::migrate::alias_refusal(&bucket, &aliases) {
            return Err(AdminError::conflict(reason));
        }
    }

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
        // A move onto a backend that stores the bucket in plaintext where it
        // encrypted is a downgrade: the same confirmation as a config
        // apply (issue #116 P3). The flip itself runs in the background
        // and is not gated again.
        let mut flipped = cfg.clone();
        crate::maintenance::migrate::apply_flip(&mut flipped, &bucket_key, &target_backend, None);
        let confirmed = crate::config::confirmed_downgrades(Some(&headers));
        let downgrades = flipped.unconfirmed_encryption_downgrades(&cfg, Some(&confirmed));
        if !downgrades.is_empty() {
            return Err(AdminError::invalid(downgrades.join("; ")));
        }
        MigrateParams {
            disk_wait: false,
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
        // Encryption needs atomic writes (N2): a bucket that encrypts with
        // a proxy key on the target would be read-only there. A fresh
        // probe of the target backend; the move OUT of a read-only bucket
        // is the way out and is never refused for its source.
        let mut flipped = cfg.clone();
        crate::maintenance::migrate::apply_flip(
            &mut flipped,
            &bucket_key,
            &params.target_backend,
            None,
        );
        crate::coordination::atomicity::gate_encryption_change(
            &cfg,
            &flipped,
            &state.s3_state.backend_capabilities,
        )
        .await?;
    }

    // A backend that the health loop gates would fail every copy: refuse
    // now, in the dialog, instead of a job that fails a second later.
    // Then the destination check of the stage phase, at once: a non-empty
    // destination is a 409 that the dialog shows. The stage phase checks
    // again (the destination can change in between). A count that fails
    // here (an unreadable listing) leaves the decision to the stage phase.
    {
        let cfg = state.config.read().await.clone();
        let verdict = cfg.backend_by_name(&params.target_backend).and_then(|def| {
            state
                .s3_state
                .backend_health
                .get(&params.target_backend, def)
        });
        if let Some(reason) = crate::maintenance::migrate::target_health_refusal(
            &params.target_backend,
            verdict.as_ref(),
        ) {
            return Err(AdminError::conflict(reason));
        }
        if params.target == MigrateTarget::Empty {
            let real = crate::maintenance::migrate::real_bucket_name(&cfg.buckets, &bucket_key)
                .to_string();
            match crate::maintenance::migrate::destination_count_on_backend(
                &cfg,
                &params.target_backend,
                &real,
                1000,
            )
            .await
            {
                Ok((existing, more)) => crate::maintenance::migrate::destination_check(
                    params.target,
                    existing,
                    more,
                    &real,
                    &params.target_backend,
                )
                .map_err(AdminError::conflict)?,
                Err(e) => tracing::warn!(
                    "maintenance: migrate of '{bucket}': destination pre-check failed ({e}); \
                     the stage phase checks it"
                ),
            }
        }
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

    crate::maintenance::wake_workers(&state.s3_state.maintenance_notify);
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

/// Remove a finished one-off job (and its failures) from the Jobs list.
/// 409 while the job is active: cancel it first.
pub async fn delete_finished_job(
    state: &Arc<AdminState>,
    id: i64,
    headers: &HeaderMap,
) -> Result<(), AdminError> {
    let db = state.jobs_db.as_ref().ok_or_else(AdminError::no_jobs_db)?;
    let db = db.lock().await;
    if db.maintenance_job_by_id(id)?.is_none() {
        return Err(AdminError::not_found(format!("job {id} not found")));
    }
    if !db.maintenance_delete_finished_job(id)? {
        return Err(AdminError::conflict(
            "the job is still active: cancel it first",
        ));
    }
    drop(db);
    super::audit_log(
        "maintenance_job_delete",
        "admin",
        &format!("job:{id}"),
        headers,
    );
    Ok(())
}

/// POST /_/api/admin/jobs/maintenance:<id>/cancel (routed via jobs.rs)
pub async fn cancel_job(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, AdminError> {
    let db = state.jobs_db.as_ref().ok_or_else(AdminError::no_jobs_db)?;
    let (outcome, job) = {
        let db = db.lock().await;
        let job = db.maintenance_job_by_id(id)?;
        let outcome = db.maintenance_request_cancel(id)?;
        // A job that never ran settles now, and its gate (bucket AND, for
        // migrate, the staging route) goes with the row. A migrate that
        // ran turns `cancelling`: the worker unwinds it.
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
) -> Result<Json<serde_json::Value>, AdminError> {
    let bucket = bucket.admit(&state)?;
    // Session-light, but bucket-scoped: only a principal that may list the
    // bucket learns its maintenance state (403, as the S3 LIST answers).
    let client_ip = crate::rate_limiter::extract_client_ip_with_peer(
        &headers,
        connect_info.map(|ci| ci.0.ip()),
    );
    let viewer = super::auth::extract_session_token(&state.sessions, &headers)
        .and_then(|t| super::auth::bucket_view_session(&state, &t, client_ip, &bucket))
        .ok_or_else(|| AdminError::forbidden("the session may not list this bucket"))?;
    let jobs = super::with_jobs_db(&state, "read bucket maintenance status", |db| {
        db.maintenance_active_jobs_for_bucket(&bucket.to_ascii_lowercase())
    })
    .await?;
    let (deletes, mut holding): (Vec<_>, Vec<_>) = jobs
        .into_iter()
        .partition(|j| j.kind == crate::maintenance::delete::KIND);
    let job = (!holding.is_empty()).then(|| holding.remove(0));
    // `active` is the job row (until it settles); `writes_blocked` is the
    // write gate.
    // They differ: a cancelled migrate releases the source while it still
    // removes its staged copies, and a migrate in `cleanup` no longer gates
    // the bucket. "Uploads are unavailable" follows `writes_blocked`.
    let busy = state.s3_state.maintenance_gate.is_busy(&bucket);
    Ok(Json(serde_json::json!({
        "active": job.map(|j| MaintenanceJobView {
            writes_blocked: Some(busy),
            ..MaintenanceJobView::from(j)
        }),
        "writes_blocked": busy,
        // The bucket's running deletes: they do not hold it, so several
        // run next to each other and next to `active`.
        "deletes": delete_job_views(deletes, &viewer.principal()),
    })))
}

/// Pure: the delete jobs of a bucket as the viewer sees them, each with
/// whether the viewer may cancel it.
fn delete_job_views(
    jobs: Vec<MaintenanceJob>,
    viewer: &crate::maintenance::delete::Principal,
) -> Vec<MaintenanceJobView> {
    jobs.into_iter()
        .map(|j| {
            let may = crate::maintenance::delete::may_cancel(viewer, j.params.as_deref());
            MaintenanceJobView {
                may_cancel: Some(may),
                ..MaintenanceJobView::from(j)
            }
        })
        .collect()
}

#[cfg(test)]
mod delete_view_tests {
    use super::*;
    use crate::maintenance::delete::{DeleteParams, Principal};

    fn user(ak: &str) -> Principal {
        Principal::User {
            access_key_id: ak.into(),
            client_ip: None,
            user_id: None,
        }
    }

    fn delete_job(id: i64, owner: Principal, triggered_by: &str) -> MaintenanceJob {
        MaintenanceJob {
            id,
            kind: crate::maintenance::delete::KIND.into(),
            bucket: "releases".into(),
            status: "running".into(),
            phase: "delete".into(),
            objects_total: None,
            objects_done: 0,
            objects_skipped: 0,
            objects_failed: 0,
            bytes_done: 0,
            continuation_token: None,
            last_error: None,
            triggered_by: Some(triggered_by.into()),
            params: Some(
                serde_json::to_string(&DeleteParams {
                    prefixes: vec!["a/".into()],
                    keys: vec![],
                    principal: owner,
                    expected: 0,
                })
                .unwrap(),
            ),
            created_at: 1,
            started_at: None,
            finished_at: None,
            updated_at: 1,
            objects_adopted: 0,
            objects_changed: 0,
        }
    }

    /// The flag follows the stored principal, never the display label: a
    /// user NAMED `admin` may not cancel the admin's job, and a user whose
    /// name changed still cancels its own.
    #[test]
    fn may_cancel_follows_the_stored_principal_not_the_label() {
        let jobs = || {
            vec![
                delete_job(1, Principal::Admin, "admin"),
                delete_job(2, user("AKDANA"), "dana"),
                delete_job(3, user("AKDANA"), "renamed"),
            ]
        };
        let flags = |viewer: Principal| {
            delete_job_views(jobs(), &viewer)
                .iter()
                .map(|v| v.may_cancel)
                .collect::<Vec<_>>()
        };
        assert_eq!(flags(Principal::Admin), [Some(true); 3]);
        assert_eq!(flags(user("AKDANA")), [Some(false), Some(true), Some(true)]);
        // A user named `admin`: the label of job 1 says `admin`, the
        // principal says otherwise.
        assert_eq!(
            flags(user("AKNAMEDADMIN")),
            [Some(false), Some(false), Some(false)]
        );
        let json = serde_json::to_value(&delete_job_views(jobs(), &Principal::Admin)[0]).unwrap();
        assert_eq!(json["may_cancel"], true);
    }
}
