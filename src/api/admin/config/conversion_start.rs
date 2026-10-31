// SPDX-License-Identifier: BUSL-1.1

//! The apply that makes existing buckets start to encrypt starts their
//! conversion (`docs/plan/encryption-conversion-marker.md`, section 4).
//!
//! For each bucket that the change makes encrypt with a proxy key, that
//! could hold plaintext a client reads ([`Config::buckets_starting_conversion`])
//! and that holds at least one object:
//!
//! 1. Pre-commit, in the transition: refuse (409) when a job other than a
//!    delete is active on the bucket ([`refuse_active_jobs`]); open the
//!    conversion window with the config the new engine is built with,
//!    8 buckets at once, reusing a marker that is there ([`open`]). Any
//!    failure removes the markers that this apply wrote ([`compensate`])
//!    and fails the apply.
//! 2. The config commits.
//! 3. Post-commit, after the config write lock is released: one
//!    re-encrypt job per bucket ([`enqueue`]). A lost race (another job
//!    became active) is a warning: the window is open, and the admin view
//!    offers Resume.
//!
//! An empty bucket starts nothing: it holds no history. The validate
//! endpoints run [`plan`] too, with an object count for the dialog.

use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use futures::future::join_all;
use serde::Serialize;

use super::transition::TransitionError;
use crate::api::handlers::AppState;
use crate::config::Config;
use crate::deltaglider::DynEngine;
use crate::storage::encrypting::conversion::{
    self as marker, ByIdentity, ConversionOp, ConversionReply, OpenOutcome, RunId,
};
use crate::storage::StorageBackend;

/// How many buckets the apply probes, opens or closes at once.
pub(crate) const PARALLEL: usize = 8;

/// The most objects a validate counts per bucket (the dialog says "more
/// than" beyond it).
pub(crate) const COUNT_CAP: u64 = 10_000;

/// How long the apply waits for the jobs DB under the config write lock.
/// A task that holds the jobs DB may wait for the config: the wait is
/// bounded, so that never deadlocks, and the apply fails instead.
const JOBS_DB_WAIT: Duration = Duration::from_secs(5);

/// One bucket that a change makes start a conversion, as a validate
/// reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConversionPreview {
    pub bucket: String,
    /// The objects counted, at most [`COUNT_CAP`].
    pub objects: u64,
    /// The bucket holds more than `objects`.
    pub more: bool,
    /// The backend keeps old versions of the bucket.
    pub versioned: bool,
}

/// One conversion that an apply started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConversionStarted {
    pub bucket: String,
    /// The run id of the marker (hex): shared by every instance.
    pub run: String,
    /// The marker was there before this apply (another node, an earlier
    /// apply, or a window whose deadline passed: the job's resume reopens
    /// it with its original cutoff).
    pub reused: bool,
    /// The re-encrypt job on this node; `None` when it could not be
    /// queued (the response's warnings say why).
    pub job_id: Option<i64>,
    /// The backend keeps old versions of the bucket.
    pub versioned: bool,
    #[serde(skip)]
    pub(crate) run_id: RunId,
}

/// Pure: can the change from `old` to `new` start a conversion at all?
/// Only a change of the storage layout or of an encryption block can, so
/// every other apply skips the listing.
fn may_start(old: &Config, new: &Config) -> bool {
    old.backend != new.backend
        || old.backend_encryption != new.backend_encryption
        || old.backends != new.backends
        || old.default_backend != new.default_backend
        || old.buckets != new.buckets
}

/// The buckets that the change from `old` to `new` makes start a
/// conversion and that hold at least one object, as `engine` lists them,
/// with up to `count_cap` objects counted (1: the apply only needs to know
/// that one exists). Sorted by bucket.
pub(crate) async fn plan(
    engine: &DynEngine,
    old: &Config,
    new: &Config,
    count_cap: u64,
) -> Result<Vec<ConversionPreview>, String> {
    if !may_start(old, new) {
        return Ok(Vec::new());
    }
    let listed = engine.list_bucket_origins().await.map_err(|e| {
        format!(
            "the change makes buckets encrypt, and the proxy could not list the buckets to find \
             the ones that hold objects: {e}"
        )
    })?;
    let registry = engine.bucket_policy_registry();
    let pairs: Vec<(String, Option<String>)> = listed
        .into_iter()
        .filter(|b| !registry.is_reserved(&b.name))
        .map(|b| (b.name, b.backend_name))
        .collect();
    let names = new.buckets_starting_conversion(old, &pairs);
    // Chunks of PARALLEL with named async fns: the future stays `Send`
    // for every lifetime (a stream of borrowing closures is not, and the
    // migrate worker spawns this transition).
    let mut results = Vec::new();
    for chunk in names.chunks(PARALLEL) {
        results.extend(join_all(chunk.iter().map(|b| preview_one(engine, b, count_cap))).await);
    }
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for r in results {
        match r {
            Ok(p) if p.objects > 0 => out.push(p),
            Ok(_) => {}
            Err(e) => errors.push(e),
        }
    }
    if !errors.is_empty() {
        errors.sort();
        return Err(errors.join("; "));
    }
    out.sort_by(|a, b| a.bucket.cmp(&b.bucket));
    Ok(out)
}

/// One bucket of [`plan`].
async fn preview_one(
    engine: &DynEngine,
    bucket: &str,
    count_cap: u64,
) -> Result<ConversionPreview, String> {
    let (objects, more) = count_objects(engine, bucket, count_cap).await?;
    let versioned = objects > 0
        && engine
            .storage()
            .bucket_versioning(bucket)
            .await
            .unwrap_or(false);
    Ok(ConversionPreview {
        bucket: bucket.to_string(),
        objects,
        more,
        versioned,
    })
}

/// The objects of `bucket`, counted up to `cap`, and whether there are
/// more.
async fn count_objects(engine: &DynEngine, bucket: &str, cap: u64) -> Result<(u64, bool), String> {
    let mut count = 0u64;
    let mut token: Option<String> = None;
    loop {
        let want = (cap - count).clamp(1, 1000) as u32;
        let page = engine
            .list_objects(bucket, "", None, want, token.as_deref(), false)
            .await
            .map_err(|e| {
                format!(
                    "bucket '{bucket}': the proxy could not list its objects to find out \
                     whether it holds plaintext history: {e}"
                )
            })?;
        count += page.objects.len() as u64;
        let more = page.is_truncated && page.next_continuation_token.is_some();
        if !more {
            return Ok((count, false));
        }
        if count >= cap {
            return Ok((count, true));
        }
        token = page.next_continuation_token;
    }
}

/// The versioning warning of a bucket that starts a conversion.
pub(crate) fn versioning_warning(bucket: &str) -> String {
    format!(
        "bucket '{bucket}' keeps old versions on its backend (versioning is enabled or \
         suspended): the job converts the current objects, and their plaintext versions stay \
         on the backend as noncurrent versions. Delete those versions on the backend if they \
         must go"
    )
}

/// Pure: the refusal of a conversion on `bucket`, which has the active
/// job `job`.
fn active_job_refusal(bucket: &str, job: &crate::maintenance::store::MaintenanceJob) -> String {
    format!(
        "bucket '{bucket}' has an active {} job (maintenance:{}, {}). This change makes the \
         bucket encrypt, and the conversion of its plaintext history needs a re-encrypt job of \
         its own. Wait for the job to finish, or cancel it, then apply again",
        job.kind, job.id, job.status
    )
}

/// 409 when a job other than a delete is active on one of `buckets`.
/// `bounded`: the caller holds the config write lock, so the wait for the
/// jobs DB is bounded (and the apply fails when it ends).
pub(crate) async fn refuse_active_jobs(
    app: &AppState,
    buckets: &[&str],
    bounded: bool,
) -> Result<(), TransitionError> {
    let Some(db) = app.jobs_db.as_ref().filter(|_| !buckets.is_empty()) else {
        return Ok(());
    };
    let db = if bounded {
        tokio::time::timeout(JOBS_DB_WAIT, db.lock())
            .await
            .map_err(|_| TransitionError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: "the jobs database stayed busy, so the proxy could not check the \
                          buckets that start to encrypt for active jobs. Try again"
                    .into(),
            })?
    } else {
        db.lock().await
    };
    let mut refusals = Vec::new();
    for bucket in buckets {
        match db.maintenance_active_job_for_bucket(bucket) {
            Ok(Some(job)) => refusals.push(active_job_refusal(bucket, &job)),
            Ok(None) => {}
            Err(e) => {
                return Err(TransitionError {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    message: format!("could not read the active jobs of bucket '{bucket}': {e}"),
                })
            }
        }
    }
    if refusals.is_empty() {
        Ok(())
    } else {
        Err(TransitionError::conflict(refusals.join("; ")))
    }
}

/// Pure: what an apply makes of one bucket's open-as-built answer.
/// `Ok((run, reused))`, or the error that fails the apply.
fn opened_from(bucket: &str, reply: ConversionReply) -> Result<(RunId, bool), String> {
    match reply {
        ConversionReply::OpenedAsBuilt(OpenOutcome::Created(f, _)) => Ok((f.run, false)),
        ConversionReply::OpenedAsBuilt(OpenOutcome::Reused(f, _))
        | ConversionReply::OpenedAsBuilt(OpenOutcome::Expired(f)) => Ok((f.run, true)),
        ConversionReply::OpenedAsBuilt(OpenOutcome::Foreign) => Err(format!(
            "bucket '{bucket}' holds a conversion marker ({}) that this proxy cannot open (another \
             key, another format, or an edited marker). The proxy never overwrites it. Remove \
             the marker on the backend if no other proxy converts the bucket, then apply again",
            crate::maintenance::conversion::MARKER_KEY
        )),
        other => Err(format!(
            "bucket '{bucket}': unexpected answer to the conversion open: {other:?}"
        )),
    }
}

/// Open the conversion window of each planned bucket with the config that
/// `engine` (not yet published) is built with, [`PARALLEL`] at once. On
/// any failure the markers that this call wrote are removed, and the
/// error names every bucket that failed.
pub(crate) async fn open(
    engine: &DynEngine,
    planned: &[ConversionPreview],
    headers: &HeaderMap,
) -> Result<Vec<ConversionStarted>, String> {
    let window_secs = marker::default_window_secs();
    let mut results = Vec::new();
    for chunk in planned.chunks(PARALLEL) {
        results.extend(join_all(chunk.iter().map(|p| open_one(engine, p, window_secs))).await);
    }
    let mut started = Vec::new();
    let mut errors = Vec::new();
    for r in results {
        match r {
            Ok(s) => started.push(s),
            Err(e) => errors.push(e),
        }
    }
    if !errors.is_empty() {
        compensate(engine, &started, headers).await;
        errors.sort();
        return Err(format!(
            "the change makes buckets encrypt, and their conversion could not start (no state \
             changed): {}",
            errors.join("; ")
        ));
    }
    started.sort_by(|a, b| a.bucket.cmp(&b.bucket));
    for s in started.iter().filter(|s| !s.reused) {
        audit(
            "encryption_conversion_opened",
            &format!("run {}", s.run),
            &s.bucket,
            headers,
        );
    }
    Ok(started)
}

/// One bucket of [`open`].
async fn open_one(
    engine: &DynEngine,
    p: &ConversionPreview,
    window_secs: i64,
) -> Result<ConversionStarted, String> {
    let reply = engine
        .storage()
        .conversion(&p.bucket, ConversionOp::OpenAsBuilt { window_secs })
        .await
        .map_err(|e| {
            format!(
                "bucket '{}': the conversion window could not be opened: {e}",
                p.bucket
            )
        })?;
    let (run_id, reused) = opened_from(&p.bucket, reply)?;
    Ok(ConversionStarted {
        bucket: p.bucket.clone(),
        run: marker::run_label(&run_id),
        reused,
        job_id: None,
        versioned: p.versioned,
        run_id,
    })
}

/// Remove the markers that this apply wrote (not the reused ones), by
/// identity, with the keyring that `engine` was built with. A failure is
/// logged: the marker then closes at its deadline.
pub(crate) async fn compensate(
    engine: &DynEngine,
    started: &[ConversionStarted],
    headers: &HeaderMap,
) {
    let written: Vec<&ConversionStarted> = started.iter().filter(|s| !s.reused).collect();
    for chunk in written.chunks(PARALLEL) {
        join_all(chunk.iter().map(|s| close_one(engine, s, headers))).await;
    }
}

/// One marker of [`compensate`].
async fn close_one(engine: &DynEngine, s: &ConversionStarted, headers: &HeaderMap) {
    let closed = engine
        .storage()
        .conversion(&s.bucket, ConversionOp::CloseAsBuilt { run: s.run_id })
        .await;
    match closed {
        Ok(ConversionReply::Changed(ByIdentity::Done(None, _)))
        | Ok(ConversionReply::Changed(ByIdentity::Absent)) => audit(
            "encryption_conversion_closed",
            &format!("run {}: the apply that opened it failed", s.run),
            &s.bucket,
            headers,
        ),
        other => tracing::error!(
            "config apply: the conversion marker of bucket '{}' (run {}) that the failed apply \
             opened was not removed ({other:?}); it closes at its deadline",
            s.bucket,
            s.run
        ),
    }
}

/// Post-commit: queue one re-encrypt job per started conversion, and fill
/// its `job_id`. A bucket where another job became active (a lost race),
/// or a node without a jobs DB, gets a warning: its window is open, and
/// the admin view offers Resume. Runs after the config write lock is
/// released.
pub(crate) async fn enqueue(
    app: &AppState,
    started: &mut [ConversionStarted],
    warnings: &mut Vec<String>,
    headers: &HeaderMap,
) {
    if started.is_empty() {
        return;
    }
    let Some(db) = app.jobs_db.as_ref() else {
        for s in started.iter() {
            warnings.push(no_job_warning(
                &s.bucket,
                "this instance has no jobs database",
            ));
        }
        return;
    };
    let now = crate::maintenance::store::current_unix_seconds();
    {
        let db = db.lock().await;
        for s in started.iter_mut() {
            match db.maintenance_create_job("reencrypt", &s.bucket, "counting", None, "config", now)
            {
                Ok(Some(id)) => s.job_id = Some(id),
                Ok(None) => warnings.push(no_job_warning(
                    &s.bucket,
                    "another job became active on the bucket during the apply",
                )),
                Err(e) => warnings.push(no_job_warning(
                    &s.bucket,
                    &format!("the job could not be created: {e}"),
                )),
            }
        }
        // Gate from CREATION: no create-to-claim window for writes.
        app.maintenance_gate.sync_from(&db);
    }
    let queued: Vec<&str> = started
        .iter()
        .filter(|s| s.job_id.is_some())
        .map(|s| s.bucket.as_str())
        .collect();
    if !queued.is_empty() {
        crate::maintenance::wake_workers(&app.maintenance_notify);
        crate::audit::audit_log(
            "maintenance_reencrypt_requested",
            "config",
            &queued.join(","),
            headers,
            "",
            "",
        );
    }
}

fn no_job_warning(bucket: &str, why: &str) -> String {
    format!(
        "the conversion window of bucket '{bucket}' is open, but no re-encrypt job was queued \
         ({why}). Its plaintext history stays readable until the window's deadline. Start the \
         job with Resume on the bucket (POST /_/api/admin/jobs/reencrypt)"
    )
}

fn audit(action: &str, target: &str, bucket: &str, headers: &HeaderMap) {
    crate::audit::audit_log(action, "admin", target, headers, bucket, "");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::encrypting::conversion::Fields;

    fn fields(run: u8) -> Fields {
        Fields {
            cutoff: 10,
            deadline: 20,
            run: [run; 16],
        }
    }

    #[test]
    fn a_created_marker_is_ours_to_remove_and_a_found_one_is_reused() {
        let created = ConversionReply::OpenedAsBuilt(OpenOutcome::Created(fields(1), "v".into()));
        assert_eq!(opened_from("releases", created), Ok(([1; 16], false)));
        let reused = ConversionReply::OpenedAsBuilt(OpenOutcome::Reused(fields(2), "v".into()));
        assert_eq!(opened_from("releases", reused), Ok(([2; 16], true)));
        // A window whose deadline passed: the job's resume reopens it.
        let expired = ConversionReply::OpenedAsBuilt(OpenOutcome::Expired(fields(3)));
        assert_eq!(opened_from("releases", expired), Ok(([3; 16], true)));
        let foreign = opened_from(
            "releases",
            ConversionReply::OpenedAsBuilt(OpenOutcome::Foreign),
        )
        .unwrap_err();
        assert!(foreign.contains("cannot open"), "{foreign}");
        assert!(opened_from("releases", ConversionReply::NotEncrypted).is_err());
    }

    #[test]
    fn only_a_layout_or_encryption_change_lists_buckets() {
        let old = Config::default();
        let mut log = old.clone();
        log.log_level = "warn".into();
        assert!(!may_start(&old, &log));
        let mut buckets = old.clone();
        buckets
            .buckets
            .insert("releases".into(), Default::default());
        assert!(may_start(&old, &buckets));
    }
}
