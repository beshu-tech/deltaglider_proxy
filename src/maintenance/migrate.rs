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
    check_cancel, drain_inflight_writes, heartbeat, persist, record_failure, LEASE_LOST,
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

    let lease_lost = result.as_ref().err().is_some_and(|e| e == LEASE_LOST);
    if result.is_err() && !lease_lost {
        // (Lease loss is NOT an unwind: the job continues under the next
        // claimer, which needs the staging route — and re-asserts it per
        // page anyway.)
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
        // Real bucket on the target (idempotent). "Already exists" must be
        // tolerated for crash-resume, but backend error strings don't
        // reliably contain "exist" (the AWS SDK renders a 409 as a terse
        // "service error") — so on ANY create failure, probe the bucket
        // through the staging route instead of string-matching.
        let engine = state.engine.load().clone();
        if let Err(e) = engine.create_bucket(&params.transient_key).await {
            if engine
                .list_objects(&params.transient_key, "", None, 1, None, false)
                .await
                .is_err()
            {
                return Err(format!("create bucket on target failed: {e}"));
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
                    upload_concurrency: None,
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
            heartbeat(db, job.id, instance_id).await?;
            if !more {
                break;
            }
        }
        if pager.truncated_by_page_budget() {
            // Falling through to verify here would "verify" (and later flip
            // + delete) over a silently truncated listing — never-copied
            // tail objects would be lost. Fail instead; the persisted
            // cursor resumes the tail on retry.
            return Err("copy stopped at the page budget with more source pages \
                 pending — bucket too large for one pass; job left resumable \
                 in phase 'copy' (cursor persisted, source authoritative)"
                .to_string());
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
            heartbeat(db, job.id, instance_id).await?;
            if !more {
                break;
            }
        }
        if pager.truncated_by_page_budget() {
            return Err("verify stopped at the page budget with more source pages \
                 pending — refusing to flip over an incompletely verified \
                 listing; job left resumable in phase 'verify'"
                .to_string());
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
        // job settle (cleanup below doesn't need the gate). The transient
        // route was just removed from the config, so its gate entry goes
        // too.
        state.maintenance_gate.clear(bucket);
        state.maintenance_gate.clear(&params.transient_key);
        info!(
            "migrate: bucket '{}' flipped to backend '{}'",
            bucket, params.target_backend
        );
        phase = "cleanup".to_string();
        persist(db, job, &phase, None, done, skipped, failed, bytes, None).await;
    }

    // ── Phase: cleanup (optional delete-source; never fails the job) ──
    if phase == "cleanup" && params.delete_source {
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
        let mut cancelled_mid_cleanup = false;
        'cleanup: for _ in 0..MAX_JOB_PAGES {
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
                return Err(format!(
                    "cleanup refused: bucket '{}' is not routed to '{}' in the live \
                     config — source data left untouched",
                    bucket, params.target_backend
                ));
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
                    Ok(_) => deleted_this_sweep += 1,
                    Err(e) => {
                        delete_failures += 1;
                        record_failure(db, job.id, key, &format!("source delete failed: {e}"))
                            .await;
                    }
                }
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
            heartbeat(db, job.id, instance_id).await?;
        }
        remove_routes(
            mutator,
            &[cleanup_key],
            "Migration source-cleanup route removed",
        )
        .await;
        // The MIGRATION succeeded either way; an interrupted cleanup is
        // surfaced as a note, never as a failed/cancelled job.
        let note = if cancelled_mid_cleanup {
            Some(format!(
                "source cleanup stopped by cancel{} — remaining source \
                 objects can be removed manually",
                if delete_failures > 0 {
                    format!(" ({delete_failures} failure(s))")
                } else {
                    String::new()
                }
            ))
        } else if delete_failures > 0 {
            Some(format!(
                "source cleanup incomplete ({delete_failures} failure(s)) — \
                 remaining source objects can be removed manually"
            ))
        } else {
            None
        };
        if let Some(note) = note {
            let db = db.lock().await;
            let _ = db.maintenance_finish(job.id, "completed", Some(&note));
            return Ok(());
        }
    }

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
