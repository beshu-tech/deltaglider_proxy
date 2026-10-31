// SPDX-License-Identifier: BUSL-1.1

//! The apply that makes existing buckets start (or stop) to encrypt
//! starts (or ends) their conversion
//! (`docs/plan/encryption-conversion-marker.md`, section 4).
//!
//! For each bucket that the change makes encrypt with a proxy key, that
//! could hold plaintext a client reads ([`Config::buckets_starting_conversion`])
//! and that holds at least one object:
//!
//! 1. Pre-commit, in the transition: refuse (409) when a job other than a
//!    delete is active on the bucket ([`refuse_active_jobs`]); open the
//!    conversion window with the config the new engine is built with,
//!    [`PARALLEL`] buckets at once ([`open`]). A marker of this bucket that
//!    is still there (open, expired, or a leftover) keeps its run id but
//!    gets a new cutoff and deadline ([`OpenMode::Refresh`]): an old
//!    cutoff would refuse what the bucket stored as plaintext since. Any
//!    failure removes the markers that this apply created ([`compensate`])
//!    and fails the apply.
//! 2. The config commits.
//! 3. Post-commit, after the config write lock is released: one
//!    re-encrypt job per bucket with the marker's run ([`enqueue`]). A lost
//!    race (another job became active) is a warning: the window is open,
//!    and the admin view offers Resume.
//!
//! Each node's apply queues its own job, also when another node converts
//! the bucket already: the rewrites are idempotent (a sealed object is
//! skipped), and the first job that ends closes the marker of the run.
//!
//! A change that makes a bucket STOP encrypting closes its marker by
//! identity, any run, with the engine that encrypted it
//! ([`close_stopped`]). An empty bucket starts nothing: it holds no
//! history. The validate endpoints run [`plan`] too, with an object count
//! for the dialog.

use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use futures::future::join_all;
use serde::Serialize;

use super::transition::TransitionError;
use crate::api::handlers::AppState;
use crate::config::Config;
use crate::deltaglider::DynEngine;
use crate::maintenance::conversion::{NotQueued, QueuedReencrypt, ReencryptParams};
use crate::storage::encrypting::conversion::{
    self as marker, ByIdentity, ConversionOp, ConversionReply, OpenMode, OpenOutcome, RunId,
};
use crate::storage::StorageBackend;

/// How many buckets the apply probes, opens or closes at once (also the
/// admin bucket list's marker reads).
pub(crate) const PARALLEL: usize = 8;

/// The most objects a validate counts per bucket (the dialog says "more
/// than" beyond it).
pub(crate) const COUNT_CAP: u64 = 10_000;

/// How long the apply waits for the jobs DB under the config write lock.
/// A task that holds the jobs DB may wait for the config: the wait is
/// bounded, so that never deadlocks, and the apply fails instead.
const JOBS_DB_WAIT: Duration = Duration::from_secs(5);

/// `f` over `items`, [`PARALLEL`] at once, the results in order. Chunks of
/// named async fns: the future stays `Send` for every lifetime (a stream
/// of borrowing closures is not, and the migrate worker spawns the
/// transition).
pub(crate) async fn in_parallel<'a, T, R, F, Fut>(items: &'a [T], f: F) -> Vec<R>
where
    F: Fn(&'a T) -> Fut,
    Fut: std::future::Future<Output = R>,
{
    let mut out = Vec::with_capacity(items.len());
    for chunk in items.chunks(PARALLEL) {
        out.extend(join_all(chunk.iter().map(&f)).await);
    }
    out
}

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
    /// The backend did not say whether it keeps old versions (the error).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub versioning_error: Option<String>,
}

/// One conversion that an apply started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConversionStarted {
    pub bucket: String,
    /// The run id of the marker (hex): shared by every instance.
    pub run: String,
    /// The marker was there before this apply (another node, an earlier
    /// apply, a leftover): its run id stays, its cutoff and deadline are
    /// this apply's.
    pub reused: bool,
    /// The re-encrypt job on this node; `None` when it could not be
    /// queued (the response's warnings say why).
    pub job_id: Option<i64>,
    /// The backend keeps old versions of the bucket.
    pub versioned: bool,
    /// The job deletes the noncurrent plaintext versions after it
    /// converted every object ([`PURGE_HEADER`]; a versioned bucket only).
    pub purge_plaintext_versions: bool,
    #[serde(skip)]
    pub(crate) run_id: RunId,
    #[serde(skip)]
    pub(crate) versioning_error: Option<String>,
}

/// The request header of a config write that asks the re-encrypt jobs it
/// queues to delete the noncurrent plaintext versions of the versioned
/// buckets (`true`). The admin UI sends it when the operator ticks the box
/// in the conversion dialog.
pub const PURGE_HEADER: &str = "x-dgp-purge-plaintext-versions";

/// Pure: does a write with `headers` ask for the purge?
pub(crate) fn purge_requested(headers: &HeaderMap) -> bool {
    headers
        .get(PURGE_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
}

/// Pure: can the change from `old` to `new` start or stop a conversion at
/// all? Only a change of the storage layout or of an encryption block
/// can, so every other apply skips the listing.
fn may_start(old: &Config, new: &Config) -> bool {
    old.backend != new.backend
        || old.backend_encryption != new.backend_encryption
        || old.backends != new.backends
        || old.default_backend != new.default_backend
        || old.buckets != new.buckets
}

/// What a change does to the conversions of the listed buckets.
#[derive(Debug, Default)]
pub(crate) struct Planned {
    /// The buckets that start a conversion and hold an object, sorted.
    pub starting: Vec<ConversionPreview>,
    /// The buckets that stop encrypting, sorted: their marker closes.
    pub stopping: Vec<String>,
}

/// The conversions that the change from `old` to `new` starts (buckets
/// that hold at least one object, with up to `count_cap` objects counted;
/// 1: the apply only needs to know that one exists) and stops, as
/// `engine` lists the buckets.
pub(crate) async fn plan_all(
    engine: &DynEngine,
    old: &Config,
    new: &Config,
    count_cap: u64,
) -> Result<Planned, String> {
    if !may_start(old, new) {
        return Ok(Planned::default());
    }
    let listed = engine.list_bucket_origins().await.map_err(|e| {
        format!(
            "the change moves the encryption of buckets, and the proxy could not list the \
             buckets to find the ones that hold objects: {e}"
        )
    })?;
    let registry = engine.bucket_policy_registry();
    let pairs: Vec<(String, Option<String>)> = listed
        .into_iter()
        .filter(|b| !registry.is_reserved(&b.name))
        .map(|b| (b.name, b.backend_name))
        .collect();
    let names = new.buckets_starting_conversion(old, &pairs);
    let mut stopping = new.buckets_stopping_encryption(old, &pairs);
    stopping.sort();
    let results = in_parallel(&names, |b| preview_one(engine, b, count_cap)).await;
    let mut starting = Vec::new();
    let mut errors = Vec::new();
    for r in results {
        match r {
            Ok(p) if p.objects > 0 => starting.push(p),
            Ok(_) => {}
            Err(e) => errors.push(e),
        }
    }
    if !errors.is_empty() {
        errors.sort();
        return Err(errors.join("; "));
    }
    starting.sort_by(|a, b| a.bucket.cmp(&b.bucket));
    Ok(Planned { starting, stopping })
}

/// The conversions that a change starts ([`plan_all`]): the validate.
pub(crate) async fn plan(
    engine: &DynEngine,
    old: &Config,
    new: &Config,
    count_cap: u64,
) -> Result<Vec<ConversionPreview>, String> {
    Ok(plan_all(engine, old, new, count_cap).await?.starting)
}

/// One bucket of [`plan_all`].
async fn preview_one(
    engine: &DynEngine,
    bucket: &str,
    count_cap: u64,
) -> Result<ConversionPreview, String> {
    let (objects, more) = count_objects(engine, bucket, count_cap).await?;
    let (versioned, versioning_error) = if objects == 0 {
        (false, None)
    } else {
        match engine.storage().bucket_versioning(bucket).await {
            Ok(v) => (v, None),
            Err(e) => (false, Some(e.to_string())),
        }
    };
    Ok(ConversionPreview {
        bucket: bucket.to_string(),
        objects,
        more,
        versioned,
        versioning_error,
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

/// THE versioning warning of a bucket that starts a conversion: the
/// backend keeps old versions, or did not say (`error`); `None`: neither.
fn versioning_note(bucket: &str, versioned: bool, error: Option<&str>) -> Option<String> {
    if let Some(e) = error {
        return Some(format!(
            "bucket '{bucket}': the proxy could not find out whether the backend keeps old \
             versions of it ({e}). If it does, the plaintext versions stay on the backend as \
             noncurrent versions after the job converts the current objects"
        ));
    }
    versioned.then(|| {
        format!(
            "bucket '{bucket}' keeps old versions on its backend (versioning is enabled or \
             suspended): the job converts the current objects, and their plaintext versions \
             stay on the backend as noncurrent versions, unless the write asks the job to \
             delete them (the header X-DGP-Purge-Plaintext-Versions: true, or the box in the \
             admin UI's dialog). A deleted version cannot be restored"
        )
    })
}

impl ConversionPreview {
    /// The versioning warning of this bucket, if any.
    pub(crate) fn versioning_warning(&self) -> Option<String> {
        versioning_note(
            &self.bucket,
            self.versioned,
            self.versioning_error.as_deref(),
        )
    }
}

impl ConversionStarted {
    /// The versioning warning of this bucket, if any.
    pub(crate) fn versioning_warning(&self) -> Option<String> {
        versioning_note(
            &self.bucket,
            self.versioned,
            self.versioning_error.as_deref(),
        )
    }
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

/// Pure: what an apply makes of one bucket's open answer.
/// `Ok((run, reused))`, or the error that fails the apply.
fn opened_from(bucket: &str, reply: ConversionReply) -> Result<(RunId, bool), String> {
    match reply {
        ConversionReply::Opened(OpenOutcome::Created(f, _)) => Ok((f.run, false)),
        ConversionReply::Opened(OpenOutcome::Refreshed(f, _) | OpenOutcome::Reused(f, _)) => {
            Ok((f.run, true))
        }
        ConversionReply::Opened(OpenOutcome::Foreign) => Err(format!(
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
/// `engine` (not yet published) is built with, [`PARALLEL`] at once, in
/// [`OpenMode::Refresh`]. On any failure the markers that this call
/// created are removed, and the error names every bucket that failed.
pub(crate) async fn open(
    engine: &DynEngine,
    planned: &[ConversionPreview],
    headers: &HeaderMap,
) -> Result<Vec<ConversionStarted>, String> {
    let window_secs = marker::default_window_secs();
    let results = in_parallel(planned, |p| open_one(engine, p, window_secs)).await;
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
    for s in &started {
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
    let op = ConversionOp::Open {
        window_secs,
        mode: OpenMode::Refresh,
    };
    let reply = engine
        .storage()
        .conversion(&p.bucket, op)
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
        purge_plaintext_versions: false,
        run_id,
        versioning_error: p.versioning_error.clone(),
    })
}

/// Remove the markers that this apply created (not the refreshed ones), by
/// identity, with the keyring that `engine` was built with. A failure is
/// logged: the marker then closes at its deadline.
pub(crate) async fn compensate(
    engine: &DynEngine,
    started: &[ConversionStarted],
    headers: &HeaderMap,
) {
    let written: Vec<&ConversionStarted> = started.iter().filter(|s| !s.reused).collect();
    in_parallel(&written, |s| {
        close_one(
            engine,
            &s.bucket,
            Some(s.run_id),
            "the apply that opened it failed",
            headers,
        )
    })
    .await;
}

/// The buckets that the change stops encrypting: close their markers by
/// identity, any run, with `old_engine` (the engine that encrypted them:
/// its keyring opens the marker). Runs after the commit; a failure is a
/// warning (the marker then closes at its deadline).
pub(crate) async fn close_stopped(
    old_engine: &DynEngine,
    stopping: &[String],
    headers: &HeaderMap,
) -> Vec<String> {
    let results = in_parallel(stopping, |b| {
        close_one(
            old_engine,
            b,
            None,
            "the bucket stopped encrypting",
            headers,
        )
    })
    .await;
    stopping
        .iter()
        .zip(results)
        .filter(|(_, closed)| !closed)
        .map(|(b, _)| {
            format!(
                "bucket '{b}' stopped encrypting, but its conversion marker could not be \
                 removed; the window closes at its deadline"
            )
        })
        .collect()
}

/// Remove the marker of `bucket` by identity (`run: None`: any run). True
/// when no marker of the run is left (removed, or none was there).
async fn close_one(
    engine: &DynEngine,
    bucket: &str,
    run: Option<RunId>,
    why: &str,
    headers: &HeaderMap,
) -> bool {
    let closed = engine
        .storage()
        .conversion(bucket, ConversionOp::CloseAsBuilt { run })
        .await;
    let label = run.map_or_else(|| "any".to_string(), |r| marker::run_label(&r));
    match closed {
        Ok(ConversionReply::Changed(ByIdentity::Done(None, _))) => {
            audit(
                "encryption_conversion_closed",
                &format!("run {label}: {why}"),
                bucket,
                headers,
            );
            true
        }
        Ok(ConversionReply::Changed(ByIdentity::Absent)) => true,
        other => {
            tracing::error!(
                "config apply: the conversion marker of bucket '{bucket}' (run {label}) was not \
                 removed after {why} ({other:?}); it closes at its deadline"
            );
            false
        }
    }
}

/// Post-commit: queue one re-encrypt job per started conversion, with the
/// marker's run, and fill its `job_id`. A bucket where another job became
/// active (a lost race), or a node without a jobs DB, gets a warning: its
/// window is open, and the admin view offers Resume. Runs after the
/// config write lock is released.
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
    let purge = purge_requested(headers);
    let requests: Vec<QueuedReencrypt> = started
        .iter()
        .map(|s| QueuedReencrypt {
            bucket: s.bucket.clone(),
            params: ReencryptParams {
                purge_plaintext_versions: purge && s.versioned,
                run: Some(s.run_id),
            },
        })
        .collect();
    let results =
        crate::maintenance::conversion::queue_reencrypt(app, db, &requests, "config", headers)
            .await;
    for ((s, r), q) in started.iter_mut().zip(results).zip(&requests) {
        // The response says "purge" only for a job that will do it.
        s.purge_plaintext_versions = false;
        match r {
            Ok(id) => {
                s.job_id = Some(id);
                s.purge_plaintext_versions = q.params.purge_plaintext_versions;
            }
            Err(NotQueued::Active) => warnings.push(no_job_warning(
                &s.bucket,
                "another job became active on the bucket during the apply",
            )),
            Err(NotQueued::Failed(e)) => warnings.push(no_job_warning(
                &s.bucket,
                &format!("the job could not be created: {e}"),
            )),
        }
    }
}

fn no_job_warning(bucket: &str, why: &str) -> String {
    format!(
        "the conversion window of bucket '{bucket}' is open, but no re-encrypt job was queued \
         ({why}). Its plaintext history stays readable until the window's deadline. Start the \
         job with Resume on the bucket (POST /_/api/admin/jobs/reencrypt with \
         {{\"buckets\": [\"{bucket}\"], \"open_window\": true}})"
    )
}

fn audit(action: &str, target: &str, bucket: &str, headers: &HeaderMap) {
    crate::audit::audit_log(action, "admin", target, headers, bucket, "");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::encrypting::conversion::Fields;

    #[test]
    fn only_a_true_header_asks_for_the_purge() {
        let with = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(PURGE_HEADER, v.parse().unwrap());
            purge_requested(&h)
        };
        assert!(!purge_requested(&HeaderMap::new()));
        assert!(with("true"));
        assert!(with("TRUE "));
        assert!(!with("false"));
        assert!(!with("1"));
        assert!(!with(""));
    }

    fn fields(run: u8) -> Fields {
        Fields {
            cutoff: 10,
            deadline: 20,
            run: [run; 16],
        }
    }

    #[test]
    fn a_created_marker_is_ours_to_remove_and_a_found_one_is_reused() {
        let created = ConversionReply::Opened(OpenOutcome::Created(fields(1), "v".into()));
        assert_eq!(opened_from("releases", created), Ok(([1; 16], false)));
        // A marker that was there: refreshed, its run kept, never removed by
        // the compensation.
        let refreshed = ConversionReply::Opened(OpenOutcome::Refreshed(fields(2), "v".into()));
        assert_eq!(opened_from("releases", refreshed), Ok(([2; 16], true)));
        let foreign =
            opened_from("releases", ConversionReply::Opened(OpenOutcome::Foreign)).unwrap_err();
        assert!(foreign.contains("cannot open"), "{foreign}");
        assert!(opened_from("releases", ConversionReply::NotEncrypted).is_err());
    }

    #[test]
    fn a_versioning_read_error_is_a_warning_not_silence() {
        assert_eq!(versioning_note("releases", false, None), None);
        let versioned = versioning_note("releases", true, None).unwrap();
        assert!(versioned.contains("keeps old versions"), "{versioned}");
        let unknown = versioning_note("releases", false, Some("AccessDenied")).unwrap();
        assert!(
            unknown.contains("could not find out") && unknown.contains("AccessDenied"),
            "{unknown}"
        );
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
