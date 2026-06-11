// SPDX-License-Identifier: GPL-3.0-only

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
//!    route. Delete failures are recorded but do not fail the migration
//!    (the flip already happened).
//!
//! Cancellation: pre-flip → unwind the transient route and settle
//! `cancelled` (source untouched, still authoritative). The flip itself
//! is not interruptible (cancel is checked between pages/phases); during
//! cleanup a cancel stops deleting and settles with a note.
//!
//! Multi-instance caveat (bigger blast radius than reencrypt, restated
//! deliberately): the route flip mutates THIS instance's config file +
//! engine; peers converge only via config sync, and their write gates
//! never arm. Single-runner posture, same as the rest of `maintenance`.

use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::info;

use crate::api::handlers::AppState;
use crate::config_apply::ConfigMutator;
use crate::config_db::ConfigDb;
use crate::job_loop::{Pager, MAX_JOB_PAGES};
use crate::transfer::{copy_object_with_retries, ObjectTransferRequest, TransferProvenance};

use super::store::MaintenanceJob;
use super::worker::{
    check_cancel, drain_inflight_writes, heartbeat, persist, record_failure, CANCELLED,
};

pub const TRANSIENT_PREFIX: &str = "__dgmigrate_";
const PAGE_SIZE: u32 = 1000;

/// Kind-specific parameters carried in `maintenance_jobs.params` (JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrateParams {
    pub target_backend: String,
    pub delete_source: bool,
    pub transient_key: String,
    pub from_backend: String,
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

/// Ensure the transient (or cleanup) route exists in the live config —
/// idempotent; re-run per page because a config apply can wipe it.
async fn ensure_route(
    mutator: &ConfigMutator,
    route_key: &str,
    backend: &str,
    alias_bucket: &str,
    context: &str,
) -> Result<(), String> {
    let present = {
        let cfg = mutator.read().await;
        cfg.buckets
            .get(route_key)
            .is_some_and(|p| p.backend.as_deref() == Some(backend))
    };
    if present {
        return Ok(());
    }
    let route_key = route_key.to_string();
    let backend = backend.to_string();
    let alias = alias_bucket.to_string();
    mutator
        .mutate_and_apply(context, move |cfg| {
            cfg.buckets.insert(
                route_key,
                crate::bucket_policy::BucketPolicyConfig {
                    backend: Some(backend),
                    alias: Some(alias),
                    ..Default::default()
                },
            );
        })
        .await
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

/// Run one migrate job to completion (or error). The caller settles the
/// job row and clears the gate; THIS function unwinds the transient
/// route on any PRE-FLIP termination (cancel or failure).
pub async fn execute_migrate_phases(
    mutator: &ConfigMutator,
    db: &Arc<Mutex<ConfigDb>>,
    state: &Arc<AppState>,
    instance_id: &str,
    job: &MaintenanceJob,
) -> Result<(), String> {
    let params = parse_params(job.params.as_deref().ok_or("migrate job has no params")?)?;
    let result = run_phases(mutator, db, state, instance_id, job, &params).await;

    if result.is_err() {
        // Determine the phase we died in (re-read — phases persist it).
        let phase = {
            let db = db.lock().await;
            db.maintenance_job_by_id(job.id)
                .ok()
                .flatten()
                .map(|j| j.phase)
                .unwrap_or_else(|| job.phase.clone())
        };
        if unwinds_on_failure(&phase) {
            // Source stays authoritative; remove the staging route.
            remove_routes(
                mutator,
                std::slice::from_ref(&params.transient_key),
                "Migration aborted — staging route removed",
            )
            .await;
        }
    }
    result
}

async fn run_phases(
    mutator: &ConfigMutator,
    db: &Arc<Mutex<ConfigDb>>,
    state: &Arc<AppState>,
    instance_id: &str,
    job: &MaintenanceJob,
    params: &MigrateParams,
) -> Result<(), String> {
    let bucket = &job.bucket;
    let mut phase = job.phase.clone();
    let resume_token = job.continuation_token.clone();
    let mut done = job.objects_done;
    let mut skipped = job.objects_skipped;
    let mut failed = job.objects_failed;
    let mut bytes = job.bytes_done;
    let provenance_value = format!("{}->{}", params.from_backend, params.target_backend);

    // ── Phase: stage ──
    if phase == "stage" {
        check_cancel(db, job.id).await?;
        ensure_route(
            mutator,
            &params.transient_key,
            &params.target_backend,
            bucket,
            &format!(
                "Migration staging route '{}' → '{}'",
                params.transient_key, params.target_backend
            ),
        )
        .await?;
        // Real bucket on the target (idempotent — tolerate "exists").
        let engine = state.engine.load().clone();
        if let Err(e) = engine.create_bucket(&params.transient_key).await {
            let msg = e.to_string();
            if !msg.to_lowercase().contains("exist") {
                return Err(format!("create bucket on target failed: {msg}"));
            }
        }
        // The gate has been rejecting NEW source writes since job creation;
        // wait out any write admitted before it armed.
        drain_inflight_writes(state, bucket).await?;
        phase = "copy".to_string();
        persist(db, job, &phase, None, done, skipped, failed, bytes, None).await;
    }

    // ── Phase: copy (resumable; ANY copy failure aborts pre-flip) ──
    if phase == "copy" {
        // Resume only when the job was persisted IN this phase.
        let mut pager = Pager::resuming(if job.phase == "copy" {
            resume_token.clone()
        } else {
            None
        });
        while pager.begin_page().is_some() {
            check_cancel(db, job.id).await?;
            // Re-assert the staging route: an admin config apply mid-job
            // replaces cfg.buckets wholesale; without the route the copies
            // below would silently land on the DEFAULT backend.
            ensure_route(
                mutator,
                &params.transient_key,
                &params.target_backend,
                bucket,
                "Migration staging route re-asserted after config change",
            )
            .await?;
            let engine = state.engine.load().clone();
            let page = match engine
                .list_objects(bucket, "", None, PAGE_SIZE, pager.token(), false)
                .await
            {
                Ok(p) => p,
                Err(e) if pager.poisoned_resume_token() => {
                    // Restart the phase from page 0: HEAD-skip makes the
                    // re-list idempotent. (Counters are NOT reset, so
                    // already-copied objects tally as `skipped` a second
                    // time — display drift only, never a re-copy.)
                    tracing::warn!(
                        "migrate: job #{} copy resume token rejected ({e}); restarting phase fresh",
                        job.id
                    );
                    pager.restart_fresh();
                    persist(db, job, "copy", None, done, skipped, failed, bytes, None).await;
                    continue;
                }
                Err(e) => return Err(format!("list source failed: {e}")),
            };
            for (key, _) in page.objects.iter().filter(|(k, _)| !k.ends_with('/')) {
                if engine.head(&params.transient_key, key).await.is_ok() {
                    skipped += 1;
                    continue;
                }
                let req = ObjectTransferRequest {
                    source_bucket: bucket,
                    source_key: key,
                    destination_bucket: &params.transient_key,
                    destination_key: key,
                    provenance: Some(TransferProvenance {
                        metadata_key: "dg-migration",
                        metadata_value: &provenance_value,
                    }),
                    strip_user_metadata_keys: &[],
                    operation: "migrate",
                };
                match copy_object_with_retries(&engine, req).await {
                    Ok(outcome) => {
                        done += 1;
                        bytes += outcome.bytes_copied as i64;
                    }
                    Err(e) => {
                        failed += 1;
                        record_failure(db, job.id, key, &e.to_string()).await;
                        persist(db, job, "copy", None, done, skipped, failed, bytes, None).await;
                        return Err(format!(
                            "copy of '{key}' failed — source remains authoritative: {e}"
                        ));
                    }
                }
            }
            let more = pager.advance(page.is_truncated, page.next_continuation_token);
            persist(
                db,
                job,
                "copy",
                None,
                done,
                skipped,
                failed,
                bytes,
                pager.token(),
            )
            .await;
            heartbeat(db, job.id, instance_id).await;
            if !more {
                break;
            }
        }
        phase = "verify".to_string();
        persist(db, job, &phase, None, done, skipped, failed, bytes, None).await;
    }

    // ── Phase: verify ──
    if phase == "verify" {
        let mut pager = Pager::resuming(if job.phase == "verify" {
            resume_token.clone()
        } else {
            None
        });
        while pager.begin_page().is_some() {
            check_cancel(db, job.id).await?;
            let engine = state.engine.load().clone();
            let page = match engine
                .list_objects(bucket, "", None, PAGE_SIZE, pager.token(), false)
                .await
            {
                Ok(p) => p,
                Err(e) if pager.poisoned_resume_token() => {
                    tracing::warn!(
                        "migrate: job #{} verify resume token rejected ({e}); restarting phase fresh",
                        job.id
                    );
                    pager.restart_fresh();
                    persist(db, job, "verify", None, done, skipped, failed, bytes, None).await;
                    continue;
                }
                Err(e) => return Err(format!("verify list failed: {e}")),
            };
            for (key, _) in page.objects.iter().filter(|(k, _)| !k.ends_with('/')) {
                if engine.head(&params.transient_key, key).await.is_err() {
                    return Err(format!("verification failed: '{key}' missing on target"));
                }
            }
            let more = pager.advance(page.is_truncated, page.next_continuation_token);
            persist(
                db,
                job,
                "verify",
                None,
                done,
                skipped,
                failed,
                bytes,
                pager.token(),
            )
            .await;
            heartbeat(db, job.id, instance_id).await;
            if !more {
                break;
            }
        }
        phase = "flip".to_string();
        persist(db, job, &phase, None, done, skipped, failed, bytes, None).await;
    }

    // ── Phase: flip (idempotent; NOT interruptible) ──
    if phase == "flip" {
        let bucket_key = bucket.clone();
        let target = params.target_backend.clone();
        let transient = params.transient_key.clone();
        mutator
            // STRICT: the flip's file-persist must succeed or the whole flip
            // rolls back (engine swapped back to source). A file that lags
            // the flip is the data-loss crash window: boot would route
            // clients to the source while the resumed cleanup deletes it.
            .mutate_and_apply_strict(
                &format!("Bucket '{bucket_key}' migrated to backend '{target}'"),
                move |cfg| {
                    let mut policy = cfg.buckets.get(&bucket_key).cloned().unwrap_or_default();
                    policy.backend = Some(target);
                    cfg.buckets.insert(bucket_key, policy);
                    cfg.buckets.remove(&transient);
                },
            )
            .await?;
        // Destination is authoritative — client writes resume NOW, not at
        // job settle (cleanup below doesn't need the gate).
        state.maintenance_gate.clear(bucket);
        info!(
            "migrate: bucket '{}' flipped to backend '{}'",
            bucket, params.target_backend
        );
        phase = "cleanup".to_string();
        persist(db, job, &phase, None, done, skipped, failed, bytes, None).await;
    }

    // ── Phase: cleanup (optional delete-source; never fails the job) ──
    if phase == "cleanup" && params.delete_source {
        // Defense-in-depth for the crash window: deleting source objects is
        // only safe if the LIVE config genuinely routes the bucket to the
        // target. A resumed job whose flip didn't stick (file lagged the
        // engine, then a crash) must fail loudly instead of deleting a
        // bucket clients are actively writing to.
        let routed_to_target = {
            let cfg = mutator.read().await;
            cfg.buckets
                .get(bucket)
                .and_then(|p| p.backend.as_deref().map(|b| b == params.target_backend))
                .unwrap_or(false)
        };
        if !routed_to_target {
            return Err(format!(
                "cleanup refused: bucket '{}' is not routed to '{}' in the live \
                 config — the flip did not persist; source data left untouched",
                bucket, params.target_backend
            ));
        }
        let cleanup_key = format!("{}__src", params.transient_key);
        ensure_route(
            mutator,
            &cleanup_key,
            &params.from_backend,
            bucket,
            "Migration source-cleanup route staged",
        )
        .await?;
        let mut cleanup_token: Option<String> = None;
        let mut delete_failures = 0u32;
        'cleanup: for _ in 0..MAX_JOB_PAGES {
            if check_cancel(db, job.id).await.is_err() {
                break 'cleanup; // flip already happened; stop deleting quietly
            }
            let engine = state.engine.load().clone();
            let page = match engine
                .list_objects(
                    &cleanup_key,
                    "",
                    None,
                    PAGE_SIZE,
                    cleanup_token.as_deref(),
                    false,
                )
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    delete_failures += 1;
                    record_failure(db, job.id, "", &format!("cleanup list failed: {e}")).await;
                    break 'cleanup;
                }
            };
            let mut deleted_this_sweep = 0u32;
            for (key, _) in page.objects.iter().filter(|(k, _)| !k.ends_with('/')) {
                match engine.delete(&cleanup_key, key).await {
                    Ok(()) => deleted_this_sweep += 1,
                    Err(e) => {
                        delete_failures += 1;
                        record_failure(db, job.id, key, &format!("source delete failed: {e}"))
                            .await;
                    }
                }
            }
            // Deletes shrink the listing — restart from the top each sweep.
            // A sweep that deleted NOTHING will never converge: stop instead
            // of re-listing the same page (and re-recording the same
            // failures) up to the page cap.
            if page.next_continuation_token.is_none() || deleted_this_sweep == 0 {
                break 'cleanup;
            }
            cleanup_token = None;
            heartbeat(db, job.id, instance_id).await;
        }
        remove_routes(
            mutator,
            &[cleanup_key],
            "Migration source-cleanup route removed",
        )
        .await;
        if delete_failures > 0 {
            // Recorded, surfaced, but the MIGRATION succeeded.
            let db = db.lock().await;
            let _ = db.maintenance_finish(
                job.id,
                "completed",
                Some(&format!(
                    "source cleanup incomplete ({delete_failures} failure(s)) — \
                     remaining source objects can be removed manually"
                )),
            );
            return Ok(());
        }
    }

    let _ = CANCELLED; // referenced for doc-parity with reencrypt
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_round_trip() {
        let p = MigrateParams {
            target_backend: "hz".into(),
            delete_source: true,
            transient_key: "__dgmigrate_b_0".into(),
            from_backend: "local".into(),
        };
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(parse_params(&json).unwrap(), p);
        assert!(parse_params("nope").is_err());
        assert!(parse_params("{}").is_err(), "missing fields rejected");
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
