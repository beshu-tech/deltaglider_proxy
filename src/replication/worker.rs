//! Replication worker: executes one full run of a single rule against
//! a live engine + config DB.
//!
//! What `run_rule` does (H1+H2+H3+M1 fixes wave):
//!
//! 1. Loops engine `list_objects` pages until exhaustion. After each
//!    page the worker persists `replication_state.continuation_token`
//!    so a crash mid-run resumes on the next tick instead of starting
//!    over from page 1.
//! 2. Per-object: HEAD destination, consult planner, `engine.retrieve`
//!    source, `engine.store_with_multipart_etag` (when source carries
//!    one) or `engine.store` (single-PUT objects). Preserves the H1
//!    multipart-ETag identity across replication.
//! 3. After the forward-copy pass, when `replicate_deletes` is true,
//!    paginates the destination prefix and deletes any key not present
//!    on source.
//! 4. Records per-object failures into the failure ring.
//! 5. Final status: `"failed"` when ANY copy/delete errored, else
//!    `"succeeded"`. Pre-fix the status was only flipped to failed when
//!    EVERY copy failed, so dashboards reading `last_status` got a
//!    silent partial failure.
//!
//! Resumability: after a successful complete pass the
//! `continuation_token` is cleared. If the worker crashes mid-pass,
//! `reconcile_on_boot` flips the running row to `failed` but the token
//! stays — next legitimate run resumes from the saved cursor.

use super::planner::{normalize_prefix, plan_batch};
use super::state_store::{current_unix_seconds, FailureInsert, RunTotals};
use crate::config_db::ConfigDb;
use crate::config_sections::ReplicationRule;
use crate::deltaglider::DynEngine;
use crate::event_outbox::{EventKind, EventSource, NewEvent};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// User-metadata key stamped on objects created by replication so the
/// delete pass (H2 fix) can tell its own copies apart from objects
/// written by other rules or operators sharing the same destination
/// prefix. Value is the rule name.
///
/// Why a user-metadata key (not a system-managed marker): user-metadata
/// round-trips through both backends without any DG-specific plumbing,
/// survives encryption (per-backend SSE doesn't encrypt user-metadata),
/// and is visible to operators auditing what wrote a given object.
const REPLICATION_RULE_METADATA_KEY: &str = "dg-replication-rule";
const COPY_MAX_ATTEMPTS: u32 = 3;

/// Outcome of a single run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    /// Terminal status string (goes into `replication_run_history.status`).
    pub status: String,
    pub totals: RunTotals,
}

#[derive(Debug, Clone)]
pub struct RunLease {
    pub owner: String,
    pub ttl_secs: i64,
    pub heartbeat_secs: i64,
}

/// Bound on the number of pages consumed in a single run, defending
/// against pathological cases where pagination loops forever (the
/// engine reports is_truncated=true but no next token, for example).
const MAX_PAGES_PER_RUN: u32 = 10_000;

pub async fn run_rule(
    db: Arc<Mutex<ConfigDb>>,
    engine: &Arc<DynEngine>,
    rule: &ReplicationRule,
    max_failures_retained: u32,
    triggered_by: &str,
    lease: Option<RunLease>,
) -> Result<(i64, RunOutcome), crate::config_db::ConfigDbError> {
    let started_at = current_unix_seconds();

    // Look up the saved continuation token to resume from a prior tick.
    // Cleared at the end of a successful complete pass.
    let (run_id, mut continuation) = {
        let db = db.lock().await;
        db.replication_ensure_state(&rule.name, started_at)?;
        let state = db.replication_load_state(&rule.name)?;
        let resume_token = state.and_then(|s| s.continuation_token);
        let id = db.replication_begin_run(&rule.name, started_at, triggered_by)?;
        (id, resume_token)
    };

    info!(
        "Replication run starting: rule='{}' src={}/{} dst={}/{} resuming={}",
        rule.name,
        rule.source.bucket,
        rule.source.prefix,
        rule.destination.bucket,
        rule.destination.prefix,
        continuation.is_some(),
    );

    let mut totals = RunTotals::default();
    let mut had_any_error = false;
    let mut hit_fatal_error = false;
    let cap = rule.batch_size.clamp(1, 10_000);
    let source_prefix = normalize_prefix(&rule.source.prefix);
    let lease_alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let heartbeat_handle =
        spawn_lease_heartbeat(db.clone(), &rule.name, lease.clone(), lease_alive.clone());

    // ── Forward-copy pass: paginate source until exhausted ──
    'pages: for page_idx in 0..MAX_PAGES_PER_RUN {
        if !renew_run_lease(
            &db,
            rule,
            lease.as_ref(),
            &lease_alive,
            run_id,
            max_failures_retained,
        )
        .await?
        {
            totals.errors += 1;
            hit_fatal_error = true;
            break 'pages;
        }

        let page = match engine
            .list_objects(
                &rule.source.bucket,
                &source_prefix,
                None,
                cap,
                continuation.as_deref(),
                true,
            )
            .await
        {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    "replication rule '{}' list page {} failed: {}",
                    rule.name, page_idx, e
                );
                log_failure(
                    &db,
                    &rule.name,
                    run_id,
                    "",
                    "",
                    &format!("list source failed: {}", e),
                    max_failures_retained,
                )
                .await?;
                totals.errors += 1;
                hit_fatal_error = true;
                break 'pages;
            }
        };

        totals.objects_scanned += page.objects.len() as i64;

        // Plan this page. The planner heads each destination key and
        // applies the conflict policy + glob filters.
        let plan = {
            let head_engine = engine.clone();
            let dest_bucket = rule.destination.bucket.clone();
            plan_batch(&page.objects, rule, move |dest_key| {
                let engine = head_engine.clone();
                let dest_bucket = dest_bucket.clone();
                let dk = dest_key.to_string();
                async move { engine.head(&dest_bucket, &dk).await.ok() }
            })
            .await
        };

        let plan = match plan {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    "replication rule '{}' page {} planner error: {}",
                    rule.name, page_idx, e
                );
                log_failure(
                    &db,
                    &rule.name,
                    run_id,
                    "",
                    "",
                    &format!("planner error: {}", e),
                    max_failures_retained,
                )
                .await?;
                totals.errors += 1;
                hit_fatal_error = true;
                break 'pages;
            }
        };

        totals.objects_skipped += plan.skipped.len() as i64;

        // Execute the copies for this page.
        for (src_key, dest_key) in &plan.to_copy {
            if !renew_run_lease(
                &db,
                rule,
                lease.as_ref(),
                &lease_alive,
                run_id,
                max_failures_retained,
            )
            .await?
            {
                totals.errors += 1;
                hit_fatal_error = true;
                break 'pages;
            }
            match copy_one_with_retries(engine, rule, src_key, dest_key).await {
                Ok(bytes_copied) => {
                    totals.objects_copied += 1;
                    totals.bytes_copied += bytes_copied as i64;
                    {
                        let db = db.lock().await;
                        if let Err(err) = db.event_outbox_insert(&NewEvent::new(
                            EventKind::ReplicationObjectCopied,
                            rule.destination.bucket.as_str(),
                            dest_key.as_str(),
                            EventSource::Replication,
                            current_unix_seconds(),
                            serde_json::json!({
                                "rule_name": &rule.name,
                                "source_bucket": &rule.source.bucket,
                                "source_key": src_key.as_str(),
                                "destination_bucket": &rule.destination.bucket,
                                "destination_key": dest_key.as_str(),
                                "content_length": bytes_copied,
                            }),
                        )) {
                            warn!(
                                "replication rule '{}' could not append event for {:?} -> {:?}: {}",
                                rule.name, src_key, dest_key, err
                            );
                        }
                    }
                }
                Err(e) => {
                    totals.errors += 1;
                    had_any_error = true;
                    log_failure(
                        &db,
                        &rule.name,
                        run_id,
                        src_key,
                        dest_key,
                        &format!("{}", e),
                        max_failures_retained,
                    )
                    .await?;
                    debug!(
                        "replication rule '{}' object failure src={:?} dst={:?}: {}",
                        rule.name, src_key, dest_key, e
                    );
                }
            }
            {
                let db = db.lock().await;
                db.replication_update_run_progress(run_id, totals)?;
            }
        }

        // Persist the cursor so the next tick can resume here if we
        // crash before the run finishes naturally.
        continuation = page.next_continuation_token.clone();
        {
            let db = db.lock().await;
            db.replication_set_continuation_token(&rule.name, continuation.as_deref())?;
            db.replication_update_run_progress(run_id, totals)?;
        }

        if !page.is_truncated || continuation.is_none() {
            break 'pages;
        }
    }

    // ── Delete-replication pass (opt-in per rule) ──
    //
    // After the forward copy completes, paginate the destination prefix
    // and delete every key whose corresponding source key is missing.
    // Only fires when forward-copy didn't hit a fatal error — partial
    // listing failures could leave us thinking source is empty when
    // it's not, and a full destination wipe would be catastrophic.
    if rule.replicate_deletes && !hit_fatal_error {
        if let Err(e) = run_delete_pass(
            db.clone(),
            engine,
            rule,
            run_id,
            &mut totals,
            &mut had_any_error,
            max_failures_retained,
        )
        .await
        {
            warn!("replication rule '{}' delete pass error: {}", rule.name, e);
            had_any_error = true;
        }
    }

    // Final status: any failure (fatal OR per-object) → "failed".
    // Pre-fix the status was only "failed" when EVERY copy errored,
    // which silently lied to dashboards on partial-failure runs (M1).
    let status = if hit_fatal_error || had_any_error {
        "failed".to_string()
    } else {
        "succeeded".to_string()
    };

    let finished_at = current_unix_seconds();
    let next_due = if hit_fatal_error {
        // Tighter retry on fatal errors so the operator-facing
        // "next due" doesn't claim a long sleep when the worker
        // gave up immediately.
        finished_at + 60
    } else {
        compute_next_due(rule, finished_at)
    };

    // Clear the continuation token on a clean complete pass — next
    // run starts from the beginning of the prefix.
    let clear_cursor_on_clean = !hit_fatal_error;

    {
        let db = db.lock().await;
        if clear_cursor_on_clean {
            db.replication_set_continuation_token(&rule.name, None)?;
        }
        db.replication_finish_run(run_id, &rule.name, &status, finished_at, totals, next_due)?;
    }

    info!(
        "Replication run finished: rule='{}' status={} scanned={} copied={} skipped={} deleted={} errors={} bytes={}",
        rule.name,
        status,
        totals.objects_scanned,
        totals.objects_copied,
        totals.objects_skipped,
        totals.objects_deleted,
        totals.errors,
        totals.bytes_copied,
    );
    if let Some(handle) = heartbeat_handle {
        handle.abort();
    }
    Ok((run_id, RunOutcome { status, totals }))
}

fn spawn_lease_heartbeat(
    db: Arc<Mutex<ConfigDb>>,
    rule_name: &str,
    lease: Option<RunLease>,
    lease_alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Option<tokio::task::JoinHandle<()>> {
    let lease = lease?;
    let rule_name = rule_name.to_string();
    let heartbeat_secs = lease.heartbeat_secs.max(1) as u64;
    Some(tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(heartbeat_secs);
        loop {
            tokio::time::sleep(interval).await;
            let renewed = {
                let db = db.lock().await;
                db.replication_renew_lease(
                    &rule_name,
                    &lease.owner,
                    current_unix_seconds(),
                    lease.ttl_secs,
                )
                .unwrap_or(false)
            };
            if !renewed {
                lease_alive.store(false, std::sync::atomic::Ordering::Release);
                warn!(
                    "Replication lease heartbeat lost for rule '{}'; worker will stop before more work",
                    rule_name
                );
                return;
            }
        }
    }))
}

async fn renew_run_lease(
    db: &Arc<Mutex<ConfigDb>>,
    rule: &ReplicationRule,
    lease: Option<&RunLease>,
    lease_alive: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    run_id: i64,
    max_failures_retained: u32,
) -> Result<bool, crate::config_db::ConfigDbError> {
    let Some(lease) = lease else {
        return Ok(true);
    };
    if !lease_alive.load(std::sync::atomic::Ordering::Acquire) {
        return record_lost_lease(db, &rule.name, run_id, max_failures_retained).await;
    }
    let now = current_unix_seconds();
    let guard = db.lock().await;
    if guard.replication_renew_lease(&rule.name, &lease.owner, now, lease.ttl_secs)? {
        return Ok(true);
    }
    drop(guard);
    record_lost_lease(db, &rule.name, run_id, max_failures_retained).await
}

async fn record_lost_lease(
    db: &Arc<Mutex<ConfigDb>>,
    rule_name: &str,
    run_id: i64,
    max_failures_retained: u32,
) -> Result<bool, crate::config_db::ConfigDbError> {
    log_failure(
        db,
        rule_name,
        run_id,
        "",
        "",
        "lost replication lease; stopping run before more work",
        max_failures_retained,
    )
    .await?;
    Ok(false)
}

async fn log_failure(
    db: &Arc<Mutex<ConfigDb>>,
    rule_name: &str,
    run_id: i64,
    source_key: &str,
    dest_key: &str,
    error_message: &str,
    max_failures_retained: u32,
) -> Result<(), crate::config_db::ConfigDbError> {
    let db = db.lock().await;
    db.replication_record_failure(
        rule_name,
        FailureInsert {
            run_id: Some(run_id),
            occurred_at: current_unix_seconds(),
            source_key,
            dest_key,
            error_message,
        },
        max_failures_retained,
    )
}

async fn copy_one_with_retries(
    engine: &Arc<DynEngine>,
    rule: &ReplicationRule,
    src_key: &str,
    dest_key: &str,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let mut last_err: Option<String> = None;
    for attempt in 1..=COPY_MAX_ATTEMPTS {
        match copy_one(engine, rule, src_key, dest_key).await {
            Ok(bytes) => return Ok(bytes),
            Err(err) => {
                let msg = err.to_string();
                if !is_transient_copy_error(&msg) || attempt == COPY_MAX_ATTEMPTS {
                    return Err(if attempt > 1 {
                        format!("{} (after {} attempts)", msg, attempt).into()
                    } else {
                        msg.into()
                    });
                }
                warn!(
                    "replication rule '{}' transient copy failure attempt {}/{} src={:?} dst={:?}: {}",
                    rule.name, attempt, COPY_MAX_ATTEMPTS, src_key, dest_key, msg
                );
                last_err = Some(msg);
                tokio::time::sleep(std::time::Duration::from_millis(250 * attempt as u64)).await;
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| "copy failed without error detail".to_string())
        .into())
}

fn is_transient_copy_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    [
        "failed to read response body",
        "streaming error",
        "connection reset",
        "connection closed",
        "connection aborted",
        "timed out",
        "timeout",
        "temporary failure",
        "service unavailable",
        "slowdown",
        "internalerror",
        "broken pipe",
    ]
    .iter()
    .any(|needle| m.contains(needle))
}

/// Delete-replication pass: paginate the destination prefix; for each
/// key that's NOT on source, delete it from destination.
///
/// The key check is HEAD-on-source (cheaper than re-listing). If the
/// HEAD succeeds the source has it → keep destination's copy. If the
/// HEAD returns NotFound → delete destination.
///
/// Other errors (network, AccessDenied) are recorded as failures and
/// the destination key is preserved. Better to leave an extra copy than
/// to false-delete on a transient.
async fn run_delete_pass(
    db: Arc<Mutex<ConfigDb>>,
    engine: &Arc<DynEngine>,
    rule: &ReplicationRule,
    run_id: i64,
    totals: &mut RunTotals,
    had_any_error: &mut bool,
    max_failures_retained: u32,
) -> Result<(), crate::config_db::ConfigDbError> {
    let cap = rule.batch_size.clamp(1, 10_000);
    let mut cursor: Option<String> = None;
    let destination_prefix = normalize_prefix(&rule.destination.prefix);

    'pages: for page_idx in 0..MAX_PAGES_PER_RUN {
        // metadata=true so user_metadata (carrying our provenance
        // marker, H2 fix) is populated in the listing — saves a
        // per-object HEAD round-trip.
        let page = match engine
            .list_objects(
                &rule.destination.bucket,
                &destination_prefix,
                None,
                cap,
                cursor.as_deref(),
                true,
            )
            .await
        {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    "replication rule '{}' delete-pass list page {} failed: {}",
                    rule.name, page_idx, e
                );
                log_failure(
                    &db,
                    &rule.name,
                    run_id,
                    "",
                    "",
                    &format!("delete-pass list dest failed: {}", e),
                    max_failures_retained,
                )
                .await?;
                totals.errors += 1;
                *had_any_error = true;
                return Ok(());
            }
        };

        for (dest_key, listed_meta) in &page.objects {
            // H2 fix: only consider deleting objects this rule wrote.
            // Each replicated copy carries `dg-replication-rule = <rule.name>`
            // in user_metadata (stamped by `copy_one`). If the listed
            // metadata is missing (LIST without metadata=true) or the
            // marker doesn't match, skip — never delete unrelated
            // objects, even if their key-after-prefix-rewrite happens
            // to be missing on source.
            //
            // The list call below already passes `metadata=true` so
            // user_metadata is populated. Defence in depth: if it's
            // empty, we HEAD to confirm before any delete.
            let has_marker_in_listing = listed_meta
                .user_metadata
                .get(REPLICATION_RULE_METADATA_KEY)
                .map(|v| v == &rule.name)
                .unwrap_or(false);

            let owned_by_this_rule = if has_marker_in_listing {
                true
            } else {
                // Listing didn't carry user-metadata (some backends
                // omit it). HEAD the object to be sure.
                match engine.head(&rule.destination.bucket, dest_key).await {
                    Ok(meta) => meta
                        .user_metadata
                        .get(REPLICATION_RULE_METADATA_KEY)
                        .map(|v| v == &rule.name)
                        .unwrap_or(false),
                    // HEAD failed — preserve. Better to leak a
                    // candidate than false-delete a foreign object.
                    Err(_) => false,
                }
            };

            if !owned_by_this_rule {
                debug!(
                    "replication rule '{}' delete-pass skip (no provenance marker): {:?}",
                    rule.name, dest_key
                );
                continue;
            }

            // Translate dest key back to its source counterpart.
            let src_key = match dest_to_source_key(rule, dest_key) {
                Some(k) => k,
                None => {
                    // Key sits outside the rule's destination-prefix
                    // (paranoid case: marker matched but prefix doesn't).
                    continue;
                }
            };

            // HEAD source. NotFound → delete destination (we wrote it,
            // it's still under our prefix, source no longer has the
            // key — this is a legitimate deletion to replicate).
            // Other errors → leave alone, log as failure.
            match engine.head(&rule.source.bucket, &src_key).await {
                Ok(_) => {
                    // Source still has it. Skip.
                }
                Err(e) => {
                    let s3_err: crate::api::S3Error = e.into();
                    if matches!(s3_err, crate::api::S3Error::NoSuchKey(_)) {
                        // Source missing → replicate the deletion.
                        match engine.delete(&rule.destination.bucket, dest_key).await {
                            Ok(()) => {
                                totals.objects_deleted += 1;
                            }
                            Err(de) => {
                                totals.errors += 1;
                                *had_any_error = true;
                                log_failure(
                                    &db,
                                    &rule.name,
                                    run_id,
                                    &src_key,
                                    dest_key,
                                    &format!("destination delete failed: {}", de),
                                    max_failures_retained,
                                )
                                .await?;
                            }
                        }
                    } else {
                        // Anything else: log & preserve. False-delete
                        // would be much worse than a leftover copy.
                        totals.errors += 1;
                        *had_any_error = true;
                        log_failure(
                            &db,
                            &rule.name,
                            run_id,
                            &src_key,
                            dest_key,
                            &format!("delete-pass head source failed: {}", s3_err),
                            max_failures_retained,
                        )
                        .await?;
                    }
                }
            }
        }

        cursor = page.next_continuation_token;
        if !page.is_truncated || cursor.is_none() {
            break 'pages;
        }
    }

    Ok(())
}

/// Translate a destination key back to its source-side counterpart by
/// reversing the prefix-rewrite the planner applies.
///
/// Returns `None` when the destination key doesn't start with the
/// rule's destination prefix (which means it's outside this rule's
/// jurisdiction; the delete pass leaves it alone).
fn dest_to_source_key(rule: &ReplicationRule, dest_key: &str) -> Option<String> {
    let dst_prefix = normalize_prefix(&rule.destination.prefix);
    let src_prefix = normalize_prefix(&rule.source.prefix);
    let dst_prefix = dst_prefix.as_str();
    let src_prefix = src_prefix.as_str();
    if dst_prefix.is_empty() && src_prefix.is_empty() {
        return Some(dest_key.to_string());
    }
    if dst_prefix == src_prefix {
        return Some(dest_key.to_string());
    }
    if dst_prefix.is_empty() {
        return Some(format!(
            "{}{}",
            src_prefix,
            dest_key.trim_start_matches('/')
        ));
    }
    let tail = dest_key.strip_prefix(dst_prefix)?;
    Some(format!("{}{}", src_prefix, tail.trim_start_matches('/')))
}

/// Copy a single object: engine.retrieve from source, engine.store on
/// destination. Honours the source's `multipart_etag` (H3 fix) so the
/// destination's HEAD returns the same ETag the original Complete
/// response advertised. Returns bytes copied.
async fn copy_one(
    engine: &Arc<DynEngine>,
    rule: &ReplicationRule,
    src_key: &str,
    dest_key: &str,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    // HEAD first so we can error cleanly if the source disappeared
    // mid-run (retrieve would also error, but this gives a crisper
    // cause on the failure row).
    let _src_meta = engine.head(&rule.source.bucket, src_key).await.map_err(
        |e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("source head failed: {}", e).into()
        },
    )?;

    let (data, meta) = engine
        .retrieve(&rule.source.bucket, src_key)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("source retrieve failed: {}", e).into()
        })?;

    let content_type = meta.content_type.clone();
    let mut user_metadata = meta.user_metadata.clone();
    let bytes = data.len();

    // H2 fix: stamp our rule name so the delete pass can tell this
    // object apart from anything written by another rule or by an
    // operator sharing the destination prefix. Without this marker,
    // `replicate_deletes=true` would happily delete unrelated objects
    // whose source-counterpart key happens to be missing.
    user_metadata.insert(REPLICATION_RULE_METADATA_KEY.to_string(), rule.name.clone());

    // H3 fix: when the source carries a multipart_etag, route through
    // store_with_multipart_etag so the destination HEAD reports the
    // same ETag the source's CompleteMultipartUpload advertised. Pre-
    // fix, replication silently rewrote the ETag to a full-body MD5,
    // making source != dest from the client's view.
    if let Some(mp_etag) = meta.multipart_etag.clone() {
        engine
            .store_with_multipart_etag(
                &rule.destination.bucket,
                dest_key,
                &data,
                content_type,
                user_metadata,
                mp_etag,
            )
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("destination store failed: {}", e).into()
            })?;
    } else {
        engine
            .store(
                &rule.destination.bucket,
                dest_key,
                &data,
                content_type,
                user_metadata,
            )
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("destination store failed: {}", e).into()
            })?;
    }

    Ok(bytes)
}

/// Compute when this rule should next be due. Falls back to a 1-hour
/// recovery window if the rule's `interval` is unparseable (should
/// never happen in practice — validated at Config::check time).
fn compute_next_due(rule: &ReplicationRule, finished_at: i64) -> i64 {
    match humantime::parse_duration(&rule.interval) {
        Ok(d) => finished_at + d.as_secs() as i64,
        Err(_) => finished_at + 3600,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_sections::{ConflictPolicy, ReplicationEndpoint, ReplicationRule};

    fn mk_rule() -> ReplicationRule {
        ReplicationRule {
            name: "r".to_string(),
            enabled: true,
            source: ReplicationEndpoint {
                bucket: "a".into(),
                prefix: String::new(),
            },
            destination: ReplicationEndpoint {
                bucket: "b".into(),
                prefix: String::new(),
            },
            interval: "1h".into(),
            batch_size: 100,
            replicate_deletes: false,
            conflict: ConflictPolicy::NewerWins,
            include_globs: Vec::new(),
            exclude_globs: vec![".dg/*".into()],
        }
    }

    #[test]
    fn compute_next_due_honours_interval() {
        let rule = mk_rule();
        assert_eq!(compute_next_due(&rule, 1000), 1000 + 3600);
    }

    #[test]
    fn compute_next_due_falls_back_on_invalid() {
        let mut rule = mk_rule();
        rule.interval = "garbage".into();
        assert_eq!(compute_next_due(&rule, 1000), 1000 + 3600);
    }

    #[test]
    fn running_progress_updates_history_before_finish() {
        let db = ConfigDb::in_memory("testpass").unwrap();
        db.replication_ensure_state("r", 100).unwrap();
        let run_id = db.replication_begin_run("r", 100, "scheduler").unwrap();
        let totals = RunTotals {
            objects_scanned: 10,
            objects_copied: 4,
            objects_skipped: 6,
            objects_deleted: 0,
            bytes_copied: 1234,
            errors: 2,
        };
        db.replication_update_run_progress(run_id, totals).unwrap();

        let runs = db.replication_recent_runs("r", 1).unwrap();
        assert_eq!(runs[0].status, "running");
        assert_eq!(runs[0].objects_scanned, 10);
        assert_eq!(runs[0].objects_copied, 4);
        assert_eq!(runs[0].errors, 2);
    }

    #[test]
    fn dest_to_source_key_identity_when_prefixes_empty() {
        let rule = mk_rule();
        assert_eq!(
            dest_to_source_key(&rule, "file.txt"),
            Some("file.txt".to_string())
        );
    }

    #[test]
    fn dest_to_source_key_strips_destination_prefix() {
        let mut rule = mk_rule();
        rule.source.prefix = "releases/".into();
        rule.destination.prefix = "archive/2026/".into();
        assert_eq!(
            dest_to_source_key(&rule, "archive/2026/v1.zip"),
            Some("releases/v1.zip".to_string())
        );
    }

    #[test]
    fn dest_to_source_key_returns_none_for_outside_keys() {
        let mut rule = mk_rule();
        rule.destination.prefix = "archive/".into();
        assert_eq!(dest_to_source_key(&rule, "other-stuff/x.bin"), None);
    }

    #[test]
    fn dest_to_source_key_handles_empty_dest_prefix_with_src_prefix() {
        let mut rule = mk_rule();
        rule.source.prefix = "releases/".into();
        rule.destination.prefix = "".into();
        assert_eq!(
            dest_to_source_key(&rule, "v1.zip"),
            Some("releases/v1.zip".to_string())
        );
    }

    #[test]
    fn transient_copy_error_classification_is_narrow() {
        assert!(is_transient_copy_error(
            "source retrieve failed: Storage error: S3 error: Failed to read response body: streaming error"
        ));
        assert!(is_transient_copy_error("connection reset by peer"));
        assert!(is_transient_copy_error("503 SlowDown"));

        assert!(!is_transient_copy_error(
            "destination store failed: Storage error: Bucket not found: test-bucket"
        ));
        assert!(!is_transient_copy_error("AccessDenied"));
        assert!(!is_transient_copy_error("NoSuchKey"));
    }
}
