// SPDX-License-Identifier: GPL-3.0-only

//! Event-driven replication consumer.
//!
//! Replication is **event-driven**: object mutations (PUT/DELETE/COPY/
//! CompleteMultipartUpload) are appended to the durable `event_outbox` by the
//! S3 write path, and this consumer drains them in near-real time, fanning each
//! object out to the replication rules whose `source` matches. A slow per-rule
//! full reconcile (`worker::run_rule`, default 24h) is the self-healing safety
//! net — events are the primary trigger.
//!
//! ## Pub/sub via a per-listener cursor
//!
//! The outbox is append-only. Each independent listener (webhook delivery and
//! replication) keeps its OWN high-water `last_event_id` in `listener_cursors`
//! and reads `WHERE id > cursor`. The two listeners never contend on a shared
//! status column. Ordering is by the autoincrement `id` (the true arrival
//! order) — NOT `occurred_at`, which is wall-clock and can tie or regress.
//!
//! ## Per-key compaction
//!
//! Before acting, the consumer collapses all pending events for a single
//! `(bucket, key)` into ONE *liveness* verdict (Copy / Delete / Noop) via
//! [`compact_key_events`] — so create+modify+delete within one drain is a
//! single net action, not three. The actual Copy-vs-skip / Delete-vs-noop
//! idempotency is then the planner's job (`should_replicate` + a dest HEAD),
//! keeping the decision logic in exactly one place shared with reconcile.
//!
//! This module hosts the PURE helpers (filtering, compaction, routing); they
//! are unit-tested without any I/O. The background loop is `spawn_event_consumer`.

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::api::handlers::AppState;
use crate::config::SharedConfig;
use crate::config_db::ConfigDb;
use crate::config_sections::ReplicationRule;
use crate::event_outbox::{
    current_unix_seconds, EventKind, EventOutboxRecord, EventSource, NewEvent,
};
use crate::transfer::{
    copy_object_with_retries, ObjectTransferRequest, TransferProvenance,
    REPLICATION_RULE_METADATA_KEY,
};

use super::planner::{
    compile_rule_globs, normalize_prefix, rewrite_key, should_replicate, Decision,
};

/// The listener name under which event-driven replication tracks its outbox
/// cursor (independent of the webhook dispatcher).
pub const REPLICATION_LISTENER: &str = "replication";

/// The sentinel "rule name" the consumer's single-flight lease is keyed under,
/// so only one instance drains+advances the shared cursor at a time.
const CONSUMER_LEASE_KEY: &str = "__event_consumer__";

/// Max events drained per tick.
const DRAIN_BATCH: u32 = 500;

/// The net effect a batch of events has on a single object's destination copy.
/// Compaction reduces N events for one key to one of these; the consumer then
/// confirms the actual action against the destination via the planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// The key's final state is "present" — (re)copy it to the destination.
    Copy,
    /// The key's final state is "absent" — delete it at the destination (if we
    /// wrote it there and the rule replicates deletes).
    Delete,
    /// No events / nothing to do.
    Noop,
}

/// The source-side liveness a known event kind produces. `Copy` ⇒ object ends
/// up PRESENT, `Delete` ⇒ ABSENT. `None` ⇒ the kind carries no liveness signal
/// (not an object-state transition).
///
/// This is the single exhaustive `match` over [`EventKind`]: the `#[deny]`-free
/// non-wildcard arm set means adding a variant to `EventKind` fails to compile
/// here until it is explicitly classified, so a new event kind can never
/// silently fall through to `Noop` (Finding 1). The two string predicates below
/// derive from this so the DB-string path and the enum stay in lockstep.
fn liveness_of_kind(kind: EventKind) -> Option<KeyAction> {
    match kind {
        EventKind::ObjectCreated
        | EventKind::ObjectCopied
        | EventKind::ReplicationObjectCopied
        | EventKind::LifecycleTransitioned => Some(KeyAction::Copy),
        EventKind::ObjectDeleted | EventKind::LifecycleExpired => Some(KeyAction::Delete),
    }
}

/// Parse a raw outbox `kind` string back to the typed [`EventKind`], or `None`
/// for an unrecognized string (an old/foreign event kind that this build does
/// not know about). Mirrors [`EventKind::as_str`].
fn parse_event_kind(kind: &str) -> Option<EventKind> {
    match kind {
        "ObjectCreated" => Some(EventKind::ObjectCreated),
        "ObjectDeleted" => Some(EventKind::ObjectDeleted),
        "ObjectCopied" => Some(EventKind::ObjectCopied),
        "ReplicationObjectCopied" => Some(EventKind::ReplicationObjectCopied),
        "LifecycleExpired" => Some(EventKind::LifecycleExpired),
        "LifecycleTransitioned" => Some(EventKind::LifecycleTransitioned),
        _ => None,
    }
}

/// Event kinds whose effect leaves the object PRESENT at the source.
fn is_present_producing(kind: &str) -> bool {
    parse_event_kind(kind).and_then(liveness_of_kind) == Some(KeyAction::Copy)
}

/// Event kinds whose effect leaves the object ABSENT at the source.
fn is_absent_producing(kind: &str) -> bool {
    parse_event_kind(kind).and_then(liveness_of_kind) == Some(KeyAction::Delete)
}

/// Collapse a key's events (in ascending `id`/arrival order) to one liveness
/// verdict. **Last-event-wins**: the final event decides whether the source
/// object ends up present (→ `Copy`) or absent (→ `Delete`); an empty slice or
/// a final event of an unrecognized kind is `Noop`.
///
/// Rationale (locked decision): compaction only yields *liveness*. Whether a
/// `Copy` actually transfers (vs. dest already current) and whether a `Delete`
/// actually removes (vs. dest never had it) is decided downstream by the
/// planner + a dest HEAD — so idempotency lives in ONE place, not a second
/// per-key table.
///
/// `kinds` are the raw `EventOutboxRecord::kind` strings in id order.
pub fn compact_key_events(kinds: &[&str]) -> KeyAction {
    match kinds.last() {
        None => KeyAction::Noop,
        Some(k) if is_absent_producing(k) => KeyAction::Delete,
        Some(k) if is_present_producing(k) => KeyAction::Copy,
        // Unknown terminal kind. The compile-time `liveness_of_kind` match means
        // a NEW `EventKind` variant can't reach here un-classified — so this is
        // only an event-kind string this build genuinely doesn't recognize
        // (e.g. written by a newer/foreign instance). We still treat it as Noop
        // and let the cursor advance, but warn loudly: a silent replication drop
        // here would otherwise be invisible (Finding 1).
        Some(k) => {
            warn!(
                "event consumer: unrecognized terminal event kind {k:?}; treating as Noop \
                 (no replication action). If this is a new DeltaGlider event kind, update \
                 liveness_of_kind/parse_event_kind."
            );
            KeyAction::Noop
        }
    }
}

/// `true` for keys that represent a real user object — i.e. NOT a directory
/// marker, DeltaGlider config-sync internal, or storage-layer delta artifact
/// (`reference.bin`, `*.delta`). Shared by the write-path emit filter and the
/// consumer's routing so DG internals never generate replication work.
///
/// Mirrors the guard set in `planner::should_replicate` (`:186-201`) so the
/// emit filter and the planner agree on what's a user object.
pub fn is_user_object_key(key: &str) -> bool {
    if key.ends_with('/') {
        return false; // directory marker
    }
    if key.starts_with(".deltaglider/") || key.contains("/.deltaglider/") {
        return false; // config-sync internal
    }
    // Storage-layer delta artifacts. The engine listing usually hides these,
    // but filter defensively at the source boundary.
    let filename = key.rsplit('/').next().unwrap_or(key);
    if filename == "reference.bin" || filename.ends_with(".delta") {
        return false;
    }
    true
}

/// `true` iff the destination object carries THIS rule's provenance marker —
/// i.e. replication wrote it. The delete-pass (event-driven and reconcile)
/// keys off this so it only ever removes objects we created, never a
/// foreign/pre-existing object that happens to share a key. This is the
/// delete-safety lynchpin; it has an exhaustive unit truth-table.
pub fn owned_by_rule(meta: &crate::types::FileMetadata, rule_name: &str) -> bool {
    meta.user_metadata
        .get(REPLICATION_RULE_METADATA_KEY)
        .map(|v| v == rule_name)
        .unwrap_or(false)
}

/// The highest CONTIGUOUS event id that fully succeeded, given the drained
/// `rows` (ascending by id) and the set of ids whose key-action failed. Walk
/// ascending and stop at the first failed id: the cursor only ever moves
/// forward and never revisits, so advancing PAST a failed id would lose that
/// event permanently. Returns `cursor` unchanged when the first row already
/// failed.
fn contiguous_watermark(
    rows: &[EventOutboxRecord],
    failed_ids: &std::collections::BTreeSet<i64>,
    cursor: i64,
) -> i64 {
    let mut watermark = cursor;
    for rec in rows {
        if failed_ids.contains(&rec.id) {
            break;
        }
        watermark = rec.id;
    }
    watermark
}

/// Group outbox records by `(bucket, key)`, preserving id order within each
/// group (the input MUST already be ascending by id). The returned map's values
/// are the records for that key, oldest first — exactly what compaction wants.
pub fn group_events_by_key<'a>(
    records: &'a [EventOutboxRecord],
) -> BTreeMap<(&'a str, &'a str), Vec<&'a EventOutboxRecord>> {
    let mut groups: BTreeMap<(&'a str, &'a str), Vec<&'a EventOutboxRecord>> = BTreeMap::new();
    for rec in records {
        groups
            .entry((rec.bucket.as_str(), rec.key.as_str()))
            .or_default()
            .push(rec);
    }
    groups
}

/// Find the enabled replication rules whose `source` matches `(bucket, key)`:
/// same source bucket AND `key` falls under the (normalized) source prefix AND
/// the key is a user object. Glob include/exclude filtering is applied by the
/// caller via the planner (kept here to the cheap, allocation-free predicate so
/// the consumer can pre-filter before compiling globsets).
///
/// A key may match multiple rules — all are returned; the consumer fans the
/// action out to each.
pub fn match_rules<'a>(
    rules: &'a [ReplicationRule],
    bucket: &str,
    key: &str,
) -> Vec<&'a ReplicationRule> {
    if !is_user_object_key(key) {
        return Vec::new();
    }
    rules
        .iter()
        .filter(|rule| rule.enabled)
        .filter(|rule| rule.source.bucket == bucket)
        .filter(|rule| {
            let prefix = normalize_prefix(&rule.source.prefix);
            // Empty normalized prefix == whole bucket; otherwise require the key
            // to live under it (prefix already carries a trailing slash, so this
            // is a true path-boundary match, not a substring one).
            prefix.is_empty() || key.starts_with(&prefix)
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────
// Background consumer loop
// ─────────────────────────────────────────────────────────────────────────

/// Spawn the event-driven replication consumer. One per process; mirrors the
/// webhook dispatcher (`event_delivery::spawn_dispatcher`). Each tick it drains
/// new outbox events past its cursor and applies them to matching rules.
pub fn spawn_event_consumer(
    config: SharedConfig,
    db: Arc<Mutex<ConfigDb>>,
    state: Arc<AppState>,
) -> tokio::task::JoinHandle<()> {
    let instance_id = format!("event-consumer:{}", uuid::Uuid::new_v4());
    tokio::spawn(async move {
        info!(
            "Replication event consumer started: instance_id={}",
            instance_id
        );
        loop {
            let replication = { config.read().await.replication.clone() };
            let tick = super::scheduler::scheduler_tick(&replication);
            tokio::time::sleep(tick).await;

            if !replication.enabled {
                debug!("Event consumer skipped: replication disabled");
                continue;
            }
            // Seed the cursor at the current MAX(id) on first ENABLED boot so a
            // fresh consumer does NOT replay the entire historical outbox as
            // "new" — reconcile covers pre-existing state. Deliberately done
            // AFTER the `enabled` gate: a webhook-only deployment (replication
            // disabled) must never create a `replication` cursor row, because
            // that row pins the webhook pruner's delete floor
            // (`event_outbox_min_listener_cursor`) and would otherwise let the
            // outbox grow without bound while replication never advances it.
            seed_cursor_if_absent(&db).await;
            // Single-flight: only the consumer-lease holder drains+advances the
            // shared cursor, so two instances can't both move it / double-copy.
            //
            // The lease is acquired once per tick and NOT renewed mid-drain
            // (unlike the reconcile worker's heartbeat). A drain exceeding the
            // TTL can therefore let a second instance steal the lease and
            // overlap on the same cursor window. That is intentionally tolerated
            // because every action is idempotent: a re-Copy is gated by a dest
            // HEAD + `should_replicate` (a no-op when current), and a re-Delete
            // is provenance-guarded + HEADs an already-absent dest. The cursor
            // advance is monotonic (`MAX`), so overlap costs duplicate WORK, not
            // correctness. Per-rule leases below add a second mutual-exclusion
            // layer for the common case.
            let lease_ttl = super::scheduler::lease_ttl_secs(&replication);
            let now = current_unix_seconds();
            let acquired = {
                let dbg = db.lock().await;
                let _ = dbg.replication_ensure_state(CONSUMER_LEASE_KEY, now);
                dbg.replication_try_acquire_lease(CONSUMER_LEASE_KEY, &instance_id, now, lease_ttl)
                    .unwrap_or(false)
            };
            if !acquired {
                debug!("Event consumer tick skipped: another instance holds the lease");
                continue;
            }

            drain_once(&db, &state, &replication, &instance_id, now).await;

            let dbg = db.lock().await;
            let _ = dbg.replication_release_lease(CONSUMER_LEASE_KEY, &instance_id);
        }
    })
}

/// Seed the replication cursor to `MAX(event_outbox.id)` if it has no cursor yet
/// (don't replay history on first feature boot).
async fn seed_cursor_if_absent(db: &Arc<Mutex<ConfigDb>>) {
    let dbg = db.lock().await;
    if dbg.listener_cursor_load(REPLICATION_LISTENER).unwrap_or(0) == 0 {
        // Read the newest event id; advance the cursor to it so only events
        // after this moment are treated as live.
        let max_id = dbg
            .event_outbox_recent(1)
            .ok()
            .and_then(|rows| rows.first().map(|r| r.id))
            .unwrap_or(0);
        if max_id > 0 {
            let _ =
                dbg.listener_cursor_advance(REPLICATION_LISTENER, max_id, current_unix_seconds());
        }
    }
}

/// One drain pass: read new events, group + compact per key, route to rules,
/// act (copy/delete) under the per-rule lease, and advance the cursor to the
/// highest CONTIGUOUS fully-handled id.
async fn drain_once(
    db: &Arc<Mutex<ConfigDb>>,
    state: &Arc<AppState>,
    replication: &crate::config_sections::ReplicationConfig,
    instance_id: &str,
    now: i64,
) {
    let cursor = {
        let dbg = db.lock().await;
        dbg.listener_cursor_load(REPLICATION_LISTENER).unwrap_or(0)
    };
    let rows = {
        let dbg = db.lock().await;
        match dbg.event_outbox_since(cursor, DRAIN_BATCH) {
            Ok(r) => r,
            Err(e) => {
                warn!("event consumer: failed to read outbox: {e}");
                return;
            }
        }
    };
    if rows.is_empty() {
        return;
    }

    let lease_ttl = super::scheduler::lease_ttl_secs(replication);
    let engine = state.engine.load();

    // Compile globsets once per drain, keyed by rule name.
    let groups = group_events_by_key(&rows);

    // Track the highest contiguous id fully handled. We process keys in id
    // order of their LAST event; but the safe contiguous watermark is computed
    // over the raw rows: walk rows ascending and stop advancing at the first id
    // whose key-action failed.
    let mut failed_ids: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();

    for ((bucket, key), recs) in &groups {
        let kinds: Vec<&str> = recs.iter().map(|r| r.kind.as_str()).collect();
        let action = compact_key_events(&kinds);
        if action == KeyAction::Noop {
            continue;
        }
        let max_id_for_key = recs.iter().map(|r| r.id).max().unwrap_or(0);

        let matched = match_rules(&replication.rules, bucket, key);
        if matched.is_empty() {
            continue; // no rule cares about this key; it's still "handled"
        }

        for rule in matched {
            // Maintenance gate: the destination bucket is being rewritten in
            // place (re-encryption). Stall this key's events — the cursor
            // does not advance past them, so they replay after the job ends.
            if state.maintenance_gate.is_busy(&rule.destination.bucket) {
                debug!(
                    "event consumer: rule '{}' deferred — destination '{}' under maintenance",
                    rule.name, rule.destination.bucket
                );
                failed_ids.insert(max_id_for_key);
                continue;
            }
            // Respect the SAME per-rule lease the scheduler/reconcile uses, so
            // fast-path + reconcile + multi-instance are mutually exclusive.
            //
            // Three outcomes, NOT two (Finding 2): `Ok(true)` = we hold the
            // lease, proceed; `Ok(false)` = another worker holds it, stall this
            // key for next tick; `Err(_)` = the DB itself failed. A DB error is
            // NOT "lease busy" — collapsing it into a stall would silently pin
            // the cursor on a transient fault. Abort the whole drain (advance
            // nothing) and let the next tick retry the same window.
            let got = {
                let dbg = db.lock().await;
                let _ = dbg.replication_ensure_state(&rule.name, now);
                dbg.replication_try_acquire_lease(&rule.name, instance_id, now, lease_ttl)
            };
            match got {
                Ok(true) => {}
                Ok(false) => {
                    // Busy on another worker — leave for next tick (don't advance
                    // past this key's events).
                    failed_ids.insert(max_id_for_key);
                    continue;
                }
                Err(e) => {
                    warn!(
                        "event consumer: lease acquisition for rule '{}' failed (DB error): {e}; \
                         aborting drain, cursor held at {cursor}",
                        rule.name
                    );
                    return;
                }
            }

            let outcome = apply_action(&engine, db, rule, bucket, key, action).await;

            {
                let dbg = db.lock().await;
                let _ = dbg.replication_release_lease(&rule.name, instance_id);
            }

            if let Err(err) = outcome {
                warn!(
                    "event consumer: rule '{}' {:?} {}/{} failed: {}",
                    rule.name, action, bucket, key, err
                );
                let dbg = db.lock().await;
                let _ = dbg.replication_record_failure(
                    &rule.name,
                    crate::replication::state_store::FailureInsert {
                        run_id: None,
                        occurred_at: now,
                        source_key: key,
                        dest_key: key,
                        error_message: &err.to_string(),
                    },
                    replication.max_failures_retained,
                );
                failed_ids.insert(max_id_for_key);
            }
        }
    }

    // Advance the cursor to the highest CONTIGUOUS id with no failure (anything
    // at or past the first failed id is left for next tick → at-least-once).
    let watermark = contiguous_watermark(&rows, &failed_ids, cursor);
    if watermark > cursor {
        {
            let dbg = db.lock().await;
            let _ = dbg.listener_cursor_advance(
                REPLICATION_LISTENER,
                watermark,
                current_unix_seconds(),
            );
        }
        // Settle barrier: a drain that advanced the cursor handled real events.
        // Bump after the advance so a test polling the event-version can wait on
        // a drain instead of polling S3 / sleeping (event-driven writes no run).
        super::state_store::bump_replication_event_version();
        debug!(
            "event consumer: cursor {} -> {} ({} events drained)",
            cursor,
            watermark,
            rows.len()
        );
    }
}

/// Apply one compacted action for one (rule, key): the planner + dest HEAD
/// decide whether it actually copies/deletes (idempotency lives here, shared
/// with reconcile).
async fn apply_action(
    engine: &Arc<crate::deltaglider::DynEngine>,
    db: &Arc<Mutex<ConfigDb>>,
    rule: &ReplicationRule,
    bucket: &str,
    key: &str,
    action: KeyAction,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let dest_key = rewrite_key(&rule.source.prefix, &rule.destination.prefix, key)?;

    match action {
        KeyAction::Noop => Ok(()),
        KeyAction::Copy => {
            // Source may have been deleted after the event landed — HEAD it.
            let src_meta = match engine.head(bucket, key).await {
                Ok(m) => m,
                Err(crate::deltaglider::EngineError::NotFound(_)) => {
                    // Source gone — nothing to copy; reconcile/the delete event
                    // will handle removal. Treat as handled.
                    return Ok(());
                }
                Err(e) => return Err(Box::new(e)),
            };
            let dest_meta = engine.head(&rule.destination.bucket, &dest_key).await.ok();
            let (include_globs, exclude_globs) = compile_rule_globs(rule)?;
            let decision = should_replicate(
                key,
                &src_meta,
                dest_meta.as_ref(),
                rule.conflict,
                &include_globs,
                &exclude_globs,
            );
            match decision {
                // `should_replicate` echoes the SOURCE key in `dest_key` (it
                // never rewrites), so we always use the prefix-rewritten
                // `dest_key` computed above — same as the reconcile worker.
                Decision::Copy { .. } => {
                    let transfer = ObjectTransferRequest {
                        source_bucket: &rule.source.bucket,
                        source_key: key,
                        destination_bucket: &rule.destination.bucket,
                        destination_key: &dest_key,
                        provenance: Some(TransferProvenance {
                            metadata_key: REPLICATION_RULE_METADATA_KEY,
                            metadata_value: &rule.name,
                        }),
                        strip_user_metadata_keys: &[],
                        operation: "replication-event",
                        upload_concurrency: None,
                    };
                    let outcome = copy_object_with_retries(engine, transfer).await?;
                    // Emit ReplicationObjectCopied so the chain is observable
                    // (mirrors the reconcile worker).
                    emit_replication_copied(db, rule, key, &dest_key, outcome.bytes_copied).await;
                    Ok(())
                }
                Decision::Skip { .. } => Ok(()),
            }
        }
        KeyAction::Delete => {
            if !rule.replicate_deletes {
                return Ok(());
            }
            // Only delete a dest object WE wrote (provenance marker), mirroring
            // the reconcile delete-pass safety property.
            match engine.head(&rule.destination.bucket, &dest_key).await {
                Ok(meta) => {
                    if owned_by_rule(&meta, &rule.name) {
                        engine.delete(&rule.destination.bucket, &dest_key).await?;
                    }
                    Ok(())
                }
                // Dest absent → nothing to delete.
                Err(crate::deltaglider::EngineError::NotFound(_)) => Ok(()),
                Err(e) => Err(Box::new(e)),
            }
        }
    }
}

/// Append a `ReplicationObjectCopied` event (best-effort) so the replication
/// chain is observable downstream, identical to the reconcile worker.
async fn emit_replication_copied(
    db: &Arc<Mutex<ConfigDb>>,
    rule: &ReplicationRule,
    source_key: &str,
    dest_key: &str,
    bytes_copied: usize,
) {
    let dbg = db.lock().await;
    let _ = dbg.event_outbox_insert(&NewEvent::new(
        EventKind::ReplicationObjectCopied,
        rule.destination.bucket.as_str(),
        dest_key,
        EventSource::Replication,
        current_unix_seconds(),
        serde_json::json!({
            "rule_name": &rule.name,
            "source_bucket": &rule.source.bucket,
            "source_key": source_key,
            "destination_bucket": &rule.destination.bucket,
            "destination_key": dest_key,
            "content_length": bytes_copied,
            "trigger": "event",
        }),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_sections::{ConflictPolicy, ReplicationEndpoint, ReplicationRule};

    fn rule(name: &str, src_bucket: &str, src_prefix: &str, enabled: bool) -> ReplicationRule {
        ReplicationRule {
            name: name.to_string(),
            enabled,
            source: ReplicationEndpoint {
                bucket: src_bucket.to_string(),
                prefix: src_prefix.to_string(),
            },
            destination: ReplicationEndpoint {
                bucket: "dest".to_string(),
                prefix: String::new(),
            },
            interval: "24h".to_string(),
            batch_size: 100,
            replicate_deletes: false,
            conflict: ConflictPolicy::default(),
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
        }
    }

    // ── compact_key_events ──────────────────────────────────────────────────
    #[test]
    fn compact_empty_is_noop() {
        assert_eq!(compact_key_events(&[]), KeyAction::Noop);
    }

    #[test]
    fn compact_single_create_is_copy() {
        assert_eq!(compact_key_events(&["ObjectCreated"]), KeyAction::Copy);
    }

    #[test]
    fn compact_create_then_modify_is_copy() {
        // create + overwrite (another create) → net present → one Copy.
        assert_eq!(
            compact_key_events(&["ObjectCreated", "ObjectCreated"]),
            KeyAction::Copy
        );
    }

    #[test]
    fn compact_create_modify_delete_is_delete() {
        // create + modify + delete within the window → net absent → Delete.
        assert_eq!(
            compact_key_events(&["ObjectCreated", "ObjectCreated", "ObjectDeleted"]),
            KeyAction::Delete
        );
    }

    #[test]
    fn compact_delete_after_create_is_delete() {
        assert_eq!(
            compact_key_events(&["ObjectCreated", "ObjectDeleted"]),
            KeyAction::Delete
        );
    }

    #[test]
    fn compact_recreate_after_delete_is_copy() {
        // delete then re-create → final present → Copy.
        assert_eq!(
            compact_key_events(&["ObjectDeleted", "ObjectCreated"]),
            KeyAction::Copy
        );
    }

    #[test]
    fn compact_lifecycle_kinds() {
        assert_eq!(compact_key_events(&["LifecycleExpired"]), KeyAction::Delete);
        assert_eq!(
            compact_key_events(&["LifecycleTransitioned"]),
            KeyAction::Copy
        );
    }

    #[test]
    fn compact_unknown_terminal_kind_is_noop() {
        assert_eq!(compact_key_events(&["SomethingElse"]), KeyAction::Noop);
        // ...but a recognized kind AFTER an unknown one still decides.
        assert_eq!(
            compact_key_events(&["SomethingElse", "ObjectDeleted"]),
            KeyAction::Delete
        );
    }

    // ── is_user_object_key ──────────────────────────────────────────────────
    #[test]
    fn user_object_key_filters_internals() {
        assert!(is_user_object_key("ror/builds/x.zip"));
        assert!(is_user_object_key("a"));
        assert!(!is_user_object_key("ror/builds/")); // dir marker
        assert!(!is_user_object_key(".deltaglider/state.json"));
        assert!(!is_user_object_key("bucket/.deltaglider/x"));
        assert!(!is_user_object_key("ror/.dg/reference.bin"));
        assert!(!is_user_object_key("reference.bin"));
        assert!(!is_user_object_key("ror/libs/foo.delta"));
        // a key that merely CONTAINS "reference.bin" as a substring is fine
        assert!(is_user_object_key("ror/reference.bin.bak"));
    }

    // ── group_events_by_key ────────────────────────────────────────────────
    fn rec(id: i64, bucket: &str, key: &str, kind: &str) -> EventOutboxRecord {
        EventOutboxRecord {
            id,
            kind: kind.to_string(),
            bucket: bucket.to_string(),
            key: key.to_string(),
            source: "s3_api".to_string(),
            occurred_at: 0,
            payload: serde_json::json!({}),
            status: "pending".to_string(),
            attempts: 0,
            next_attempt_at: None,
            claimed_by: None,
            claimed_at: None,
            delivered_at: None,
            last_error: None,
            created_at: 0,
        }
    }

    #[test]
    fn group_preserves_id_order_within_key() {
        let recs = vec![
            rec(1, "b", "k1", "ObjectCreated"),
            rec(2, "b", "k2", "ObjectCreated"),
            rec(3, "b", "k1", "ObjectDeleted"),
        ];
        let groups = group_events_by_key(&recs);
        let k1 = groups.get(&("b", "k1")).unwrap();
        assert_eq!(
            k1.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(),
            vec!["ObjectCreated", "ObjectDeleted"]
        );
        // and that group compacts to Delete.
        let kinds: Vec<&str> = k1.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(compact_key_events(&kinds), KeyAction::Delete);
        assert_eq!(groups.get(&("b", "k2")).unwrap().len(), 1);
    }

    // ── match_rules ─────────────────────────────────────────────────────────
    #[test]
    fn match_rules_by_bucket_and_prefix() {
        let rules = vec![
            rule("builds", "beshu", "ror/builds/", true),
            rule("e2e", "beshu", "ror/e2e_reports/", true),
            rule("other-bucket", "scratch", "", true),
            rule("disabled", "beshu", "ror/builds/", false),
        ];
        // A builds key → only the builds rule (disabled one excluded).
        let m = match_rules(&rules, "beshu", "ror/builds/1.0/x.zip");
        assert_eq!(
            m.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["builds"]
        );
        // Wrong bucket → nothing.
        assert!(match_rules(&rules, "beshu", "private/x").is_empty());
        // A whole-bucket rule matches any key in its bucket.
        assert_eq!(
            match_rules(&rules, "scratch", "anything/deep/x")
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            vec!["other-bucket"]
        );
    }

    #[test]
    fn match_rules_prefix_boundary_not_substring() {
        let rules = vec![rule("libs", "beshu", "ror/libs/", true)];
        // `ror/libs-internal/` must NOT match the `ror/libs/` prefix (boundary).
        assert!(match_rules(&rules, "beshu", "ror/libs-internal/x").is_empty());
        assert!(!match_rules(&rules, "beshu", "ror/libs/x").is_empty());
    }

    #[test]
    fn match_rules_skips_internal_keys() {
        let rules = vec![rule("all", "beshu", "", true)];
        assert!(match_rules(&rules, "beshu", "ror/.dg/reference.bin").is_empty());
        assert!(match_rules(&rules, "beshu", "x/").is_empty()); // dir marker
        assert!(!match_rules(&rules, "beshu", "ror/real.zip").is_empty());
    }

    #[test]
    fn match_rules_multi_rule_fanout() {
        let rules = vec![
            rule("a", "beshu", "ror/", true),
            rule("b", "beshu", "ror/builds/", true),
        ];
        // A builds key is under BOTH ror/ and ror/builds/ → matches both.
        let m = match_rules(&rules, "beshu", "ror/builds/x");
        assert_eq!(m.len(), 2);
    }

    // ── EventKind ↔ liveness coupling ───────────────────────────────────────
    // GUARD: compaction matches raw `EventKind::as_str` strings, but the
    // classification flows through the EXHAUSTIVE `liveness_of_kind` match over
    // `EventKind` — so adding a variant fails to compile there until classified
    // (no silent Noop drop). This test additionally pins every variant to its
    // expected liveness so a string rename has to update both sides.
    #[test]
    fn liveness_of_kind_is_total_and_classifies_every_variant() {
        use EventKind::*;
        // Every known kind must produce a liveness verdict (never `None`); a new
        // variant without an arm here won't compile.
        for kind in [
            ObjectCreated,
            ObjectDeleted,
            ObjectCopied,
            ReplicationObjectCopied,
            LifecycleExpired,
            LifecycleTransitioned,
        ] {
            assert!(
                liveness_of_kind(kind).is_some(),
                "EventKind::{kind:?} has no liveness classification"
            );
            // The string round-trips through parse_event_kind too.
            assert_eq!(parse_event_kind(kind.as_str()), Some(kind));
        }
        // An unrecognized string is None (treated as Noop downstream).
        assert_eq!(parse_event_kind("TotallyMadeUp"), None);
    }

    #[test]
    fn every_event_kind_has_a_liveness_classification() {
        use EventKind::*;
        let cases = [
            (ObjectCreated, KeyAction::Copy),
            (ObjectCopied, KeyAction::Copy),
            (ReplicationObjectCopied, KeyAction::Copy),
            (LifecycleTransitioned, KeyAction::Copy),
            (ObjectDeleted, KeyAction::Delete),
            (LifecycleExpired, KeyAction::Delete),
        ];
        for (kind, expected) in cases {
            let s = kind.as_str();
            assert_eq!(
                compact_key_events(&[s]),
                expected,
                "EventKind::{kind:?} ({s:?}) is no longer classified as {expected:?} — \
                 update is_present_producing/is_absent_producing to match as_str"
            );
            // And it must be classified by EXACTLY one of the two predicates.
            assert_ne!(
                is_present_producing(s),
                is_absent_producing(s),
                "EventKind::{kind:?} must be present XOR absent producing"
            );
        }
    }

    // ── owned_by_rule (delete-safety lynchpin) ──────────────────────────────
    fn meta_with_marker(value: Option<&str>) -> crate::types::FileMetadata {
        let mut m = crate::types::FileMetadata::fallback(
            "k".to_string(),
            0,
            String::new(),
            chrono::Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        if let Some(v) = value {
            m.user_metadata
                .insert(REPLICATION_RULE_METADATA_KEY.to_string(), v.to_string());
        }
        m
    }

    #[test]
    fn owned_by_rule_truth_table() {
        // marker present and matches the rule → ours.
        assert!(owned_by_rule(&meta_with_marker(Some("builds")), "builds"));
        // marker present but a DIFFERENT rule → not ours (never delete).
        assert!(!owned_by_rule(&meta_with_marker(Some("e2e")), "builds"));
        // no marker at all (foreign / pre-existing object) → not ours.
        assert!(!owned_by_rule(&meta_with_marker(None), "builds"));
        // empty marker value never matches a real rule name.
        assert!(!owned_by_rule(&meta_with_marker(Some("")), "builds"));
    }

    // ── contiguous_watermark ────────────────────────────────────────────────
    fn rec_id(id: i64) -> EventOutboxRecord {
        rec(id, "b", "k", "ObjectCreated")
    }

    #[test]
    fn watermark_advances_to_last_when_nothing_failed() {
        let rows = vec![rec_id(3), rec_id(5), rec_id(8)];
        let failed = std::collections::BTreeSet::new();
        assert_eq!(contiguous_watermark(&rows, &failed, 0), 8);
    }

    #[test]
    fn watermark_stops_before_first_failed_id() {
        let rows = vec![rec_id(3), rec_id(5), rec_id(8)];
        let failed: std::collections::BTreeSet<i64> = [5].into_iter().collect();
        // Advances to 3 (the last good id BEFORE the failed 5); 8 is held back
        // even though it succeeded — at-least-once, retried next tick.
        assert_eq!(contiguous_watermark(&rows, &failed, 0), 3);
    }

    #[test]
    fn watermark_holds_cursor_when_first_row_failed() {
        let rows = vec![rec_id(3), rec_id(5)];
        let failed: std::collections::BTreeSet<i64> = [3].into_iter().collect();
        assert_eq!(contiguous_watermark(&rows, &failed, 2), 2);
    }

    #[test]
    fn watermark_empty_rows_returns_cursor() {
        let rows: Vec<EventOutboxRecord> = vec![];
        let failed = std::collections::BTreeSet::new();
        assert_eq!(contiguous_watermark(&rows, &failed, 7), 7);
    }
}
