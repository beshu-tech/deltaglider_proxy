// SPDX-License-Identifier: BUSL-1.1

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
//!   browser memory.
//!
//! This module moves the orchestration into the proxy where the
//! engine is already running. Endpoints live under `/_/api/admin/objects/*`
//! behind **`require_bulk_session`**. They call `engine.retrieve` / `store`
//! / `delete` directly. For an admin GUI session the session is the
//! authorization boundary. For a non-admin browser session (S3BrowserLift)
//! every key is authorized with the user's IAM policy before it is touched
//! — the same `can_with_context` check, with `aws:SourceIp`, that the S3
//! API runs — and a denied key is reported per key ([`BulkActor`]).
//!
//! The zip streams: each object goes through the engine's streaming
//! retrieve into a STORED/ZIP64 archive ([`crate::zip_stream`]), so server
//! memory does not grow with the selection or the object size.

use crate::api::admin::extract::{AdminJson, AdminQuery};
use crate::api::handlers::AppState;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info, warn};

use super::auth::BulkSession;
use super::path_guard::{AdminBucket, AdminObjectPath};
use crate::iam::{AuthenticatedUser, S3Action};

// ---------------------------------------------------------------------------
// Request/response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CopyRequest {
    /// Source bucket — every key in `keys` is read from here.
    pub source_bucket: AdminBucket,
    /// Destination bucket. May be the same as source.
    pub dest_bucket: AdminBucket,
    /// Optional prefix prepended to every destination key.
    #[serde(default)]
    pub dest_prefix: AdminObjectPath,
    /// Pairs of (source_key, relative_dest_suffix). The dest key is
    /// `dest_prefix + relative_suffix`. The client computes relatives
    /// because folder-selection semantics are UI-driven (which prefix
    /// counts as the "common prefix" depends on what the user picked).
    pub items: Vec<CopyItem>,
}

#[derive(Debug, Deserialize)]
pub struct CopyItem {
    pub source_key: AdminObjectPath,
    pub relative: AdminObjectPath,
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
    pub source_bucket: AdminBucket,
    pub dest_bucket: AdminBucket,
    #[serde(default)]
    pub dest_prefix: AdminObjectPath,
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
    pub bucket: AdminBucket,
    pub keys: Vec<AdminObjectPath>,
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

fn dest_key(dest_prefix: &str, relative: &str) -> String {
    if dest_prefix.is_empty() {
        relative.to_string()
    } else {
        format!("{}{}", dest_prefix, relative)
    }
}

/// True when a move item's destination resolves to the exact same
/// bucket+key as its source — i.e. the "copy" is a self no-op and the source
/// must NOT be deleted (doing so is data loss). Pure decision point; unit-tested.
fn is_same_location_move(
    source_bucket: &str,
    dest_bucket: &str,
    dest_prefix: &str,
    source_key: &str,
    relative: &str,
) -> bool {
    source_bucket == dest_bucket && dest_key(dest_prefix, relative) == source_key
}

/// Detect duplicate destination keys in a copy/move plan. The client
/// already does this; we re-check server-side because trusting the
/// client to validate was the cause of past silent overwrites.
fn detect_collisions(items: &[CopyItem], dest_prefix: &str) -> Vec<String> {
    use std::collections::HashMap;
    let mut counts: HashMap<String, usize> = HashMap::new();
    for it in items {
        *counts
            .entry(dest_key(dest_prefix, &it.relative))
            .or_insert(0) += 1;
    }
    counts
        .into_iter()
        .filter_map(|(k, n)| if n > 1 { Some(k) } else { None })
        .collect()
}

/// First destination key (same bucket) that is ANOTHER item's source key.
/// The loop copies items in order, so such a destination overwrites a source
/// before it is read, and a move then deletes it (D12: moving `f/` into
/// `f/sub/`). An item whose destination is its own source is the
/// same-location case, handled in the delete loop.
fn dest_overwrites_other_source(
    items: &[CopyItem],
    source_bucket: &str,
    dest_bucket: &str,
    dest_prefix: &str,
) -> Option<String> {
    if source_bucket != dest_bucket {
        return None;
    }
    let sources: std::collections::HashSet<&str> =
        items.iter().map(|i| i.source_key.as_str()).collect();
    items.iter().find_map(|it| {
        let dk = dest_key(dest_prefix, &it.relative);
        (dk != *it.source_key && sources.contains(dk.as_str())).then_some(dk)
    })
}

/// Plan checks shared by copy and move: no two items to one destination, and
/// no destination that is another selected source.
fn validate_plan(
    items: &[CopyItem],
    source_bucket: &str,
    dest_bucket: &str,
    dest_prefix: &str,
) -> Result<(), (StatusCode, String)> {
    let collisions = detect_collisions(items, dest_prefix);
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
    if let Some(k) = dest_overwrites_other_source(items, source_bucket, dest_bucket, dest_prefix) {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "destination {k:?} is also a selected source: the operation would overwrite \
                 it before it is copied (is the destination inside the selection?)"
            ),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// POST /_/api/admin/objects/copy
// ---------------------------------------------------------------------------
// Per-key authorization
// ---------------------------------------------------------------------------

/// Who a bulk request acts as.
enum BulkActor {
    /// Admin GUI or open mode: no per-key check.
    Unrestricted,
    /// A browser session's IAM user and the request's policy context.
    User {
        user: Box<AuthenticatedUser>,
        context: Box<iam_rs::Context>,
    },
}

impl BulkActor {
    /// The actor for a session. An IAM user that is gone or disabled
    /// since the session began gets 403.
    fn for_session(
        state: &crate::api::admin::AdminState,
        session: &BulkSession,
    ) -> Result<Self, (StatusCode, String)> {
        let (access_key_id, client_ip) = match session {
            BulkSession::AdminGui | BulkSession::Open => return Ok(Self::Unrestricted),
            BulkSession::IamUser {
                access_key_id,
                client_ip,
            } => (access_key_id, *client_ip),
        };
        let iam = state.iam_state.load();
        let user = match iam.as_ref() {
            crate::iam::IamState::Iam(index) => index.get(access_key_id),
            _ => None,
        }
        .filter(|u| u.enabled)
        .ok_or((
            StatusCode::FORBIDDEN,
            "AccessDenied: user disabled or gone".to_string(),
        ))?;
        let mut context = iam_rs::Context::new();
        crate::iam::permissions::insert_source_ip(&mut context, client_ip);
        Ok(Self::User {
            user: Box::new(AuthenticatedUser::from(user)),
            context: Box::new(context),
        })
    }

    /// Ok when the actor may do `action` on `bucket/key`; else the per-key
    /// error text.
    fn check(&self, action: S3Action, bucket: &str, key: &str) -> Result<(), String> {
        match self {
            Self::Unrestricted => Ok(()),
            Self::User { user, context } => {
                if user.can_with_context(action, bucket, key, context) {
                    Ok(())
                } else {
                    Err(format!(
                        "AccessDenied: {} may not {} {bucket}/{key}",
                        user.name,
                        action.as_str()
                    ))
                }
            }
        }
    }

    /// May the actor list `bucket` under `prefix` (`s3:prefix` = prefix)?
    fn may_list(&self, bucket: &str, prefix: &str) -> bool {
        match self {
            Self::Unrestricted => true,
            Self::User { user, context } => {
                let mut ctx = (**context).clone();
                ctx.insert(
                    "s3:prefix".to_string(),
                    iam_rs::ContextValue::String(prefix.to_string()),
                );
                user.can_with_context(S3Action::List, bucket, prefix, &ctx)
                    || (!user.is_explicitly_denied(S3Action::List, bucket, prefix, &ctx)
                        && user.can_see_bucket(bucket))
            }
        }
    }

    /// May the actor see `key` in a listing (the S3 LIST filter)?
    fn may_see(&self, bucket: &str, key: &str) -> bool {
        match self {
            Self::Unrestricted => true,
            Self::User { user, context } => {
                crate::iam::permissions::user_can_see_listed_key(user, bucket, key, context)
            }
        }
    }

    /// The audit actor.
    fn label(&self) -> &str {
        match self {
            Self::Unrestricted => "admin",
            Self::User { user, .. } => &user.name,
        }
    }
}

/// 409 when the bucket is under maintenance (re-encryption rewrites the
/// bucket in place; admin writes bypass the S3 gate, so check explicitly).
fn reject_if_under_maintenance(
    state: &std::sync::Arc<crate::api::admin::AdminState>,
    bucket: &str,
) -> Result<(), (StatusCode, String)> {
    if state.s3_state.maintenance_gate.is_busy(bucket) {
        return Err((
            StatusCode::CONFLICT,
            format!("bucket '{bucket}' is temporarily read-only: maintenance in progress"),
        ));
    }
    Ok(())
}

/// 403 for the coordination bucket (`config_sync_bucket`): reserved for the
/// proxy's own sync client, for every identity. Admin bulk ops bypass the S3
/// gate, so they check explicitly.
fn reject_if_reserved(
    state: &std::sync::Arc<crate::api::admin::AdminState>,
    bucket: &str,
) -> Result<(), (StatusCode, String)> {
    match state
        .s3_state
        .engine
        .load()
        .bucket_policy_registry()
        .reserved_bucket_reason(bucket)
    {
        Some(reason) => Err((StatusCode::FORBIDDEN, reason)),
        None => Ok(()),
    }
}

/// 403 when the bucket is `replication_target_only` — admin bulk ops are
/// client writes too (same seam as the S3 gate, adapted to admin errors).
fn reject_if_replication_target_only(
    state: &std::sync::Arc<crate::api::admin::AdminState>,
    bucket: &str,
) -> Result<(), (StatusCode, String)> {
    crate::api::handlers::object_helpers::check_client_write_allowed(&state.s3_state, bucket)
        .map_err(|e| (StatusCode::FORBIDDEN, e.to_string()))
}

pub async fn copy_objects(
    Extension(session): Extension<BulkSession>,
    State(state): State<Arc<crate::api::admin::AdminState>>,
    headers: axum::http::HeaderMap,
    AdminJson(req): AdminJson<CopyRequest>,
) -> Result<Json<CopyResponse>, (StatusCode, String)> {
    reject_if_reserved(&state, &req.source_bucket)?;
    reject_if_reserved(&state, &req.dest_bucket)?;
    reject_if_under_maintenance(&state, &req.dest_bucket)?;
    reject_if_replication_target_only(&state, &req.dest_bucket)?;
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

    validate_plan(
        &req.items,
        &req.source_bucket,
        &req.dest_bucket,
        &req.dest_prefix,
    )?;

    let actor = BulkActor::for_session(&state, &session)?;
    let s3 = state.s3_state.clone();
    let (res, _) = run_copy_loop(&s3, &req, &actor, false).await;
    info!(
        "bulk copy: src={} dst={}/{} succeeded={} failed={}",
        req.source_bucket, req.dest_bucket, req.dest_prefix, res.succeeded, res.failed
    );
    super::audit_log(
        "bulk_copy",
        actor.label(),
        &format!(
            "{} -> {}/{} ok={} failed={}",
            req.source_bucket, req.dest_bucket, req.dest_prefix, res.succeeded, res.failed
        ),
        &headers,
    );
    Ok(Json(res))
}

/// Also returns, per item, the source object as it was copied (`None`: not
/// copied), so a move deletes a source only while it is still that object.
/// `delete_source`: a move, so the actor also needs delete on each source.
async fn run_copy_loop(
    s3: &Arc<AppState>,
    req: &CopyRequest,
    actor: &BulkActor,
    delete_source: bool,
) -> (CopyResponse, Vec<Option<crate::types::FileMetadata>>) {
    let engine = s3.engine.load();
    let mut copied = vec![None; req.items.len()];
    let mut succeeded = 0usize;
    let mut failed = 0usize;
    let mut failures: Vec<CopyFailure> = Vec::new();
    // Register with the maintenance gate for the whole loop: the
    // entry-check in the handler only sees jobs that existed when the
    // request STARTED, but this loop can write for minutes. write_started
    // makes the worker's drain wait for us; the per-item is_busy check
    // stops us the moment a job arms mid-loop.
    let gate = &s3.maintenance_gate;
    // RAII: releases the drain slot even if this handler future is dropped
    // (admin closes the browser mid-bulk-copy) — straight-line write_finished
    // would leak the counter and wedge every later job on the bucket (H12).
    let _write = gate.begin_write(&req.dest_bucket);
    for (idx, it) in req.items.iter().enumerate() {
        if gate.is_busy(&req.dest_bucket) {
            let remaining = req.items.len() - idx;
            failed += remaining;
            if failures.len() < MAX_FAILURE_ENTRIES {
                failures.push(CopyFailure {
                    source_key: it.source_key.to_string(),
                    dest_key: String::new(),
                    error: format!(
                        "a maintenance job started on bucket '{}' — {} remaining \
                         item(s) skipped; retry after the job finishes",
                        req.dest_bucket, remaining
                    ),
                });
            }
            break;
        }
        let dk = dest_key(&req.dest_prefix, &it.relative);
        // The same checks the S3 API runs for GET source + PUT dest (+ the
        // source DELETE of a move), before any byte moves.
        let allowed = actor
            .check(S3Action::Read, &req.source_bucket, &it.source_key)
            .and_then(|()| actor.check(S3Action::Write, &req.dest_bucket, &dk))
            .and_then(|()| {
                if delete_source {
                    actor.check(S3Action::Delete, &req.source_bucket, &it.source_key)
                } else {
                    Ok(())
                }
            });
        let result = match allowed {
            Ok(()) => {
                copy_one(
                    s3,
                    &engine,
                    &req.source_bucket,
                    &it.source_key,
                    &req.dest_bucket,
                    &dk,
                )
                .await
            }
            Err(denied) => Err(denied),
        };
        match result {
            Ok(source) => {
                succeeded += 1;
                copied[idx] = Some(source);
            }
            Err(e) => {
                failed += 1;
                if failures.len() < MAX_FAILURE_ENTRIES {
                    failures.push(CopyFailure {
                        source_key: it.source_key.to_string(),
                        dest_key: dk,
                        error: e,
                    });
                }
            }
        }
    }
    drop(_write);
    (
        CopyResponse {
            succeeded,
            failed,
            failures,
        },
        copied,
    )
}

/// Copy one object the way a client write is handled: quota gate on the
/// destination, the shared engine-routed transfer (streams or spools large
/// objects instead of holding them in RAM; strips encryption markers; keeps
/// multipart ETags), then an `ObjectCopied` outbox event. Returns the
/// source's metadata as read before the copy.
async fn copy_one(
    s3: &Arc<AppState>,
    engine: &Arc<crate::deltaglider::DynEngine>,
    src_bucket: &str,
    src_key: &str,
    dst_bucket: &str,
    dst_key: &str,
) -> Result<crate::types::FileMetadata, String> {
    let head = engine
        .head(src_bucket, src_key)
        .await
        .map_err(|e| format!("head {}/{}: {}", src_bucket, src_key, e))?;
    crate::api::handlers::object_helpers::check_quota(s3, dst_bucket, head.file_size)
        .map_err(|e| e.to_string())?;
    let outcome = crate::transfer::copy_object_with_retries(
        engine,
        crate::transfer::ObjectTransferRequest {
            source_bucket: src_bucket,
            source_key: src_key,
            destination_bucket: dst_bucket,
            destination_key: dst_key,
            provenance: None,
            strip_user_metadata_keys: &[],
            operation: "admin bulk copy",
            upload_concurrency: None,
            keep_created_at: false,
        },
    )
    .await
    .map_err(|e| format!("copy {src_bucket}/{src_key} -> {dst_bucket}/{dst_key}: {e}"))?;
    emit_event(
        s3,
        crate::event_outbox::EventKind::ObjectCopied,
        dst_bucket,
        dst_key,
        serde_json::json!({
            "content_length": outcome.content_length(),
            "source_bucket": src_bucket,
            "source_key": src_key,
        }),
    )
    .await;
    Ok(head)
}

/// Outbox append for an admin write, same shape as the S3 adapter's (source
/// `S3Api`: these are client writes, so replication and webhooks see them).
async fn emit_event(
    s3: &Arc<AppState>,
    kind: crate::event_outbox::EventKind,
    bucket: &str,
    key: &str,
    payload: serde_json::Value,
) {
    crate::api::handlers::object_helpers::enqueue_object_event(
        s3,
        crate::event_outbox::NewEvent::new(
            kind,
            bucket,
            key,
            crate::event_outbox::EventSource::S3Api,
            crate::replication::current_unix_seconds(),
            payload,
        ),
    )
    .await;
}

// ---------------------------------------------------------------------------
// POST /_/api/admin/objects/move
// ---------------------------------------------------------------------------

pub async fn move_objects(
    Extension(session): Extension<BulkSession>,
    State(state): State<Arc<crate::api::admin::AdminState>>,
    headers: axum::http::HeaderMap,
    AdminJson(req): AdminJson<MoveRequest>,
) -> Result<Json<MoveResponse>, (StatusCode, String)> {
    reject_if_reserved(&state, &req.source_bucket)?;
    reject_if_reserved(&state, &req.dest_bucket)?;
    reject_if_under_maintenance(&state, &req.dest_bucket)?;
    reject_if_under_maintenance(&state, &req.source_bucket)?;
    // Move = store into dest + delete from source: both are client writes.
    reject_if_replication_target_only(&state, &req.dest_bucket)?;
    reject_if_replication_target_only(&state, &req.source_bucket)?;
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

    validate_plan(
        &req.items,
        &req.source_bucket,
        &req.dest_bucket,
        &req.dest_prefix,
    )?;

    let s3 = state.s3_state.clone();
    let copy_req = CopyRequest {
        source_bucket: req.source_bucket.clone(),
        dest_bucket: req.dest_bucket.clone(),
        dest_prefix: req.dest_prefix.clone(),
        items: req
            .items
            .iter()
            .map(|i| CopyItem {
                source_key: i.source_key.clone(),
                relative: i.relative.clone(),
            })
            .collect(),
    };
    let actor = BulkActor::for_session(&state, &session)?;
    let (copy_result, copied) = run_copy_loop(&s3, &copy_req, &actor, true).await;

    // Atomicity rule: only delete sources if EVERY copy succeeded.
    // Pre-migration the client implemented this same rule client-side;
    // doing it here keeps the contract identical and lets us extend
    // to actual transactions later.
    let mut deleted = 0usize;
    let mut skipped_self = 0usize;
    if copy_result.failed == 0 {
        let engine = s3.engine.load();
        let gate = &s3.maintenance_gate;
        // RAII drain slot — released on drop even if the handler future is
        // cancelled mid-loop (H12).
        let _write = gate.begin_write(&req.source_bucket);
        for (it, source) in req.items.iter().zip(&copied) {
            if gate.is_busy(&req.source_bucket) {
                // A maintenance job armed mid-loop: stop deleting sources.
                // The copies succeeded; leftovers are benign (same contract
                // as a failed source delete below).
                warn!(
                    "bulk move: maintenance job started on '{}' — leaving remaining \
                     source objects in place",
                    req.source_bucket
                );
                break;
            }
            // DATA-LOSS GUARD: a move whose destination key resolves to the
            // SAME bucket+key as the source is a self-copy no-op. Deleting the
            // source here would destroy the only copy. Never delete a source we
            // did not actually relocate elsewhere — regardless of what the
            // client computed. (The GUI should also prevent offering this, but
            // this is the last line of defence.)
            if is_same_location_move(
                &req.source_bucket,
                &req.dest_bucket,
                &req.dest_prefix,
                &it.source_key,
                &it.relative,
            ) {
                skipped_self += 1;
                continue;
            }
            let Some(source) = source else { continue };
            // Delete the source only while it holds the bytes we copied: a
            // client PUT since the copy is a new object, not ours to delete.
            let still_copied = |current: &crate::types::FileMetadata| {
                crate::deltaglider::DynEngine::same_generation(source, current)
            };
            match engine
                .delete_if(&req.source_bucket, &it.source_key, &still_copied)
                .await
            {
                Ok(crate::deltaglider::ConditionalDelete::Deleted(_)) => {
                    deleted += 1;
                    emit_event(
                        &s3,
                        crate::event_outbox::EventKind::ObjectDeleted,
                        &req.source_bucket,
                        &it.source_key,
                        serde_json::json!({}),
                    )
                    .await;
                }
                Ok(_) => debug!(
                    "bulk move: source {}/{} changed or gone since its copy; left in place",
                    req.source_bucket, it.source_key
                ),
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
        drop(_write);
        if skipped_self > 0 {
            warn!(
                "bulk move: skipped deleting {} source(s) whose destination equals the source \
                 (same-location move — would have been data loss)",
                skipped_self
            );
        }
    }

    info!(
        "bulk move: src={} dst={}/{} succeeded={} failed={} deleted={}",
        req.source_bucket,
        req.dest_bucket,
        req.dest_prefix,
        copy_result.succeeded,
        copy_result.failed,
        deleted
    );
    super::audit_log(
        "bulk_move",
        actor.label(),
        &format!(
            "{} -> {}/{} ok={} failed={} deleted={}",
            req.source_bucket,
            req.dest_bucket,
            req.dest_prefix,
            copy_result.succeeded,
            copy_result.failed,
            deleted
        ),
        &headers,
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
    Extension(session): Extension<BulkSession>,
    State(state): State<Arc<crate::api::admin::AdminState>>,
    headers: axum::http::HeaderMap,
    AdminJson(req): AdminJson<DeleteRequest>,
) -> Result<Json<DeleteResponse>, (StatusCode, String)> {
    reject_if_reserved(&state, &req.bucket)?;
    reject_if_under_maintenance(&state, &req.bucket)?;
    reject_if_replication_target_only(&state, &req.bucket)?;
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

    let actor = BulkActor::for_session(&state, &session)?;
    let engine = state.s3_state.engine.load();
    let mut deleted = 0usize;
    let mut failed = 0usize;
    let mut failures: Vec<DeleteFailure> = Vec::new();
    // Same gate participation as run_copy_loop: visible to the worker's
    // drain, and stops the moment a maintenance job arms mid-loop.
    let gate = &state.s3_state.maintenance_gate;
    // RAII drain slot — released on drop even if the handler future is
    // cancelled mid-loop (H12).
    let _write = gate.begin_write(&req.bucket);
    for (idx, key) in req.keys.iter().enumerate() {
        if gate.is_busy(&req.bucket) {
            let remaining = req.keys.len() - idx;
            failed += remaining;
            if failures.len() < MAX_FAILURE_ENTRIES {
                failures.push(DeleteFailure {
                    key: key.to_string(),
                    error: format!(
                        "a maintenance job started on bucket '{}' — {} remaining \
                         key(s) skipped; retry after the job finishes",
                        req.bucket, remaining
                    ),
                });
            }
            break;
        }
        if let Err(denied) = actor.check(S3Action::Delete, &req.bucket, key) {
            failed += 1;
            if failures.len() < MAX_FAILURE_ENTRIES {
                failures.push(DeleteFailure {
                    key: key.to_string(),
                    error: denied,
                });
            }
            continue;
        }
        match engine.delete(&req.bucket, key).await {
            Ok(_) => {
                deleted += 1;
                emit_event(
                    &state.s3_state,
                    crate::event_outbox::EventKind::ObjectDeleted,
                    &req.bucket,
                    key,
                    serde_json::json!({}),
                )
                .await;
            }
            Err(e) => {
                // Not-found is treated as deleted (idempotent).
                let s3_err: crate::api::S3Error = e.into();
                if matches!(s3_err, crate::api::S3Error::NoSuchKey(_)) {
                    deleted += 1;
                } else {
                    failed += 1;
                    if failures.len() < MAX_FAILURE_ENTRIES {
                        failures.push(DeleteFailure {
                            key: key.to_string(),
                            error: format!("{}", s3_err),
                        });
                    }
                }
            }
        }
    }

    drop(_write);
    info!(
        "bulk delete: bucket={} deleted={} failed={}",
        req.bucket, deleted, failed
    );
    super::audit_log(
        "bulk_delete",
        actor.label(),
        &format!("{} deleted={} failed={}", req.bucket, deleted, failed),
        &headers,
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
// The archive streams. The first readable entry opens BEFORE the answer, so
// a request where no file can be read still gets an error status. A later
// file that cannot be opened goes into the skip report inside the archive;
// a file that fails after its bytes started aborts the download (see
// `crate::zip_stream`). No size cap: memory is bounded by the stream.

/// ZIP entry names for `(bucket, key)` pairs: each key's path below the
/// deepest folder shared by the whole selection, so a zipped folder keeps its
/// structure (`release-3.0/docs/NOTES.md`) and distinct keys can never
/// collide. With keys from several buckets the bucket name leads.
///
/// It used to name entries by basename and "de-duplicate" the later one as
/// `key.replace('/', "_")` — which for a top-level key is the basename again,
/// so selecting `README.md` next to a folder holding another `README.md`
/// failed the whole download with a 500 "Duplicate filename".
fn zip_entry_names(items: &[(String, String)]) -> Vec<String> {
    let multi_bucket = items.windows(2).any(|w| w[0].0 != w[1].0);
    let full: Vec<String> = items
        .iter()
        .map(|(b, k)| {
            if multi_bucket {
                format!("{b}/{k}")
            } else {
                k.clone()
            }
        })
        .collect();
    // Longest shared directory prefix (whole path segments only).
    let dir_of = |p: &str| p.rfind('/').map_or(0, |i| i + 1);
    let mut common = full.first().map_or(0, |p| dir_of(p));
    for p in &full {
        let limit = common.min(dir_of(p));
        let first = &full[0];
        let mut n = 0;
        for (i, (a, b)) in first[..limit].bytes().zip(p[..limit].bytes()).enumerate() {
            if a != b {
                break;
            }
            if a == b'/' {
                n = i + 1;
            }
        }
        common = n;
    }
    full.into_iter().map(|p| p[common..].to_string()).collect()
}

pub async fn download_zip(
    Extension(session): Extension<BulkSession>,
    State(state): State<Arc<crate::api::admin::AdminState>>,
    headers: axum::http::HeaderMap,
    AdminQuery(q): AdminQuery<ZipQuery>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let parsed: Vec<(String, String)> = q
        .keys
        .split(',')
        .filter(|s| !s.is_empty())
        .filter_map(|item| {
            // Each entry is `bucket/key`, split on the FIRST '/' so
            // keys with embedded slashes round-trip correctly.
            item.split_once('/')
                .map(|(b, k)| (b.to_string(), k.to_string()))
        })
        .collect();
    if parsed.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "?keys must be a comma-separated list of bucket/key entries".into(),
        ));
    }
    for (b, k) in &parsed {
        super::path_guard::check_bucket(b)
            .and_then(|()| super::path_guard::check_object_path(k))
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
        reject_if_reserved(&state, b)?;
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

    let actor = BulkActor::for_session(&state, &session)?;
    let engine = state.s3_state.engine.load_full();
    let names = zip_entry_names(&parsed);
    let mut skipped: Vec<(String, String)> = Vec::new();
    let mut failures: Vec<ZipFailure> = Vec::new();
    // Authorize every key first, against one IAM snapshot.
    let mut entries: std::collections::VecDeque<crate::zip_stream::Entry<(String, String)>> =
        std::collections::VecDeque::with_capacity(parsed.len());
    for ((bucket, key), name) in parsed.iter().zip(names) {
        let label = format!("{bucket}/{key}");
        if let Err(denied) = actor.check(S3Action::Read, bucket, key) {
            failures.push(ZipFailure::AccessDenied);
            skipped.push((label, denied));
            continue;
        }
        entries.push_back(crate::zip_stream::Entry {
            name,
            label,
            key: (bucket.clone(), key.clone()),
            opened: None,
        });
    }
    // Open the first readable entry before answering.
    while let Some(mut first) = entries.pop_front() {
        match open_zip_entry(&engine, &first.key.0, &first.key.1).await {
            Ok(src) => {
                first.opened = Some(src);
                entries.push_front(first);
                break;
            }
            Err(e) => {
                debug!("zip: skipping {}: {}", first.label, e);
                failures.push(zip_failure_kind(&e));
                skipped.push((first.label, e.to_string()));
            }
        }
    }
    if entries.is_empty() {
        let msg = zip_skip_report(parsed.len(), &skipped)
            .err()
            .unwrap_or_else(|| "no file to put in the ZIP".to_string());
        return Err((zip_all_failed_status(&failures), msg));
    }

    info!(
        "zip: streaming {} of {} selected files",
        entries.len(),
        parsed.len()
    );
    // Audited when the download starts: later per-file skips are in the
    // archive's skip report, the IAM denials are known now.
    let mut buckets: Vec<&str> = parsed.iter().map(|(b, _)| b.as_str()).collect();
    buckets.dedup();
    buckets.sort_unstable();
    buckets.dedup();
    let denied = failures
        .iter()
        .filter(|f| **f == ZipFailure::AccessDenied)
        .count();
    super::audit_log(
        "bulk_zip",
        actor.label(),
        &format!(
            "{} keys={} denied={}",
            buckets.join(","),
            parsed.len(),
            denied
        ),
        &headers,
    );
    let requested = parsed.len();
    let open_engine = engine.clone();
    let stream = crate::zip_stream::stream_archive(
        crate::zip_stream::Limits::ZIP,
        entries.into(),
        skipped,
        move |(bucket, key): &(String, String)| {
            let engine = open_engine.clone();
            let (bucket, key) = (bucket.clone(), key.clone());
            async move {
                open_zip_entry(&engine, &bucket, &key)
                    .await
                    .map_err(|e| e.to_string())
            }
        },
        move |written: &[String], skipped: &[(String, String)]| {
            let report = zip_skip_report(requested, skipped).ok().flatten()?;
            let taken: Vec<&str> = written.iter().map(String::as_str).collect();
            Some((zip_skip_report_name(&taken), report.into_bytes()))
        },
    );

    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let cd = format!("attachment; filename=\"deltaglider-{date}.zip\"");
    let mut resp = axum::response::Response::new(axum::body::Body::from_stream(stream));
    let h = resp.headers_mut();
    h.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/zip"),
    );
    h.insert(
        axum::http::header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_str(&cd).expect("ASCII date"),
    );
    h.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    Ok(resp)
}

/// Open one object for the ZIP through the engine's streaming retrieve, the
/// path an S3 GET takes: a passthrough object streams from the backend, a
/// large delta decodes to a spool file first (integrity checked before the
/// first byte), a small delta is already in memory.
async fn open_zip_entry(
    engine: &crate::deltaglider::DynEngine,
    bucket: &str,
    key: &str,
) -> Result<crate::zip_stream::EntrySource, crate::deltaglider::EngineError> {
    use crate::deltaglider::RetrieveResponse;
    use futures::{StreamExt, TryStreamExt};
    let (body, metadata) = match engine.retrieve_stream(bucket, key).await? {
        RetrieveResponse::Streamed {
            stream, metadata, ..
        } => (stream.map_err(std::io::Error::other).boxed(), metadata),
        RetrieveResponse::Buffered { data, metadata, .. } => {
            (crate::zip_stream::in_memory_body(data), metadata)
        }
    };
    Ok(crate::zip_stream::EntrySource {
        // The size an S3 GET announces as Content-Length; the writer fails
        // the entry when the body does not match it.
        size: metadata.file_size,
        modified: metadata.created_at,
        body,
    })
}

/// Archive entry that lists the files a partial ZIP could not include.
const ZIP_SKIP_REPORT_NAME: &str = "_deltaglider-skipped-files.txt";

/// A name for the skip report that no archive entry already uses: a selected
/// file can itself be called `_deltaglider-skipped-files.txt`, and a duplicate
/// entry name fails the whole archive.
fn zip_skip_report_name(taken: &[&str]) -> String {
    let mut name = ZIP_SKIP_REPORT_NAME.to_string();
    let mut n = 2;
    while taken.contains(&name.as_str()) {
        name = format!("_deltaglider-skipped-files-{n}.txt");
        n += 1;
    }
    name
}

/// Why one file of a ZIP could not be read, as far as the status goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ZipFailure {
    NotFound,
    AccessDenied,
    TooLarge,
    Overloaded,
    Other,
}

fn zip_failure_kind(e: &crate::deltaglider::EngineError) -> ZipFailure {
    use crate::deltaglider::EngineError;
    use crate::storage::StorageError;
    if e.is_not_found() {
        return ZipFailure::NotFound;
    }
    match e {
        EngineError::Storage(StorageError::Io(io))
            if io.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            ZipFailure::AccessDenied
        }
        EngineError::Storage(se) if crate::storage::is_backend_access_denied(se) => {
            ZipFailure::AccessDenied
        }
        EngineError::TooLarge { .. } | EngineError::Storage(StorageError::TooLarge { .. }) => {
            ZipFailure::TooLarge
        }
        EngineError::Overloaded(_) | EngineError::Storage(StorageError::Throttled(_)) => {
            ZipFailure::Overloaded
        }
        _ => ZipFailure::Other,
    }
}

/// Status of a ZIP request where no selected file could be read. It used to
/// be 404 for every cause. Now: 404 only when every file is missing; else,
/// by precedence, 403 when access was denied to any file, 413 when a file is
/// too large, 503 when the proxy or the backend shed load, and 502 for any
/// other backend failure.
fn zip_all_failed_status(failures: &[ZipFailure]) -> StatusCode {
    if !failures.is_empty() && failures.iter().all(|f| *f == ZipFailure::NotFound) {
        StatusCode::NOT_FOUND
    } else if failures.contains(&ZipFailure::AccessDenied) {
        StatusCode::FORBIDDEN
    } else if failures.contains(&ZipFailure::TooLarge) {
        StatusCode::PAYLOAD_TOO_LARGE
    } else if failures.contains(&ZipFailure::Overloaded) {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::BAD_GATEWAY
    }
}

/// Decide what a ZIP says about files it could not read. None read: an
/// error, not an empty archive. Some read: a report entry inside the
/// archive, so a partial download never looks complete.
fn zip_skip_report(
    requested: usize,
    skipped: &[(String, String)],
) -> Result<Option<String>, String> {
    if skipped.is_empty() {
        return Ok(None);
    }
    if skipped.len() >= requested {
        let (key, reason) = &skipped[0];
        return Err(format!(
            "None of the {requested} selected files could be read (first: {key}: {reason})"
        ));
    }
    let mut report = format!(
        "{} of {} selected files could not be read and are not in this archive:\n\n",
        skipped.len(),
        requested
    );
    for (key, reason) in skipped {
        report.push_str(&format!("{key}: {reason}\n"));
    }
    Ok(Some(report))
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
    pub bucket: AdminBucket,
    #[serde(default)]
    pub prefix: AdminObjectPath,
}

#[derive(Debug, Serialize)]
pub struct ListAllResponse {
    pub keys: Vec<String>,
    pub truncated: bool,
}

pub async fn list_all(
    Extension(session): Extension<BulkSession>,
    State(state): State<Arc<crate::api::admin::AdminState>>,
    AdminQuery(q): AdminQuery<ListAllQuery>,
) -> Result<Json<ListAllResponse>, (StatusCode, String)> {
    if q.prefix.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "prefix is required (refusing whole-bucket recursion)".into(),
        ));
    }

    reject_if_reserved(&state, &q.bucket)?;
    let actor = BulkActor::for_session(&state, &session)?;
    if !actor.may_list(&q.bucket, &q.prefix) {
        return Err((
            StatusCode::FORBIDDEN,
            format!(
                "AccessDenied: {} may not list {}/{}",
                actor.label(),
                &*q.bucket,
                &*q.prefix
            ),
        ));
    }
    let engine = state.s3_state.engine.load();
    let mut keys: Vec<String> = Vec::new();
    let mut continuation: Option<String> = None;
    let cap = 1000u32;
    loop {
        let page = engine
            .list_objects(
                &q.bucket,
                &q.prefix,
                None,
                cap,
                continuation.as_deref(),
                false,
            )
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;
        for (k, _) in &page.objects {
            // The S3 LIST filter: a key the actor cannot see is left out.
            if !actor.may_see(&q.bucket, k) {
                continue;
            }
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

#[cfg(test)]
mod tests {
    fn names(items: &[(&str, &str)]) -> Vec<String> {
        let owned: Vec<(String, String)> = items
            .iter()
            .map(|(b, k)| (b.to_string(), k.to_string()))
            .collect();
        super::zip_entry_names(&owned)
    }

    #[test]
    fn zip_skip_report_never_hides_missing_files() {
        let skip = |k: &str| (k.to_string(), "not found".to_string());
        assert_eq!(super::zip_skip_report(3, &[]), Ok(None));
        // Nothing readable: an error, not an empty 22-byte archive.
        let err = super::zip_skip_report(2, &[skip("b/x"), skip("b/y")]).unwrap_err();
        assert!(err.contains("None of the 2"), "{err}");
        // Partial: the archive carries the list of what is missing.
        let report = super::zip_skip_report(3, &[skip("b/x")]).unwrap().unwrap();
        assert!(report.starts_with("1 of 3 selected files"), "{report}");
        assert!(report.contains("b/x: not found"), "{report}");
    }

    #[test]
    fn zip_skip_report_name_never_collides_with_a_selected_file() {
        assert_eq!(
            super::zip_skip_report_name(&["a.txt"]),
            "_deltaglider-skipped-files.txt"
        );
        assert_eq!(
            super::zip_skip_report_name(&[
                "_deltaglider-skipped-files.txt",
                "_deltaglider-skipped-files-2.txt"
            ]),
            "_deltaglider-skipped-files-3.txt"
        );
    }

    #[test]
    fn zip_all_failed_status_follows_the_cause() {
        use super::ZipFailure::*;
        use axum::http::StatusCode;
        let st = super::zip_all_failed_status;
        assert_eq!(st(&[NotFound, NotFound]), StatusCode::NOT_FOUND);
        assert_eq!(st(&[NotFound, AccessDenied]), StatusCode::FORBIDDEN);
        assert_eq!(st(&[NotFound, Other]), StatusCode::BAD_GATEWAY);
        assert_eq!(st(&[TooLarge, Other]), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(st(&[Overloaded, NotFound]), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(st(&[AccessDenied, TooLarge]), StatusCode::FORBIDDEN);
        assert_eq!(st(&[Other]), StatusCode::BAD_GATEWAY);
        assert_eq!(st(&[]), StatusCode::BAD_GATEWAY);

        use crate::deltaglider::EngineError;
        use crate::storage::StorageError;
        let kind = super::zip_failure_kind;
        assert_eq!(kind(&EngineError::NotFound("k".into())), NotFound);
        assert_eq!(
            kind(&EngineError::Storage(StorageError::NotFound("k".into()))),
            NotFound
        );
        assert_eq!(
            kind(&EngineError::Storage(StorageError::S3(
                "GetObject failed (status=403): AccessDenied".into()
            ))),
            AccessDenied
        );
        assert_eq!(
            kind(&EngineError::Storage(StorageError::Io(
                std::io::Error::from(std::io::ErrorKind::PermissionDenied)
            ))),
            AccessDenied
        );
        assert_eq!(
            kind(&EngineError::Storage(StorageError::S3(
                "GetObject failed (status=500)".into()
            ))),
            Other
        );
        assert_eq!(kind(&EngineError::TooLarge { size: 2, max: 1 }), TooLarge);
        assert_eq!(
            kind(&EngineError::Storage(StorageError::Throttled("x".into()))),
            Overloaded
        );
        assert_eq!(kind(&EngineError::Overloaded("x".into())), Overloaded);
    }

    #[test]
    fn zip_names_keep_structure_and_never_collide() {
        // The reported 500: a top-level README.md next to a folder that also
        // holds one.
        assert_eq!(
            names(&[
                ("b", "rel/README.md"),
                ("b", "rel/docs/N.md"),
                ("b", "README.md")
            ]),
            ["rel/README.md", "rel/docs/N.md", "README.md"]
        );
        // Selecting inside a folder drops the shared folder path.
        assert_eq!(
            names(&[("b", "fw/v1/a.tar"), ("b", "fw/v1/sub/b.tar")]),
            ["a.tar", "sub/b.tar"]
        );
        // Shared directory is by whole segment, not by characters.
        assert_eq!(names(&[("b", "fw1/a"), ("b", "fw2/a")]), ["fw1/a", "fw2/a"]);
        assert_eq!(names(&[("b", "one/x.bin")]), ["x.bin"]);
        // Two buckets: the bucket leads, so same keys stay distinct.
        assert_eq!(
            names(&[("a", "k.txt"), ("b", "k.txt")]),
            ["a/k.txt", "b/k.txt"]
        );
    }

    use super::{dest_key, is_same_location_move};

    #[test]
    fn dest_key_joins_prefix_and_relative() {
        assert_eq!(dest_key("", "a/b.zip"), "a/b.zip");
        assert_eq!(dest_key("backups/", "a/b.zip"), "backups/a/b.zip");
    }

    #[test]
    fn same_location_move_is_detected_and_blocks_delete() {
        // Same bucket, dest_prefix + relative == source_key → self no-op.
        // The source MUST NOT be deleted (this is the data-loss case).
        assert!(is_same_location_move(
            "beshu",
            "beshu",
            "ror/builds/",
            "ror/builds/app.zip",
            "app.zip",
        ));
        // Empty dest_prefix, relative IS the full source key → still self.
        assert!(is_same_location_move(
            "beshu", "beshu", "", "app.zip", "app.zip",
        ));
    }

    #[test]
    fn genuine_relocations_are_not_flagged() {
        // Different bucket → real move.
        assert!(!is_same_location_move(
            "beshu",
            "archive",
            "ror/builds/",
            "ror/builds/app.zip",
            "app.zip",
        ));
        // Same bucket, different dest prefix → real move.
        assert!(!is_same_location_move(
            "beshu",
            "beshu",
            "ror/old/",
            "ror/builds/app.zip",
            "app.zip",
        ));
        // Same bucket, dest key differs from source key → real move.
        assert!(!is_same_location_move(
            "beshu",
            "beshu",
            "ror/builds/",
            "ror/staging/app.zip",
            "app.zip",
        ));
    }
}
