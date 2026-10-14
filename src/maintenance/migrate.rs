// SPDX-License-Identifier: BUSL-1.1

//! `kind = "migrate"` maintenance jobs: move a bucket between backends as
//! a durable, resumable, WRITE-GATED background job.
//!
//! This replaces the old synchronous admin handler, which ran the whole
//! copy inside one HTTP request with no progress, no resume, and — the
//! real bug — no write gate: a client write landing on the source after
//! its key was copied produced a STALE object on the destination after
//! the flip. Here the gate is armed at job creation (same machinery as
//! re-encryption), freezing the source write-set through the flip.
//!
//! Phases (resumable via `maintenance_jobs.phase` + `continuation_token`):
//!
//! 1. **stage** — insert the transient route `__dgmigrate_<bucket>_<n>` →
//!    `{backend: target, alias: bucket}` (PERSISTED to the config file so
//!    a crash leaves it visible to the boot reconcile), create the real
//!    bucket on the target, drain in-flight source writes.
//! 2. **copy** — paginate the source; HEAD-skip already-copied keys
//!    (idempotent resume); `dg-migration` provenance. ANY copy failure
//!    fails the job — migrate never flips on a partial copy.
//!    Every page RE-ASSERTS the transient route: an admin config apply
//!    mid-job replaces `cfg.buckets` wholesale, and without the route the
//!    copies would land on the DEFAULT backend.
//! 3. **verify** — re-list the source, HEAD every key on the transient.
//! 4. **flip** — one config transaction: real bucket `backend = target`,
//!    transient removed, persisted. The write gate is cleared IMMEDIATELY
//!    after the flip (clients resume against the new backend; the
//!    optional cleanup below doesn't need the gate).
//! 5. **cleanup** — optional `delete_source`: a second transient route to
//!    the OLD backend, delete every copied key through it, remove the
//!    route. Delete failures and a stopped sweep are recorded but do not
//!    fail the migration (the flip already happened): the job settles
//!    `completed_with_errors` with a note.
//!
//! Cancellation: checked every `CANCEL_CHECK_EVERY` objects. Pre-flip →
//! release the source gate, delete the staged copies THIS job made (by
//! their job-stamped `dg-migration` value), unwind the transient route
//! and settle `cancelled` (source untouched, still authoritative). A
//! pre-flip failure unwinds the same way. The flip itself is not
//! interruptible; during cleanup a cancel stops deleting and settles with
//! a note.
//!
//! Single-instance only: the route flip mutates THIS instance's config
//! file + engine, and config sync carries IAM tables, not routing. A peer
//! keeps routing the bucket to the source, its write gate never arms, and
//! cleanup deletes the writes it accepts. So `start_migrate` refuses (409)
//! while a coordination bucket is configured ([`multi_instance_refusal`]).

use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::info;

use crate::api::handlers::AppState;
use crate::config_apply::ConfigMutator;
use crate::config_db::JobsDb;
use crate::deltaglider::{
    CounterPrior, DeleteHooks, DeleteItem, DeleteOutcome, DynEngine, ListObjectsPage, StoredForms,
};
use crate::job_loop::{list_retry_delay, ListFailure, Pager, MAX_JOB_PAGES};
use crate::transfer::{
    content_verdict, copy_object_known, ContentVerdict, ObjectTransferRequest, TransferProvenance,
};

use super::paged::{
    list_job_keys, paged_phase, BackgroundCount, JobCtx, KeyPage, ListError, PageStep, PhaseSpec,
    CANCEL_CHECK_EVERY, PAGE_CONCURRENCY,
};
use super::store::MaintenanceJob;
use super::worker::{
    after_run, check_cancel, current_row, drain_inflight_writes, heartbeat, persist_flushed,
    record_failure, resumable_in, AfterRun, Counters, Holder, PhaseStop,
};

pub const TRANSIENT_PREFIX: &str = "__dgmigrate_";
const PAGE_SIZE: u32 = 1000;

/// What the migrate does with objects that already sit in the destination
/// bucket.
///
/// `Empty` (default): the destination must hold no objects. A destination
/// with objects is usually the safety copy of an earlier move; copying on
/// top of it brings back every object deleted at the source since then.
/// `Mirror`: the destination becomes an exact copy of the source — objects
/// absent at the source are deleted before the flip (audited).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MigrateTarget {
    #[default]
    Empty,
    Mirror,
}

impl MigrateTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Mirror => "mirror",
        }
    }
}

/// Kind-specific parameters carried in `maintenance_jobs.params` (JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrateParams {
    pub target_backend: String,
    pub delete_source: bool,
    pub transient_key: String,
    pub from_backend: String,
    /// Absent in rows written before the option existed → `Empty`.
    #[serde(default)]
    pub target: MigrateTarget,
}

/// Pages the stage phase lists to count the destination (the count in the
/// refusal is "at least N" past this).
const DEST_COUNT_MAX_PAGES: usize = 10;

/// Pure: may a migrate in `mode` start on a destination that holds
/// `existing` objects (`more` = the count stopped at the page cap)?
pub fn destination_check(
    mode: MigrateTarget,
    existing: u64,
    more: bool,
    dest_bucket: &str,
    target_backend: &str,
) -> Result<(), String> {
    if mode == MigrateTarget::Mirror || existing == 0 {
        return Ok(());
    }
    Err(format!(
        "destination bucket '{dest_bucket}' on backend '{target_backend}' already holds \
         {}{existing} object(s). A migrate copies on top of them, so objects deleted at the \
         source since that copy was made come back. Empty the destination first, or start \
         the move again with \"Make the destination an exact mirror\" (API: \"target\": \
         \"mirror\"): objects absent at the source are then deleted from the destination.",
        if more { "at least " } else { "" }
    ))
}

/// Pure: the `dg-migration` provenance value. It carries the job id, so a
/// pre-flip cancel removes exactly the copies THIS job made.
pub fn provenance_value(params: &MigrateParams, job_id: i64) -> String {
    format!(
        "{}->{}#job{job_id}",
        params.from_backend, params.target_backend
    )
}

/// Where the refusal below sends the operator.
pub const MIGRATE_DOC_URL: &str =
    "https://deltaglider.com/docs/how-to/move-a-bucket-between-backends";

/// Pure: refuse a migrate on a multi-instance deployment (a coordination
/// bucket is set). Bucket routing is per-instance YAML; config sync does not
/// carry it, so peers would keep writing to the source that cleanup deletes.
pub fn multi_instance_refusal(config_sync_bucket: Option<&str>) -> Option<String> {
    let bucket = config_sync_bucket.filter(|b| !b.is_empty())?;
    Some(format!(
        "migrate is refused on a multi-instance deployment (config_sync_bucket \
         '{bucket}' is set): the routing flip changes only this instance's \
         config, config sync does not carry routing, so the other instances \
         keep writing to the source that cleanup deletes. Run the move on a \
         single instance, see {MIGRATE_DOC_URL}"
    ))
}

pub fn parse_params(json: &str) -> Result<MigrateParams, String> {
    serde_json::from_str(json).map_err(|e| format!("invalid migrate params: {e}"))
}

/// Phase order. `stage`/`copy`/`verify` are pre-flip (cancel = unwind,
/// source authoritative); `flip`/`cleanup` are post-flip.
pub const PHASES: [&str; 5] = ["stage", "copy", "verify", "flip", "cleanup"];

pub fn is_pre_flip(phase: &str) -> bool {
    matches!(phase, "stage" | "copy" | "verify")
}

/// Should the write gate be ARMED while resuming a migrate at this phase?
///
/// Armed for every phase up to AND INCLUDING `flip`: a crash can persist
/// `phase="flip"` BEFORE the atomic flip actually runs (migrate.rs persists
/// the phase, then executes the flip), so a resume at `flip` may still route
/// the bucket to the SOURCE — client writes there must be gated, or they land
/// on the soon-abandoned backend and are lost (permanently, with
/// delete_source). Only `cleanup` is truly post-flip: the bucket is live on
/// the new backend and gating it would 503 clients during the source sweep.
pub fn gate_armed_during(phase: &str) -> bool {
    phase != "cleanup"
}

/// Should a job that FAILED in `phase` unwind its staging route?
/// Pre-flip phases: yes (source untouched). The flip itself: also yes —
/// `mutate_and_apply_strict` is atomic (rollback on rebuild OR persist
/// failure), so a failed flip leaves the source authoritative and the
/// staging route is just litter. Only `cleanup` failures keep routes
/// alone (the migration already happened; cleanup removes its own).
pub fn unwinds_on_failure(phase: &str) -> bool {
    phase != "cleanup"
}

/// First free `__dgmigrate_<bucket>_<n>` name.
pub fn pick_transient_key(bucket_key: &str, taken: &dyn Fn(&str) -> bool) -> String {
    let mut n = 0u32;
    loop {
        let candidate = format!("{TRANSIENT_PREFIX}{bucket_key}_{n}");
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Transient policies in the config that no ACTIVE migrate job references
/// — boot-reconcile removes exactly these (a crashed-then-resumed job's
/// transient stays for the job to reuse).
pub fn orphaned_transients<'a>(
    config_bucket_keys: impl Iterator<Item = &'a str>,
    active: &HashSet<String>,
) -> Vec<String> {
    config_bucket_keys
        .filter(|k| k.starts_with(TRANSIENT_PREFIX) && !active.contains(*k))
        .map(String::from)
        .collect()
}

/// Pure: the bucket's name ON its backend, with the routing table's rule:
/// an `alias` applies only to a policy with an explicit `backend`. The migrate
/// keeps this real name on the target (the flip pins the alias to it), so the
/// flip finds the copies and the cleanup deletes the real source — never an
/// unrelated bucket that happens to carry the virtual name.
pub fn real_bucket_name<'a>(
    buckets: &'a std::collections::BTreeMap<String, crate::bucket_policy::BucketPolicyConfig>,
    bucket: &'a str,
) -> &'a str {
    match buckets.get(bucket) {
        Some(p) if p.backend.is_some() => p.alias.as_deref().unwrap_or(bucket),
        _ => bucket,
    }
}

/// Proof that the deferred copy writes are durable. Only [`flush_copies`]
/// makes one, and [`persist_flushed`] (the only save this file can reach)
/// takes one, so every migrate save waits for a successful flush.
pub(crate) struct Flushed(());

/// Save progress only after the copies made so far are durable. The copy
/// phase writes without the per-object fsync (`storage::with_deferred_fsync`),
/// so a cursor saved first could point past a copy that a crash loses. A
/// failed flush saves nothing and fails the phase (the source stays
/// authoritative).
async fn save_after_flush<F, S, Fut>(flush: F, save: S) -> Result<(), PhaseStop>
where
    F: std::future::Future<Output = Result<Flushed, crate::deltaglider::EngineError>>,
    S: FnOnce(Flushed) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let flushed = flush
        .await
        .map_err(|e| format!("could not make the copied objects durable: {e}"))?;
    save(flushed).await;
    Ok(())
}

/// Flush the deferred copy writes of the live engine (every backend).
async fn flush_copies(state: &Arc<AppState>) -> Result<Flushed, crate::deltaglider::EngineError> {
    let engine = state.engine.load().clone();
    engine.flush_pending().await?;
    Ok(Flushed(()))
}

/// Which target copies a resumed copy phase must re-copy instead of trusting
/// their metadata. The copies after the last checkpoint were not durable, and
/// a crash can leave such a file with its metadata but without its data. They
/// all lie in the page at the saved token, so the first resumed page is
/// re-copied. A restart from page 0 (poisoned token) never reaches that page
/// first, so it re-copies the whole phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecopyScope {
    None,
    FirstPage,
    WholePhase,
}

impl RecopyScope {
    fn on_resume(resumed_in_copy: bool) -> Self {
        if resumed_in_copy {
            Self::FirstPage
        } else {
            Self::None
        }
    }

    fn on_restart_fresh(self) -> Self {
        match self {
            Self::None => Self::None,
            _ => Self::WholePhase,
        }
    }

    fn after_page(self) -> Self {
        match self {
            Self::FirstPage => Self::None,
            s => s,
        }
    }

    fn recopies(self) -> bool {
        self != Self::None
    }
}

/// HEADs of one key that a migrate sends before it gives up on a
/// transient error.
const HEAD_ATTEMPTS: u32 = 3;

/// A HEAD that did not answer: its object is neither there nor missing.
#[derive(Debug)]
struct HeadFailed {
    /// A timeout, a 5xx or a throttle (after [`HEAD_ATTEMPTS`]).
    transient: bool,
    message: String,
}

impl HeadFailed {
    /// The phase stop: a transient failure keeps the staged copies.
    fn stop(self) -> PhaseStop {
        if self.transient {
            PhaseStop::FailedTransient(self.message)
        } else {
            PhaseStop::Failed(self.message)
        }
    }
}

/// HEAD `bucket/key` (both stored forms): `Ok(None)` only when the object
/// does not exist. A transient error is sent again ([`HEAD_ATTEMPTS`] in
/// all).
async fn head_retrying(
    engine: &DynEngine,
    bucket: &str,
    key: &str,
) -> Result<Option<(crate::types::FileMetadata, StoredForms)>, HeadFailed> {
    let mut attempt = 0u32;
    loop {
        let e = match engine.head_listed(bucket, key, None).await {
            Ok(found) => return Ok(Some(found)),
            Err(e) if e.is_not_found() => return Ok(None),
            Err(e) => e,
        };
        attempt += 1;
        let transient = ListFailure::of(&e) == ListFailure::Transient;
        if transient && attempt < HEAD_ATTEMPTS {
            tokio::time::sleep(std::time::Duration::from_millis(250 * u64::from(attempt))).await;
            continue;
        }
        return Err(HeadFailed {
            transient,
            message: format!("could not check '{key}' in '{bucket}': {e}"),
        });
    }
}

/// Does the target hold the source's version of `key`? Only a target
/// that answers "not found" is `Missing`: a target HEAD that times out
/// (or answers 5xx) is retried, then an error. Any HEAD error read as
/// `Missing`, so one timeout in verify failed the job and deleted every
/// staged copy.
///
/// `source_form`: the one stored form of the source key that the source
/// listing proved (the write gate freezes the source): one HEAD, not two.
async fn copy_verdict_for(
    engine: &DynEngine,
    source_bucket: &str,
    target_bucket: &str,
    key: &str,
    source_form: Option<crate::storage::ObjectVariant>,
) -> Result<ContentVerdict, HeadFailed> {
    copy_check(engine, source_bucket, target_bucket, key, source_form)
        .await
        .map(|check| check.verdict)
}

/// What the skip check of a copy read ([`copy_check`]).
struct CopyCheck {
    verdict: ContentVerdict,
    /// What the target key holds: the copy's prior. Nothing else writes
    /// the target (the staging route is gated from its creation), so it
    /// stays true until the copy writes.
    target: CounterPrior,
    /// The source's metadata, when the check read it.
    source: Option<crate::types::FileMetadata>,
}

/// [`copy_verdict_for`], and what it read.
async fn copy_check(
    engine: &DynEngine,
    source_bucket: &str,
    target_bucket: &str,
    key: &str,
    source_form: Option<crate::storage::ObjectVariant>,
) -> Result<CopyCheck, HeadFailed> {
    let Some((dst, forms)) = head_retrying(engine, target_bucket, key).await? else {
        return Ok(CopyCheck {
            verdict: ContentVerdict::Missing,
            target: CounterPrior::Absent,
            source: None,
        });
    };
    let (verdict, source) = match engine.head_listed(source_bucket, key, source_form).await {
        Ok((src, _)) => (content_verdict(&src, Some(&dst)), Some(src)),
        // The copy (or verify) reports the source error itself.
        Err(_) => (ContentVerdict::Unknown, None),
    };
    Ok(CopyCheck {
        verdict,
        target: CounterPrior::Present {
            object: Box::new(dst),
            forms,
        },
        source,
    })
}

/// One page of `bucket` (all keys, no metadata), a transient error retried
/// with the same token like `paged_phase` does.
async fn list_page_retrying(
    engine: &DynEngine,
    bucket: &str,
    token: Option<&str>,
    context: &str,
) -> Result<ListObjectsPage, ListError> {
    let mut attempt = 0u32;
    loop {
        let e = match engine
            .list_objects(bucket, "", None, PAGE_SIZE, token, false)
            .await
        {
            Ok(page) => return Ok(page),
            Err(e) => ListError::engine(context, &e),
        };
        attempt += 1;
        match list_retry_delay(e.failure, attempt) {
            Some(pause) => tokio::time::sleep(pause).await,
            None => return Err(e),
        }
    }
}

/// Delete `keys` of `bucket` through the engine's batch: one reclaim check
/// per folder, several folders at once (a per-key delete listed the folder
/// after every key). `proceed` stops the batch between keys (cancel,
/// shutdown). Returns each key's outcome, in order.
async fn delete_keys<'a>(
    engine: &DynEngine,
    bucket: &str,
    items: Vec<DeleteItem<'a>>,
    proceed: &(dyn Fn() -> bool + Send + Sync),
) -> Vec<DeleteOutcome> {
    engine
        .delete_batch(
            bucket,
            items,
            DeleteHooks {
                proceed: Some(proceed),
                on_outcome: None,
            },
        )
        .await
}

/// A cancel request seen while a batch runs: a task polls the job row
/// once a second and sets the flag that the batch's `proceed` hook reads
/// (the hook cannot wait for the DB). Stops polling when dropped.
struct CancelWatch {
    requested: Arc<std::sync::atomic::AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl CancelWatch {
    fn spawn(db: &Arc<Mutex<JobsDb>>, job_id: i64) -> Self {
        let requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (flag, db) = (requested.clone(), db.clone());
        let task = tokio::spawn(async move {
            loop {
                if check_cancel(&db, job_id).await.is_err() {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        });
        Self { requested, task }
    }

    fn requested(&self) -> bool {
        self.requested.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for CancelWatch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Pure: how a post-flip cleanup settles, or `None` for a clean sweep (the
/// worker then settles from the row counters). The migration already
/// happened, so every outcome is a `completed*` status with a note: a failed
/// delete or a stopped sweep leaves source objects behind (`_with_errors`).
fn cleanup_settlement(
    cancelled: bool,
    delete_failures: u32,
    stopped: Option<&str>,
) -> Option<(&'static str, String)> {
    let failures = if delete_failures > 0 {
        format!(" ({delete_failures} failure(s))")
    } else {
        String::new()
    };
    let note = if let Some(reason) = stopped {
        format!("source cleanup stopped{failures}: {reason} — remaining source objects can be removed manually")
    } else if cancelled {
        format!("source cleanup stopped by cancel{failures} — remaining source objects can be removed manually")
    } else if delete_failures > 0 {
        format!("source cleanup incomplete{failures} — remaining source objects can be removed manually")
    } else {
        return None;
    };
    let status = if delete_failures > 0 || stopped.is_some() {
        "completed_with_errors"
    } else {
        "completed"
    };
    Some((status, note))
}

/// Ensure the transient (or cleanup) route to `bucket`'s real name on
/// `backend` exists in the live config — idempotent; re-run per page
/// because a config apply can wipe it.
async fn ensure_route(
    mutator: &ConfigMutator,
    route_key: &str,
    backend: &str,
    bucket: &str,
    context: &str,
) -> Result<(), String> {
    let (present, route) = {
        let cfg = mutator.read().await;
        let real = real_bucket_name(&cfg.buckets, bucket).to_string();
        let route = route_policy(cfg.buckets.get(bucket), backend, &real);
        let present = cfg
            .buckets
            .get(route_key)
            .is_some_and(|p| same_route(p, &route));
        (present, route)
    };
    if present {
        return Ok(());
    }
    let route_key = route_key.to_string();
    mutator
        .mutate_and_apply(context, move |cfg| {
            cfg.buckets.insert(route_key, route);
        })
        .await
}

/// Pure: the policy of a route to the real bucket `alias` on `backend`.
/// It stores objects as the bucket `source` does (its compression and its
/// delta ratio): with the defaults, a migrate of a bucket that has
/// compression off stored its files as deltas.
fn route_policy(
    source: Option<&crate::bucket_policy::BucketPolicyConfig>,
    backend: &str,
    alias: &str,
) -> crate::bucket_policy::BucketPolicyConfig {
    crate::bucket_policy::BucketPolicyConfig {
        backend: Some(backend.to_string()),
        alias: Some(alias.to_string()),
        compression: source.and_then(|p| p.compression),
        max_delta_ratio: source.and_then(|p| p.max_delta_ratio),
        ..Default::default()
    }
}

/// Pure: does the existing route `p` already hold `want`?
fn same_route(
    p: &crate::bucket_policy::BucketPolicyConfig,
    want: &crate::bucket_policy::BucketPolicyConfig,
) -> bool {
    p.backend == want.backend
        && p.alias == want.alias
        && p.compression == want.compression
        && p.max_delta_ratio == want.max_delta_ratio
}

async fn remove_routes(mutator: &ConfigMutator, keys: &[String], context: &str) {
    let any_present = {
        let cfg = mutator.read().await;
        keys.iter().any(|k| cfg.buckets.contains_key(k))
    };
    if !any_present {
        return;
    }
    let keys = keys.to_vec();
    if let Err(e) = mutator
        .mutate_and_apply(context, move |cfg| {
            for k in &keys {
                cfg.buckets.remove(k);
            }
        })
        .await
    {
        tracing::warn!("migrate: failed to remove transient route(s): {e}");
    }
}

/// Count the objects in `bucket` (a route), up to [`DEST_COUNT_MAX_PAGES`]
/// pages. Returns `(count, more)`.
async fn count_objects(engine: &DynEngine, bucket: &str) -> Result<(u64, bool), String> {
    let mut count = 0u64;
    let mut token: Option<String> = None;
    for _ in 0..DEST_COUNT_MAX_PAGES {
        let page = list_page_retrying(engine, bucket, token.as_deref(), "list destination failed")
            .await
            .map_err(|e| e.message)?;
        count += page
            .objects
            .iter()
            .filter(|(k, _)| !k.ends_with('/'))
            .count() as u64;
        match page.next_continuation_token {
            Some(t) if page.is_truncated => token = Some(t),
            _ => return Ok((count, false)),
        }
    }
    Ok((count, true))
}

/// Objects (references excluded) that `real_bucket` holds on backend
/// `backend`, read straight from the backend (the job's staging route does
/// not exist yet), up to `limit`. `(count, more)`; a missing bucket is
/// `(0, false)`. The start request runs it so that a non-empty destination
/// is refused at once, with a 4xx the migrate dialog shows; the stage phase
/// checks again through the route.
pub(crate) async fn destination_count_on_backend(
    cfg: &crate::config::Config,
    backend: &str,
    real_bucket: &str,
    limit: usize,
) -> Result<(u64, bool), String> {
    let def = cfg
        .backend_by_name(backend)
        .ok_or_else(|| format!("unknown backend '{backend}'"))?;
    // Backend-level: the raw client of the target backend, not a
    // bucket's encryption (allowed by `bucket_encryption_rule_has_one_home`).
    let enc = cfg
        .backend_encryption_by_name(backend)
        .cloned()
        .unwrap_or_default();
    use crate::storage::StorageBackend;
    let storage = crate::deltaglider::build_raw_backend(backend, def, &enc)
        .await
        .map_err(|e| e.to_string())?;
    if !storage
        .head_bucket(real_bucket)
        .await
        .map_err(|e| e.to_string())?
    {
        return Ok((0, false));
    }
    let listing = storage
        .bulk_list_objects_with_baselines(real_bucket, "", None, Some(limit))
        .await
        .map_err(|e| e.to_string())?;
    let count = listing
        .objects
        .iter()
        .filter(|(k, _)| !k.ends_with('/'))
        .count() as u64;
    Ok((count, listing.next_start_after.is_some()))
}

/// Pure: refuse a migrate to a backend that the health loop gates (its
/// credentials are refused, or it does not answer): every copy would fail.
pub fn target_health_refusal(
    target_backend: &str,
    verdict: Option<&crate::coordination::health::HealthVerdict>,
) -> Option<String> {
    let v = verdict.filter(|v| v.is_gating())?;
    Some(format!(
        "backend '{target_backend}' is unavailable: {}. Fix the backend (Storage → Backends → \
         Test connection) before you move a bucket to it",
        v.cause()
    ))
}

/// Pre-flip unwind: delete the destination objects THIS job copied (their
/// `dg-migration` value carries the job id), and nothing else — objects
/// this job did not write stay. Each page goes to the engine's batch as
/// conditional items: the engine reads each object under its folder lock
/// and deletes it only while it carries this job's value. Best effort: a
/// failure is recorded on the job and the unwind goes on.
async fn remove_staged_copies(
    mutator: &ConfigMutator,
    db: &Arc<Mutex<JobsDb>>,
    state: &Arc<AppState>,
    job: &MaintenanceJob,
    params: &MigrateParams,
) {
    let fail = |e: String| async move {
        tracing::warn!("migrate: job #{} staged-copy cleanup: {e}", job.id);
        // Best effort: during a shutdown the record is refused, and the
        // unwind does not run then anyway (see execute_migrate_phases).
        let _ = record_failure(db, job.id, "", &format!("staged-copy cleanup: {e}")).await;
    };
    // A config apply can wipe the staging route; without it the deletes
    // would hit the default backend.
    if let Err(e) = ensure_route(
        mutator,
        &params.transient_key,
        &params.target_backend,
        &job.bucket,
        "Migration staging route re-asserted for cleanup",
    )
    .await
    {
        fail(format!("staging route unavailable: {e}")).await;
        return;
    }
    let ours = provenance_value(params, job.id);
    let ours =
        |meta: &crate::types::FileMetadata| meta.user_metadata.get("dg-migration") == Some(&ours);
    let mut pager = Pager::resuming(None);
    let mut removed = 0u64;
    while pager.begin_page().is_some() {
        let engine = state.engine.load().clone();
        let page = match list_page_retrying(
            &engine,
            &params.transient_key,
            pager.token(),
            "list destination failed",
        )
        .await
        {
            Ok(p) => p,
            Err(e) => return fail(e.message).await,
        };
        // Tokens are key-based, so deleting listed keys never skips one.
        let keys: Vec<&str> = page
            .objects
            .iter()
            .map(|(k, _)| k.as_str())
            .filter(|k| !k.ends_with('/'))
            .collect();
        let items = keys.iter().map(|k| DeleteItem::only_if(*k, ours)).collect();
        let outcomes = delete_keys(&engine, &params.transient_key, items, &|| {
            !crate::shutdown::is_shutting_down()
        })
        .await;
        for (key, outcome) in keys.iter().zip(outcomes) {
            match outcome {
                DeleteOutcome::Deleted(_) => removed += 1,
                DeleteOutcome::Failed(e) => {
                    fail(format!("delete of staged copy '{key}' failed: {e}")).await
                }
                DeleteOutcome::NotFound | DeleteOutcome::Changed | DeleteOutcome::Skipped => {}
            }
        }
        if !pager.advance(page.is_truncated, page.next_continuation_token) {
            break;
        }
    }
    info!(
        "migrate: job #{} removed {removed} staged copy(ies) from the destination",
        job.id
    );
}

/// Run one migrate job to completion (or error). The caller settles the
/// job row (which releases the gate); THIS function unwinds the transient
/// route on any PRE-FLIP termination (cancel or failure).
pub(crate) async fn execute_migrate_phases(
    mutator: &ConfigMutator,
    db: &Arc<Mutex<JobsDb>>,
    state: &Arc<AppState>,
    holder: Holder<'_>,
    job: &MaintenanceJob,
) -> Result<(), PhaseStop> {
    let params = parse_params(job.params.as_deref().ok_or("migrate job has no params")?)?;
    let result = run_phases(mutator, db, state, holder, job, &params).await;

    // Only an outcome the worker SETTLES unwinds. Lease loss is NOT an
    // unwind: the job continues under the next claimer, which needs the
    // staging route (and re-asserts it per page anyway). A shutdown is not
    // one either: the next boot resumes the copy, so deleting the staged
    // copies would throw that work away and fail a healthy migration.
    // A transient failure (a backend that timed out or answered 5xx after
    // the in-place retries) says nothing about the copies: they stay on the
    // target, and so does the job's cursor. While it has retries left the
    // worker queues the job again (route and gate stay); after the last it
    // settles `failed` and only the route goes. Only a cancel or a
    // definitive failure deletes the copies.
    let transient = matches!(result, Err(PhaseStop::FailedTransient(_)));
    let retry = transient && super::worker::transient_retry_left(&current_row(db, job).await);
    let settles =
        after_run(&result, crate::shutdown::is_shutting_down(), retry) == AfterRun::Settle;
    if result.is_err() && settles {
        // Determine the phase we died in (re-read — phases persist it).
        let db_guard = db.lock().await;
        let phase = db_guard
            .maintenance_job_by_id(job.id)
            .ok()
            .flatten()
            .map(|j| j.phase)
            .unwrap_or_else(|| job.phase.clone());
        let unwinds = unwinds_on_failure(&phase);
        if unwinds {
            // Source stays authoritative: clients may write to it again
            // now, not after the staged-copy cleanup below.
            state.maintenance_gate.release_for_unwind(job.id, &db_guard);
        }
        drop(db_guard);
        if unwinds {
            // Stage copies nothing, so there is nothing to remove.
            if phase != "stage" && !transient {
                remove_staged_copies(mutator, db, state, job, &params).await;
            }
            // Remove the staging route.
            remove_routes(
                mutator,
                std::slice::from_ref(&params.transient_key),
                "Migration aborted — staging route removed",
            )
            .await;
        }
        if unwinds && transient && phase != "stage" {
            if let Err(PhaseStop::FailedTransient(e)) = result {
                return Err(PhaseStop::FailedTransient(format!(
                    "{e} — the copies made so far stay on backend '{}'. Start the move \
                     again with \"Make the destination an exact mirror\" (API: \"target\": \
                     \"mirror\") to reuse them (it copies only what differs), or empty the \
                     destination bucket",
                    params.target_backend
                )));
            }
        }
    }
    result
}

async fn run_phases(
    mutator: &ConfigMutator,
    db: &Arc<Mutex<JobsDb>>,
    state: &Arc<AppState>,
    holder: Holder<'_>,
    job: &MaintenanceJob,
    params: &MigrateParams,
) -> Result<(), PhaseStop> {
    let m = MigrateRun {
        mutator,
        db,
        state,
        holder,
        job,
        params,
        provenance: provenance_value(params, job.id),
    };
    // The total is the source's object count: counted next to the copy,
    // exact once the copy ended (every object copied or skipped).
    let mut c = Counters::of(job);
    let mut phase = job.phase.as_str();
    if phase == "stage" {
        m.stage(&c).await?;
        phase = "copy";
    }
    if phase == "copy" {
        m.copy(&mut c).await?;
        phase = "verify";
    }
    if phase == "verify" {
        m.verify(&c).await?;
        phase = "flip";
    }
    if phase == "flip" {
        m.flip(&c).await?;
        phase = "cleanup";
    }
    if phase == "cleanup" && params.delete_source {
        m.cleanup().await?;
    }
    Ok(())
}

/// One migrate job's run: what every phase needs.
struct MigrateRun<'a> {
    mutator: &'a ConfigMutator,
    db: &'a Arc<Mutex<JobsDb>>,
    state: &'a Arc<AppState>,
    holder: Holder<'a>,
    job: &'a MaintenanceJob,
    params: &'a MigrateParams,
    /// The `dg-migration` value this job stamps on its copies.
    provenance: String,
}

impl MigrateRun<'_> {
    fn ctx(&self) -> JobCtx<'_> {
        JobCtx {
            db: self.db,
            holder: self.holder,
            job: self.job,
        }
    }

    /// Flush the deferred copies, then save `phase` with `token`.
    async fn checkpoint(
        &self,
        phase: &str,
        c: &Counters,
        token: Option<&str>,
    ) -> Result<(), PhaseStop> {
        save_after_flush(flush_copies(self.state), |flushed| {
            persist_flushed(flushed, self.db, self.job, phase, c, token)
        })
        .await
    }

    /// Re-assert the staging route: an admin config apply mid-job replaces
    /// cfg.buckets wholesale; without the route the copies would silently
    /// land on the DEFAULT backend.
    async fn ensure_staging_route(&self, context: &str) -> Result<(), String> {
        ensure_route(
            self.mutator,
            &self.params.transient_key,
            &self.params.target_backend,
            &self.job.bucket,
            context,
        )
        .await
    }

    /// ── Phase: stage ──
    async fn stage(&self, c: &Counters) -> Result<(), PhaseStop> {
        let (bucket, params) = (&self.job.bucket, self.params);
        check_cancel(self.db, self.job.id).await?;
        self.ensure_staging_route(&format!(
            "Migration staging route '{}' → '{}'",
            params.transient_key, params.target_backend
        ))
        .await?;
        // Real bucket on the target (idempotent). "Already exists" must be
        // tolerated for crash-resume, but backend error strings don't
        // reliably contain "exist" (the AWS SDK renders a 409 as a terse
        // "service error") — so on ANY create failure, probe the bucket
        // through the staging route instead of string-matching.
        let engine = self.state.engine.load().clone();
        if let Err(e) = engine.create_bucket(&params.transient_key).await {
            if engine
                .list_objects(&params.transient_key, "", None, 1, None, false)
                .await
                .is_err()
            {
                return Err(PhaseStop::from(format!(
                    "create bucket on target failed: {e}"
                )));
            }
        }
        // Stage runs before any copy, so every object seen here predates
        // this job.
        let (existing, more) = count_objects(&engine, &params.transient_key).await?;
        let dest = real_bucket_name(&self.mutator.read().await.buckets, bucket).to_string();
        destination_check(params.target, existing, more, &dest, &params.target_backend)?;
        // The gate has been rejecting NEW source writes since job creation;
        // wait out any write admitted before it armed.
        drain_inflight_writes(self.state, bucket).await?;
        self.checkpoint("copy", c, None).await
    }

    /// ── Phase: copy (resumable; ANY copy failure aborts pre-flip) ──
    async fn copy(&self, c: &mut Counters) -> Result<(), PhaseStop> {
        let count = c.total.is_none().then(|| {
            BackgroundCount::spawn(self.state.engine.load().clone(), self.job.bucket.clone())
        });
        let mut step = CopyStep {
            m: self,
            c,
            count,
            recopy: RecopyScope::on_resume(self.job.phase == "copy"),
            engine: None,
            forms: Default::default(),
            page_token: None,
        };
        paged_phase(
            self.ctx(),
            PhaseSpec {
                phase: "copy",
                resume: resumable_in(self.job, "copy"),
                cancel_every: Some(CANCEL_CHECK_EVERY),
                // Falling through to verify here would "verify" (and later
                // flip + delete) over a silently truncated listing —
                // never-copied tail objects would be lost. Fail instead; the
                // persisted cursor resumes the tail on retry.
                budget_exhausted: "copy stopped at the page budget with more source pages \
                     pending — bucket too large for one pass; job left resumable \
                     in phase 'copy' (cursor persisted, source authoritative)",
                concurrency: PAGE_CONCURRENCY,
            },
            &mut step,
        )
        .await?;
        // A failed copy stops the phase, so every object is copied or skipped.
        c.total = Some(c.done + c.skipped);
        self.checkpoint("verify", c, None).await
    }

    /// ── Phase: verify ──
    async fn verify(&self, c: &Counters) -> Result<(), PhaseStop> {
        let mut step = VerifyStep {
            m: self,
            c,
            engine: None,
            forms: Default::default(),
            verified: 0,
        };
        let verified = paged_phase(
            self.ctx(),
            PhaseSpec {
                phase: "verify",
                resume: resumable_in(self.job, "verify"),
                cancel_every: Some(CANCEL_CHECK_EVERY),
                budget_exhausted: "verify stopped at the page budget with more source pages \
                     pending — refusing to flip over an incompletely verified \
                     listing; job left resumable in phase 'verify'",
                concurrency: PAGE_CONCURRENCY,
            },
            &mut step,
        )
        .await;
        super::live::clear(self.job);
        verified?;
        if self.params.target == MigrateTarget::Mirror {
            self.prune_destination_extras().await?;
        }
        self.checkpoint("flip", c, None).await
    }

    /// `target: mirror`: delete every destination object that the source
    /// does not hold, before the flip (the gate still freezes the source).
    /// Each delete is audited. Any source HEAD error other than not-found
    /// stops the job: an object is deleted only when the source provably
    /// lacks it.
    async fn prune_destination_extras(&self) -> Result<(), PhaseStop> {
        let mut step = PruneStep {
            m: self,
            engine: None,
            pruned: 0,
            absent: Vec::new(),
        };
        paged_phase(
            self.ctx(),
            PhaseSpec {
                phase: "mirror",
                resume: None,
                cancel_every: Some(CANCEL_CHECK_EVERY),
                budget_exhausted: "mirror: destination listing stopped at the page budget — \
                     refusing to flip over an incompletely mirrored destination",
                concurrency: PAGE_CONCURRENCY,
            },
            &mut step,
        )
        .await?;
        info!(
            "migrate: job #{} mirror deleted {} destination object(s) absent at the source",
            self.job.id, step.pruned
        );
        Ok(())
    }

    /// ── Phase: flip (idempotent; NOT interruptible) ──
    async fn flip(&self, c: &Counters) -> Result<(), PhaseStop> {
        let (bucket, params) = (&self.job.bucket, self.params);
        let bucket_key = bucket.clone();
        let target = params.target_backend.clone();
        let transient = params.transient_key.clone();
        self.mutator
            // STRICT: the flip's file-persist must succeed or the whole flip
            // rolls back (engine swapped back to source). A file that lags
            // the flip is the data-loss crash window: boot would route
            // clients to the source while the resumed cleanup deletes it.
            .mutate_and_apply_strict(
                &format!("Bucket '{bucket_key}' migrated to backend '{target}'"),
                move |cfg| {
                    // Pin the alias to the name the copies used (the real name
                    // BEFORE the flip): setting `backend` activates `alias`.
                    let real = real_bucket_name(&cfg.buckets, &bucket_key).to_string();
                    let mut policy = cfg.buckets.get(&bucket_key).cloned().unwrap_or_default();
                    policy.alias = (real != bucket_key).then_some(real);
                    policy.backend = Some(target);
                    cfg.buckets.insert(bucket_key, policy);
                    cfg.buckets.remove(&transient);
                },
            )
            .await?;
        info!(
            "migrate: bucket '{}' flipped to backend '{}'",
            bucket, params.target_backend
        );
        // The usage counter still holds the source's layout (deltas are
        // encoded again against new references on the target): its row is
        // marked for a recount.
        if let Some(usage) = &self.state.bucket_usage {
            usage.mark_for_recount(bucket);
        }
        self.checkpoint("cleanup", c, None).await?;
        // Destination is authoritative — client writes resume NOW, not at
        // job settle: the row is in `cleanup`, which gates neither the
        // bucket nor the (just removed) staging route.
        self.state
            .maintenance_gate
            .sync_from(&*self.db.lock().await);
        Ok(())
    }

    /// ── Phase: cleanup (optional delete-source; never fails the job) ──
    ///
    /// Not a [`paged_phase`]: it has no cursor (deletes shift the key
    /// tokens, so a sweep that deleted restarts from the top), and a cancel
    /// here is a note on a finished migration, not a stop.
    async fn cleanup(&self) -> Result<(), PhaseStop> {
        let (mutator, db, state, holder, job, params) = (
            self.mutator,
            self.db,
            self.state,
            self.holder,
            self.job,
            self.params,
        );
        let bucket = &job.bucket;
        let cleanup_key = format!("{}__src", params.transient_key);
        let mut delete_failures = 0u32;
        let mut cancelled_mid_cleanup = false;
        // A stop that is not a delete failure (the route cannot be staged,
        // or the live config no longer routes the bucket to the target).
        // The flip already happened, so it becomes a note, never a failed
        // job: a failed row invites a retry that 400s ("already on backend").
        let mut stopped: Option<String> = None;
        if let Err(e) = ensure_route(
            mutator,
            &cleanup_key,
            &params.from_backend,
            bucket,
            "Migration source-cleanup route staged",
        )
        .await
        {
            stopped = Some(format!("the source-cleanup route could not be staged: {e}"));
        }
        let mut cleanup_token: Option<String> = None;
        'cleanup: for _ in 0..MAX_JOB_PAGES {
            if stopped.is_some() {
                break 'cleanup;
            }
            if check_cancel(db, job.id).await.is_err() {
                // Flip already happened; stop deleting and settle with a
                // note below (NOT the generic cancel path — the MIGRATION
                // itself succeeded).
                cancelled_mid_cleanup = true;
                break 'cleanup;
            }
            // Re-checked EVERY sweep (not just once): deleting source
            // objects is only safe while the LIVE config genuinely routes
            // the bucket to the target. A resumed job whose flip didn't
            // stick — or an admin apply mid-cleanup that re-routes the
            // bucket back — must stop the sweep instantly instead of
            // deleting data clients are actively writing to.
            let routed_to_target = {
                let cfg = mutator.read().await;
                cfg.buckets
                    .get(bucket)
                    .and_then(|p| p.backend.as_deref().map(|b| b == params.target_backend))
                    .unwrap_or(false)
            };
            if !routed_to_target {
                stopped = Some(format!(
                    "cleanup refused: bucket '{}' is not routed to '{}' in the live \
                     config — source data left untouched",
                    bucket, params.target_backend
                ));
                break 'cleanup;
            }
            // The source backend is gated: wait (the job resumes in
            // cleanup) instead of sending deletes it cannot answer.
            holder.check_backend()?;
            let engine = state.engine.load().clone();
            let page = match list_page_retrying(
                &engine,
                &cleanup_key,
                cleanup_token.as_deref(),
                "cleanup list failed",
            )
            .await
            {
                Ok(p) => p,
                Err(e) => {
                    record_failure(db, job.id, "", &e.message).await?;
                    delete_failures += 1;
                    break 'cleanup;
                }
            };
            // The whole page in one batch: one reclaim check per folder,
            // several folders at once. A cancel or a shutdown stops it
            // between keys.
            let keys: Vec<&str> = page
                .objects
                .iter()
                .map(|(k, _)| k.as_str())
                .filter(|k| !k.ends_with('/'))
                .collect();
            let watch = CancelWatch::spawn(db, job.id);
            let outcomes = delete_keys(
                &engine,
                &cleanup_key,
                keys.iter().map(|k| DeleteItem::new(*k)).collect(),
                &|| !watch.requested() && !crate::shutdown::is_shutting_down(),
            )
            .await;
            drop(watch);
            let mut deleted_this_sweep = 0u32;
            for (key, outcome) in keys.iter().zip(outcomes) {
                match outcome {
                    DeleteOutcome::Deleted(_) | DeleteOutcome::NotFound => deleted_this_sweep += 1,
                    DeleteOutcome::Failed(e) => {
                        record_failure(db, job.id, key, &format!("source delete failed: {e}"))
                            .await?;
                        delete_failures += 1;
                    }
                    DeleteOutcome::Changed | DeleteOutcome::Skipped => {}
                }
            }
            if crate::shutdown::is_shutting_down() {
                return Err(PhaseStop::ShuttingDown);
            }
            if check_cancel(db, job.id).await.is_err() {
                cancelled_mid_cleanup = true;
                break 'cleanup;
            }
            // No more pages → done.
            if page.next_continuation_token.is_none() {
                break 'cleanup;
            }
            // Deletes shrink the listing, so after a sweep that DELETED something
            // we restart from the top (cleanup_token=None). But a sweep can delete
            // NOTHING while a next page exists — e.g. a page that is ALL '/'-suffixed
            // directory markers (filtered out of deletion) OR a page of only
            // failed deletes. Restarting from the top would re-list the same page
            // forever and settle 'completed' with source objects left behind (H38).
            // Instead PAGE FORWARD past the undeletable page to reach real objects;
            // once something deletes again, resume restart-from-top.
            if deleted_this_sweep == 0 {
                cleanup_token = page.next_continuation_token.clone();
            } else {
                cleanup_token = None;
            }
            heartbeat(db, job.id, holder).await?;
        }
        remove_routes(
            mutator,
            &[cleanup_key],
            "Migration source-cleanup route removed",
        )
        .await;
        // The MIGRATION succeeded either way; an interrupted cleanup is
        // surfaced as a note, never as a failed/cancelled job.
        if let Some((status, note)) =
            cleanup_settlement(cancelled_mid_cleanup, delete_failures, stopped.as_deref())
        {
            let db = db.lock().await;
            let _ = db.maintenance_finish(job.id, status, Some(&note));
            return Ok(());
        }
        Ok(())
    }
}

/// The copy phase's per-page and per-object work.
struct CopyStep<'a, 'r> {
    m: &'a MigrateRun<'r>,
    c: &'a mut Counters,
    /// The source's object count, made next to the copy.
    count: Option<BackgroundCount>,
    recopy: RecopyScope,
    engine: Option<Arc<DynEngine>>,
    /// The stored forms that the page's source listing proved.
    forms: crate::deltaglider::ListedPageForms,
    /// The cursor of the page in progress (where a failed page resumes).
    page_token: Option<String>,
}

impl CopyStep<'_, '_> {
    /// The source count, into the counters once it ended.
    fn take_count(&mut self) {
        if self.c.total.is_none() {
            self.c.total = self.count.as_ref().and_then(BackgroundCount::get);
        }
    }
}

/// What the copy of one object ended in.
enum CopyDone {
    /// The target already holds this version.
    Skipped,
    Copied {
        bytes: i64,
    },
    Failed {
        transient: bool,
        message: String,
    },
}

impl PageStep for CopyStep<'_, '_> {
    type Done = CopyDone;

    async fn begin_page(&mut self) -> Result<(), PhaseStop> {
        self.m
            .ensure_staging_route("Migration staging route re-asserted after config change")
            .await?;
        self.engine = Some(self.m.state.engine.load().clone());
        Ok(())
    }

    async fn list(&mut self, token: Option<&str>) -> Result<KeyPage, ListError> {
        self.page_token = token.map(str::to_string);
        let engine = self.engine.as_ref().expect("begin_page runs first");
        let (page, forms, _) =
            list_job_keys(engine, &self.m.job.bucket, token, "list source failed").await?;
        self.forms = forms;
        Ok(page)
    }

    /// Restart from page 0: the verdict skip makes the re-list idempotent.
    /// (Counters are NOT reset, so already-copied objects tally as
    /// `skipped` a second time — display drift only, never a re-copy.)
    fn on_restart(&mut self) {
        self.recopy = self.recopy.on_restart_fresh();
    }

    async fn save(&mut self, token: Option<&str>) -> Result<(), PhaseStop> {
        self.take_count();
        self.m.checkpoint("copy", self.c, token).await
    }

    /// Resume token = this page's: the verdict skip makes the redo of its
    /// first part idempotent.
    async fn mid_page(&mut self, page_token: Option<&str>) -> Result<(), PhaseStop> {
        self.take_count();
        self.m.checkpoint("copy", self.c, page_token).await
    }

    async fn object(&self, key: &str) -> CopyDone {
        let (m, engine) = (self.m, self.engine.as_ref().expect("begin_page runs first"));
        let (bucket, params) = (&m.job.bucket, m.params);
        // Skip only a target copy that PROVABLY matches the source: a
        // cancelled earlier attempt leaves copies that the source has
        // since outgrown. A target HEAD that keeps failing proves nothing:
        // the object is copied (the copy is idempotent).
        let check = if self.recopy.recopies() {
            None
        } else {
            copy_check(
                engine,
                bucket,
                &params.transient_key,
                key,
                self.forms.of(key),
            )
            .await
            .ok()
        };
        let (source, prior) = match check {
            Some(CopyCheck {
                verdict: ContentVerdict::Same,
                ..
            }) => return CopyDone::Skipped,
            Some(check) => (check.source, Some(check.target)),
            None => (None, None),
        };
        // The copy's source HEAD: the check's, else one of the listed form.
        // A failed read leaves it to the copy, which reports the error.
        let source = match source {
            Some(meta) => Some(meta),
            None => engine
                .head_listed(bucket, key, self.forms.of(key))
                .await
                .ok()
                .map(|(meta, _)| meta),
        };
        let req = ObjectTransferRequest {
            source_bucket: bucket,
            source_key: key,
            destination_bucket: &params.transient_key,
            destination_key: key,
            provenance: Some(TransferProvenance {
                metadata_key: "dg-migration",
                metadata_value: &m.provenance,
            }),
            strip_user_metadata_keys: &[],
            operation: "migrate",
            upload_concurrency: None,
            keep_created_at: true,
        };
        // No per-object fsync: `checkpoint` flushes the copies before any
        // progress that counts on them is saved.
        let copy = copy_object_known(engine, req, source.as_ref(), prior.as_ref());
        match crate::storage::with_deferred_fsync(copy).await {
            Ok(outcome) => CopyDone::Copied {
                bytes: outcome.bytes_copied as i64,
            },
            Err(e) => CopyDone::Failed {
                transient: e.class.retryable(),
                message: e.to_string(),
            },
        }
    }

    fn stops(done: &CopyDone) -> bool {
        matches!(done, CopyDone::Failed { .. })
    }

    async fn settle(&mut self, key: &str, done: CopyDone) -> Result<(), PhaseStop> {
        let m = self.m;
        match done {
            CopyDone::Skipped => self.c.skipped += 1,
            CopyDone::Copied { bytes } => {
                self.c.done += 1;
                self.c.bytes += bytes;
            }
            CopyDone::Failed { transient, message } => {
                record_failure(m.db, m.job.id, key, &message).await?;
                self.c.failed += 1;
                // The cursor stays at this page: the copies before it are
                // durable, and a resumed job redoes this page.
                m.checkpoint("copy", self.c, self.page_token.as_deref())
                    .await?;
                let message =
                    format!("copy of '{key}' failed — source remains authoritative: {message}");
                return Err(if transient {
                    PhaseStop::FailedTransient(message)
                } else {
                    PhaseStop::Failed(message)
                });
            }
        }
        Ok(())
    }

    async fn end_page(&mut self) -> Result<(), PhaseStop> {
        self.recopy = self.recopy.after_page();
        Ok(())
    }
}

/// The verify phase: HEAD every source key on the transient.
struct VerifyStep<'a, 'r> {
    m: &'a MigrateRun<'r>,
    c: &'a Counters,
    engine: Option<Arc<DynEngine>>,
    /// The stored forms that the page's source listing proved.
    forms: crate::deltaglider::ListedPageForms,
    /// Objects checked so far in this run of the phase.
    verified: i64,
}

impl PageStep for VerifyStep<'_, '_> {
    type Done = Result<(), PhaseStop>;

    async fn begin_page(&mut self) -> Result<(), PhaseStop> {
        self.engine = Some(self.m.state.engine.load().clone());
        Ok(())
    }

    async fn list(&mut self, token: Option<&str>) -> Result<KeyPage, ListError> {
        let engine = self.engine.as_ref().expect("begin_page runs first");
        let (page, forms, _) =
            list_job_keys(engine, &self.m.job.bucket, token, "verify list failed").await?;
        self.forms = forms;
        Ok(page)
    }

    async fn save(&mut self, token: Option<&str>) -> Result<(), PhaseStop> {
        self.m.checkpoint("verify", self.c, token).await
    }

    async fn object(&self, key: &str) -> Result<(), PhaseStop> {
        let engine = self.engine.as_ref().expect("begin_page runs first");
        match copy_verdict_for(
            engine,
            &self.m.job.bucket,
            &self.m.params.transient_key,
            key,
            self.forms.of(key),
        )
        .await
        {
            Ok(ContentVerdict::Missing) => Err(PhaseStop::from(format!(
                "verification failed: '{key}' missing on target"
            ))),
            Ok(ContentVerdict::Differs) => Err(PhaseStop::from(format!(
                "verification failed: '{key}' on target differs from the source"
            ))),
            // Unknown = no common fingerprint (foreign object); the copy
            // phase re-copied it, so it is current.
            Ok(ContentVerdict::Same | ContentVerdict::Unknown) => Ok(()),
            Err(e) => Err(e.stop()),
        }
    }

    fn stops(done: &Result<(), PhaseStop>) -> bool {
        done.is_err()
    }

    async fn settle(&mut self, _key: &str, done: Result<(), PhaseStop>) -> Result<(), PhaseStop> {
        done?;
        self.verified += 1;
        super::live::set_verified(self.m.job, self.verified);
        Ok(())
    }
}

/// The mirror prune: list the destination, delete what the source lacks.
struct PruneStep<'a, 'r> {
    m: &'a MigrateRun<'r>,
    engine: Option<Arc<DynEngine>>,
    pruned: u64,
    /// The page's destination keys that the source lacks.
    absent: Vec<String>,
}

impl PageStep for PruneStep<'_, '_> {
    /// `Ok(true)`: the source lacks the key.
    type Done = Result<bool, PhaseStop>;

    async fn begin_page(&mut self) -> Result<(), PhaseStop> {
        self.engine = Some(self.m.state.engine.load().clone());
        Ok(())
    }

    async fn list(&mut self, token: Option<&str>) -> Result<KeyPage, ListError> {
        let engine = self.engine.as_ref().expect("begin_page runs first");
        engine
            .list_objects(
                &self.m.params.transient_key,
                "",
                None,
                PAGE_SIZE,
                token,
                false,
            )
            .await
            .map(KeyPage::from)
            .map_err(|e| ListError::engine("mirror: list destination failed", &e))
    }

    /// No cursor: a resumed job re-runs the prune from the top.
    async fn save(&mut self, _token: Option<&str>) -> Result<(), PhaseStop> {
        Ok(())
    }

    async fn object(&self, key: &str) -> Result<bool, PhaseStop> {
        let engine = self.engine.as_ref().expect("begin_page runs first");
        match head_retrying(engine, &self.m.job.bucket, key).await {
            Ok(found) => Ok(found.is_none()),
            Err(e) => Err(HeadFailed {
                message: format!(
                    "mirror: {} — nothing deleted for it; source remains authoritative",
                    e.message
                ),
                ..e
            }
            .stop()),
        }
    }

    fn stops(done: &Result<bool, PhaseStop>) -> bool {
        done.is_err()
    }

    async fn settle(&mut self, key: &str, done: Result<bool, PhaseStop>) -> Result<(), PhaseStop> {
        if done? {
            self.absent.push(key.to_string());
        }
        Ok(())
    }

    /// The page's extras in one batch (one reclaim check per folder), each
    /// deleted one audited.
    async fn end_page(&mut self) -> Result<(), PhaseStop> {
        let m = self.m;
        let engine = self.engine.as_ref().expect("begin_page runs first");
        let keys = std::mem::take(&mut self.absent);
        let items = keys.iter().map(|k| DeleteItem::new(k.as_str())).collect();
        let outcomes = delete_keys(engine, &m.params.transient_key, items, &|| {
            !crate::shutdown::is_shutting_down()
        })
        .await;
        for (key, outcome) in keys.iter().zip(outcomes) {
            match outcome {
                DeleteOutcome::Deleted(_) => {
                    self.pruned += 1;
                    crate::audit::audit_log(
                        "maintenance_migrate_mirror_delete",
                        m.job.triggered_by.as_deref().unwrap_or("system"),
                        &format!("job:{}", m.job.id),
                        &axum::http::HeaderMap::new(),
                        &m.job.bucket,
                        key,
                    );
                }
                DeleteOutcome::NotFound | DeleteOutcome::Changed => {}
                DeleteOutcome::Skipped => return Err(PhaseStop::ShuttingDown),
                DeleteOutcome::Failed(e) => {
                    let message =
                        format!("mirror: delete of destination extra '{key}' failed: {e}");
                    return Err(if ListFailure::of(&e) == ListFailure::Transient {
                        PhaseStop::FailedTransient(message)
                    } else {
                        PhaseStop::Failed(message)
                    });
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "migrate_cost_tests.rs"]
mod cost_tests;

#[cfg(test)]
mod tests {
    /// jobs-5: a stopped cleanup (route staging failed, or the bucket no
    /// longer routes to the target) settles `completed_with_errors`, never
    /// `failed`: the flip already happened.
    #[test]
    fn cleanup_settlement_never_fails_the_migration() {
        use super::cleanup_settlement as settle;
        assert_eq!(settle(false, 0, None), None);
        let (s, n) = settle(false, 0, Some("route refused")).unwrap();
        assert_eq!(s, "completed_with_errors");
        assert!(n.contains("route refused"), "{n}");
        assert_eq!(settle(true, 0, None).unwrap().0, "completed");
        assert_eq!(settle(true, 2, None).unwrap().0, "completed_with_errors");
        let (s, n) = settle(false, 3, None).unwrap();
        assert_eq!(s, "completed_with_errors");
        assert!(n.contains("3 failure(s)"), "{n}");
    }

    #[test]
    fn multi_instance_refusal_only_with_a_coordination_bucket() {
        use super::multi_instance_refusal as refuse;
        assert!(refuse(None).is_none());
        assert!(refuse(Some("")).is_none());
        let msg = refuse(Some("dgp-sync")).expect("refused");
        assert!(msg.contains("dgp-sync") && msg.contains(super::MIGRATE_DOC_URL));
    }

    use super::RecopyScope;

    /// A resumed copy re-copies the page that can hold torn copies: the first
    /// page after a resume, or every page after a restart from page 0.
    #[test]
    fn recopy_scope_covers_the_page_at_the_saved_token() {
        let fresh = RecopyScope::on_resume(false);
        assert!(!fresh.recopies());
        assert!(!fresh.on_restart_fresh().recopies());

        let resumed = RecopyScope::on_resume(true);
        assert!(resumed.recopies());
        assert!(!resumed.after_page().recopies());

        let restarted = resumed.on_restart_fresh();
        assert!(restarted.recopies());
        assert!(restarted.after_page().recopies());
        assert!(restarted.after_page().after_page().recopies());
    }

    /// The checkpoint saves only after the flush, and never after a failed
    /// flush: a saved cursor must not point past a copy that is not durable.
    /// (That every migrate save is such a checkpoint is the type's job: the
    /// only save this file can reach, `persist_flushed`, takes a `Flushed`.)
    #[tokio::test]
    async fn checkpoint_saves_only_after_a_successful_flush() {
        use super::{save_after_flush, Flushed};
        let log = std::sync::Mutex::new(Vec::new());
        let flush = |ok: bool| {
            let log = &log;
            async move {
                log.lock().unwrap().push("flush");
                if ok {
                    Ok(Flushed(()))
                } else {
                    Err(crate::deltaglider::EngineError::Overloaded("disk".into()))
                }
            }
        };
        let save = |_: Flushed| async {
            log.lock().unwrap().push("save");
        };
        save_after_flush(flush(true), save).await.unwrap();
        assert_eq!(*log.lock().unwrap(), ["flush", "save"]);
        log.lock().unwrap().clear();
        assert!(save_after_flush(flush(false), save).await.is_err());
        assert_eq!(
            *log.lock().unwrap(),
            ["flush"],
            "no save after a failed flush"
        );
    }

    use super::*;

    #[test]
    fn real_bucket_name_follows_the_routing_rule() {
        use crate::bucket_policy::BucketPolicyConfig;
        let mut m = std::collections::BTreeMap::new();
        m.insert(
            "routed".to_string(),
            BucketPolicyConfig {
                backend: Some("src".into()),
                alias: Some("real".into()),
                ..Default::default()
            },
        );
        // Alias without a backend is inert in the routing table.
        m.insert(
            "inert".to_string(),
            BucketPolicyConfig {
                alias: Some("ignored".into()),
                ..Default::default()
            },
        );
        assert_eq!(real_bucket_name(&m, "routed"), "real");
        assert_eq!(real_bucket_name(&m, "inert"), "inert");
        assert_eq!(real_bucket_name(&m, "absent"), "absent");
    }

    #[test]
    fn a_route_stores_objects_as_its_source_bucket_does() {
        let source = crate::bucket_policy::BucketPolicyConfig {
            compression: Some(false),
            max_delta_ratio: Some(0.5),
            public: Some(true),
            ..Default::default()
        };
        let route = route_policy(Some(&source), "dst", "real");
        assert_eq!(route.backend.as_deref(), Some("dst"));
        assert_eq!(route.alias.as_deref(), Some("real"));
        assert_eq!(route.compression, Some(false));
        assert_eq!(route.max_delta_ratio, Some(0.5));
        assert_eq!(route.public, None, "a route grants no access");
        assert!(same_route(
            &route,
            &route_policy(Some(&source), "dst", "real")
        ));
        let old = route_policy(None, "dst", "real");
        assert!(!same_route(&old, &route), "an older route is rewritten");
    }

    #[test]
    fn params_round_trip() {
        let p = MigrateParams {
            target_backend: "hz".into(),
            delete_source: true,
            transient_key: "__dgmigrate_b_0".into(),
            from_backend: "local".into(),
            target: MigrateTarget::Mirror,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(parse_params(&json).unwrap(), p);
        // Rows written before the option existed read as `Empty`.
        let old = r#"{"target_backend":"hz","delete_source":false,"transient_key":"t","from_backend":"l"}"#;
        assert_eq!(parse_params(old).unwrap().target, MigrateTarget::Empty);
        assert!(parse_params("nope").is_err());
        assert!(parse_params("{}").is_err(), "missing fields rejected");
    }

    #[test]
    fn destination_check_refuses_a_non_empty_destination_by_default() {
        use MigrateTarget::*;
        assert!(destination_check(Empty, 0, false, "b", "hz").is_ok());
        assert!(destination_check(Mirror, 0, false, "b", "hz").is_ok());
        assert!(destination_check(Mirror, 5, true, "b", "hz").is_ok());
        let e = destination_check(Empty, 3, false, "b", "hz").unwrap_err();
        assert!(e.contains("already holds 3 object(s)"), "{e}");
        assert!(e.contains("'b' on backend 'hz'"), "{e}");
        assert!(
            e.contains("\"Make the destination an exact mirror\""),
            "names the option in the words of the migrate dialog: {e}"
        );
        let e = destination_check(Empty, 10_000, true, "b", "hz").unwrap_err();
        assert!(e.contains("at least 10000"), "{e}");
    }

    /// Maint B1: the migrate dialog offered a backend in failure cooldown.
    #[test]
    fn a_gated_target_backend_is_refused() {
        use crate::coordination::health::HealthVerdict;
        assert!(target_health_refusal("hz", None).is_none(), "never probed");
        assert!(target_health_refusal("hz", Some(&HealthVerdict::Healthy)).is_none());
        let erroring = HealthVerdict::Erroring {
            detail: "500".into(),
        };
        assert!(target_health_refusal("hz", Some(&erroring)).is_none());
        let auth = HealthVerdict::AuthRejected {
            detail: "SignatureDoesNotMatch".into(),
        };
        let e = target_health_refusal("hz", Some(&auth)).unwrap();
        assert!(e.contains("backend 'hz' is unavailable"), "{e}");
        assert!(e.contains("credentials rejected"), "{e}");
    }

    #[test]
    fn provenance_is_job_unique() {
        let p = MigrateParams {
            target_backend: "hz".into(),
            delete_source: false,
            transient_key: "t".into(),
            from_backend: "local".into(),
            target: MigrateTarget::Empty,
        };
        assert_eq!(provenance_value(&p, 7), "local->hz#job7");
        assert_ne!(provenance_value(&p, 7), provenance_value(&p, 8));
    }

    #[test]
    fn phase_classification() {
        assert!(is_pre_flip("stage"));
        assert!(is_pre_flip("copy"));
        assert!(is_pre_flip("verify"));
        assert!(!is_pre_flip("flip"));
        assert!(!is_pre_flip("cleanup"));
        assert!(unwinds_on_failure("stage"));
        assert!(unwinds_on_failure("copy"));
        assert!(unwinds_on_failure("verify"));
        assert!(
            unwinds_on_failure("flip"),
            "atomic flip failure = source authoritative"
        );
        assert!(!unwinds_on_failure("cleanup"));
        // Gate-arming is distinct from is_pre_flip: the gate stays armed
        // through `flip` (a resume there may still route to source), and is
        // cleared ONLY for `cleanup`. Conflating this with is_pre_flip was the
        // pre-flip-gate-clear CRITICAL.
        for p in ["stage", "copy", "verify", "flip"] {
            assert!(gate_armed_during(p), "{p} must stay gated");
        }
        assert!(!gate_armed_during("cleanup"), "cleanup is post-flip, live");
        assert_eq!(PHASES.len(), 5);
    }

    #[test]
    fn transient_key_walks_past_collisions() {
        let taken = |k: &str| k == "__dgmigrate_b_0" || k == "__dgmigrate_b_1";
        assert_eq!(pick_transient_key("b", &taken), "__dgmigrate_b_2");
        let none = |_: &str| false;
        assert_eq!(pick_transient_key("b", &none), "__dgmigrate_b_0");
    }

    #[test]
    fn orphan_detection() {
        let keys = [
            "pippo",
            "__dgmigrate_a_0",
            "__dgmigrate_b_0",
            "__dgmigrate_b_0__src",
        ];
        let active: HashSet<String> = ["__dgmigrate_b_0".to_string()].into();
        let mut orphans = orphaned_transients(keys.iter().copied(), &active);
        orphans.sort();
        // The active job's transient survives; its __src twin and the
        // unreferenced one are orphans. Plain buckets are never touched.
        assert_eq!(orphans, vec!["__dgmigrate_a_0", "__dgmigrate_b_0__src"]);
    }
}
