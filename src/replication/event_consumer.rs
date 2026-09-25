// SPDX-License-Identifier: BUSL-1.1

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
use crate::coordination::{CoordinationLease, LeaseSubsystem, LocalLease};
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
/// so only one consumer drains+advances this node's cursor at a time (the
/// outbox, cursor and this lease are all in the node's own database).
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
    lease: Option<Arc<dyn CoordinationLease>>,
) -> tokio::task::JoinHandle<()> {
    let instance_id = format!("event-consumer:{}", uuid::Uuid::new_v4());
    // Per-rule leases go through the SAME lease the scheduler uses (S3 when a
    // coordination bucket is configured), so the consumer and a reconcile run
    // exclude each other across instances too. Node-local when none is given.
    let lease: Arc<dyn CoordinationLease> =
        lease.unwrap_or_else(|| Arc::new(LocalLease::new(db.clone())));
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
            // cursor. The outbox and its cursor are node-local, so this lease
            // is node-local too (SQLite): it stops two consumers in one node's
            // database (for example across a fast restart) from overlapping.
            //
            // The lease is acquired once per tick and NOT renewed mid-drain
            // (unlike the reconcile worker's heartbeat). A drain exceeding the
            // TTL can therefore let a second consumer (a fast restart) steal
            // the lease and overlap on the same cursor window. That is intentionally tolerated
            // because every action is idempotent: a re-Copy is gated by a dest
            // HEAD + `should_replicate` (a no-op when current), and a re-Delete
            // re-confirms source-absence + HEADs an already-absent dest. The cursor
            // advance is monotonic (`MAX`), so overlap costs duplicate WORK, not
            // correctness. The per-rule leases in `drain_once` are what exclude
            // the scheduler, run-now and other instances.
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

            let engine = state.engine.load_full();
            drain_once(
                &config,
                &db,
                &engine,
                &state.maintenance_gate,
                &replication,
                lease.as_ref(),
                &instance_id,
                now,
            )
            .await;

            let dbg = db.lock().await;
            let _ = dbg.replication_release_lease(CONSUMER_LEASE_KEY, &instance_id);
        }
    })
}

/// Seed the replication cursor to `MAX(event_outbox.id)` if it has no cursor yet
/// (don't replay history on first feature boot). "No cursor" is "no row", not
/// "0": the row is written even at 0 (an empty outbox). Otherwise the next tick
/// saw a 0 cursor again, took the first LIVE event for history and seeded past
/// it — that event was never replicated.
async fn seed_cursor_if_absent(db: &Arc<Mutex<ConfigDb>>) {
    let dbg = db.lock().await;
    if matches!(
        dbg.listener_cursor_load_full(REPLICATION_LISTENER),
        Ok(None)
    ) {
        let max_id = dbg.event_outbox_max_id().ok().flatten().unwrap_or(0);
        let _ = dbg.listener_cursor_advance(REPLICATION_LISTENER, max_id, current_unix_seconds());
    }
}

/// What the consumer does with a rule's events, from the rule's state row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleGate {
    /// Live rule (or no row yet): act on the events.
    Proceed,
    /// Paused: do nothing; the events count as handled.
    SkipPaused,
    /// The state read failed: never guess. Stop the drain and hold the
    /// cursor, so the events replay on the next tick.
    AbortDrain,
}

fn rule_gate<E>(state: &Result<Option<super::state_store::ReplicationState>, E>) -> RuleGate {
    match state {
        Ok(Some(st)) if st.paused => RuleGate::SkipPaused,
        Ok(_) => RuleGate::Proceed,
        Err(_) => RuleGate::AbortDrain,
    }
}

/// What the consumer may do with one rule's events during one drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleClaim {
    /// We hold the rule's lease (taken at `since`, unix seconds): act.
    Held { since: i64 },
    /// Another worker (scheduler, run-now, another instance) holds it: hold
    /// the rule's events for the next tick.
    Busy,
    /// Paused or deleted: do nothing; the events count as handled.
    Skip,
    /// The lease or state read failed: never guess. Stop the drain and hold
    /// the cursor.
    Abort,
}

/// Is the rule still configured and not paused? Read on every key, not
/// once per drain: an operator's pause or delete must stop the consumer at
/// the next key, not up to a whole drain later.
async fn live_rule_gate(
    db: &Arc<Mutex<ConfigDb>>,
    config: &crate::config::SharedConfig,
    rule_name: &str,
) -> RuleClaim {
    // The rule snapshot is a tick stale; read the LIVE config.
    let configured = config
        .read()
        .await
        .replication
        .rules
        .iter()
        .any(|r| r.name == rule_name);
    if !configured {
        return RuleClaim::Skip;
    }
    // A paused rule does nothing — copies or deletes. The events count as
    // handled: holding them would pin the cursor and stall every other rule.
    // Resume makes the rule due at once, so its reconcile run catches up. A
    // state read that FAILS is not "paused": abort.
    let state = db.lock().await.replication_load_state(rule_name);
    match rule_gate(&state) {
        RuleGate::Proceed => RuleClaim::Held { since: 0 },
        RuleGate::SkipPaused => {
            debug!("event consumer: rule '{rule_name}' is paused — skipping its events");
            RuleClaim::Skip
        }
        RuleGate::AbortDrain => {
            warn!(
                "event consumer: cannot read state of rule '{rule_name}' ({:?})",
                state.err()
            );
            RuleClaim::Abort
        }
    }
}

/// Take `rule`'s per-rule lease through the coordination lease, then gate on
/// the live config and state ([`live_rule_gate`]). Every outcome other than
/// `Held` leaves the lease released.
async fn claim_rule(
    lease: &dyn CoordinationLease,
    db: &Arc<Mutex<ConfigDb>>,
    config: &crate::config::SharedConfig,
    rule_name: &str,
    instance_id: &str,
    lease_ttl: i64,
) -> RuleClaim {
    // The CURRENT time, not the drain's start: a rule first met late in a
    // long drain must not get a lease that is already expired.
    let now = current_unix_seconds();
    match lease
        .try_acquire(
            LeaseSubsystem::Replication,
            rule_name,
            instance_id,
            now,
            lease_ttl,
        )
        .await
    {
        Ok(true) => {}
        Ok(false) => return RuleClaim::Busy,
        Err(e) => {
            // A coordination-bucket error is about THIS rule's lease: hold
            // its events for the next tick and go on with the other rules.
            warn!("event consumer: lease acquisition for rule '{rule_name}' failed: {e}");
            return RuleClaim::Busy;
        }
    }
    match live_rule_gate(db, config, rule_name).await {
        RuleClaim::Held { .. } => RuleClaim::Held { since: now },
        other => {
            let _ = lease
                .release(LeaseSubsystem::Replication, rule_name, instance_id)
                .await;
            other
        }
    }
}

/// A key the destination (or the source) can never accept as written — for
/// example a `.` or empty segment on a filesystem backend. Retrying it can
/// never succeed, so the consumer records the failure and moves on instead
/// of holding the cursor (which would stall every rule).
#[derive(Debug)]
struct PermanentKeyError(String);

impl std::fmt::Display for PermanentKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "key cannot be stored as written: {}", self.0)
    }
}

impl std::error::Error for PermanentKeyError {}

/// Box an engine error, marking the ones a retry can never fix.
fn classify_engine_error(
    e: crate::deltaglider::EngineError,
) -> Box<dyn std::error::Error + Send + Sync> {
    use crate::deltaglider::EngineError;
    match e {
        EngineError::InvalidArgument(msg)
        | EngineError::Storage(crate::storage::StorageError::InvalidKey(msg)) => {
            Box::new(PermanentKeyError(msg))
        }
        other => Box::new(other),
    }
}

/// One drain pass: read new events, group + compact per key, route to rules,
/// act (copy/delete) under the per-rule lease, and advance each rule's cursor
/// to its highest CONTIGUOUS fully-handled id. Each rule's lease is taken once
/// per drain (one S3 round-trip per rule, not per key) and released at the end.
#[allow(clippy::too_many_arguments)]
async fn drain_once(
    config: &crate::config::SharedConfig,
    db: &Arc<Mutex<ConfigDb>>,
    engine: &Arc<crate::deltaglider::DynEngine>,
    gate: &crate::maintenance::gate::MaintenanceGate,
    replication: &crate::config_sections::ReplicationConfig,
    lease: &dyn CoordinationLease,
    instance_id: &str,
    now: i64,
) {
    let mut claims: std::collections::HashMap<String, RuleClaim> = std::collections::HashMap::new();
    drain_rules(
        config,
        db,
        engine,
        gate,
        replication,
        lease,
        &mut claims,
        instance_id,
        now,
    )
    .await;
    for (rule, claim) in &claims {
        if matches!(claim, RuleClaim::Held { .. }) {
            let _ = lease
                .release(LeaseSubsystem::Replication, rule, instance_id)
                .await;
        }
    }
}

/// A failing key holds its rule's cursor for this many drains, then the
/// consumer gives up on it (failure row + warning) so the rule moves on. The
/// reconcile run still retries the object (poison-skip ledger).
pub(crate) const MAX_EVENT_KEY_ATTEMPTS: u32 = 5;

/// The listener-cursor row of one rule. Each rule has its own cursor, so one
/// rule that holds its events (busy lease, maintenance, a failing key) never
/// holds another rule's.
pub(crate) fn rule_listener(rule: &str) -> String {
    format!("{REPLICATION_LISTENER}:{rule}")
}

/// Pure: a rule's new cursor. Only rows past its own cursor count; the
/// first failed id stops the advance (at-least-once).
fn rule_watermark(
    rows: &[EventOutboxRecord],
    failed_ids: Option<&std::collections::BTreeSet<i64>>,
    cursor: i64,
) -> i64 {
    let empty = std::collections::BTreeSet::new();
    let start = rows.partition_point(|r| r.id <= cursor);
    contiguous_watermark(&rows[start..], failed_ids.unwrap_or(&empty), cursor)
}

/// Pure: the global `replication` cursor = the slowest rule's cursor (it is
/// the read floor and the lag the jobs API shows). No enabled rule: every
/// row is handled.
fn global_watermark(rule_cursors: &BTreeMap<String, i64>, global: i64, rows_max: i64) -> i64 {
    rule_cursors
        .values()
        .copied()
        .min()
        .unwrap_or(rows_max)
        .max(global)
}

/// Load each enabled rule's cursor. A rule without a row starts at the global
/// cursor: on upgrade that is exactly where the single cursor stood, so no
/// event is skipped. Rows of rules no longer enabled are dropped (they would
/// pin the prune floor).
fn load_rule_cursors(
    dbg: &ConfigDb,
    rule_names: &[&str],
    global: i64,
    now: i64,
) -> Result<BTreeMap<String, i64>, crate::config_db::ConfigDbError> {
    let mut cursors = BTreeMap::new();
    for name in rule_names {
        let listener = rule_listener(name);
        let cursor = match dbg.listener_cursor_load_full(&listener)? {
            Some(row) => row.last_event_id,
            None => {
                dbg.listener_cursor_advance(&listener, global, now)?;
                global
            }
        };
        cursors.insert(name.to_string(), cursor);
    }
    let wanted: std::collections::HashSet<String> =
        rule_names.iter().map(|n| rule_listener(n)).collect();
    for name in dbg.listener_cursor_names_with_prefix(&format!("{REPLICATION_LISTENER}:"))? {
        if !wanted.contains(&name) {
            dbg.listener_cursor_delete(&name)?;
        }
    }
    Ok(cursors)
}

#[allow(clippy::too_many_arguments)]
async fn drain_rules(
    config: &crate::config::SharedConfig,
    db: &Arc<Mutex<ConfigDb>>,
    engine: &Arc<crate::deltaglider::DynEngine>,
    gate: &crate::maintenance::gate::MaintenanceGate,
    replication: &crate::config_sections::ReplicationConfig,
    lease: &dyn CoordinationLease,
    claims: &mut std::collections::HashMap<String, RuleClaim>,
    instance_id: &str,
    now: i64,
) {
    let mut rule_names: Vec<&str> = replication
        .rules
        .iter()
        .filter(|r| r.enabled)
        .map(|r| r.name.as_str())
        .collect();
    rule_names.sort_unstable();
    rule_names.dedup();
    let (global, mut cursors) = {
        let dbg = db.lock().await;
        let global = match dbg.listener_cursor_load_full(REPLICATION_LISTENER) {
            Ok(Some(row)) => {
                // Re-assert the cursor (MAX-upsert keeps last_event_id). NOTE:
                // updated_at only moves on a REAL advance (H65) — a zero-advance
                // tick here is intentionally a no-op for updated_at, so a consumer
                // WEDGED on a poison key ages out of the active-floor and stops
                // pinning the prune floor forever. A caught-up (idle) consumer
                // aging out is safe: it already consumed everything below its
                // cursor, and the operator purge floor still guards manual purges.
                let _ = dbg.listener_cursor_advance(REPLICATION_LISTENER, row.last_event_id, now);
                row.last_event_id
            }
            _ => 0,
        };
        match load_rule_cursors(&dbg, &rule_names, global, now) {
            Ok(c) => (global, c),
            Err(e) => {
                warn!("event consumer: failed to load rule cursors: {e}");
                return;
            }
        }
    };
    let lease_ttl = super::scheduler::lease_ttl_secs(replication);
    let mut advanced = false;

    // No enabled rule: every row is handled; the global cursor catches up.
    if rule_names.is_empty() {
        let dbg = db.lock().await;
        let rows_max = match dbg.event_outbox_since(global, DRAIN_BATCH) {
            Ok(r) => r.last().map(|r| r.id).unwrap_or(global),
            Err(e) => {
                warn!("event consumer: failed to read outbox: {e}");
                return;
            }
        };
        let next_global = global_watermark(&cursors, global, rows_max);
        if next_global > global {
            let _ = dbg.listener_cursor_advance(REPLICATION_LISTENER, next_global, now);
            drop(dbg);
            super::state_store::bump_replication_event_version();
        }
        return;
    }

    // Each rule reads its batch from its OWN cursor. One shared read from the
    // slowest cursor let a rule held a whole batch behind (busy lease, a
    // failing key) fill every batch with rows the other rules had already
    // handled: they stalled too, one batch later.
    for rule_name in &rule_names {
        let rule_cursor = cursors.get(*rule_name).copied().unwrap_or(global);
        let rows = {
            let dbg = db.lock().await;
            match dbg.event_outbox_since(rule_cursor, DRAIN_BATCH) {
                Ok(r) => r,
                Err(e) => {
                    warn!("event consumer: failed to read outbox: {e}");
                    return;
                }
            }
        };
        if rows.is_empty() {
            continue;
        }
        // The ids whose key-action failed (the rule's cursor stops there).
        let mut failed_ids: BTreeMap<String, std::collections::BTreeSet<i64>> = BTreeMap::new();
        let groups = group_events_by_key(&rows);
        if drain_rule_rows(
            config,
            db,
            engine,
            gate,
            replication,
            lease,
            claims,
            instance_id,
            now,
            lease_ttl,
            global,
            rule_name,
            rule_cursor,
            &groups,
            &mut failed_ids,
        )
        .await
        .is_break()
        {
            warn!("event consumer: aborting drain, cursors held");
            break;
        }
        // Advance the rule's cursor to its highest CONTIGUOUS handled id
        // (anything at or past its first failed id is left for next tick).
        let next = rule_watermark(&rows, failed_ids.get(*rule_name), rule_cursor);
        if next > rule_cursor {
            let dbg = db.lock().await;
            let _ = dbg.listener_cursor_advance(
                &rule_listener(rule_name),
                next,
                current_unix_seconds(),
            );
            cursors.insert(rule_name.to_string(), next);
            advanced = true;
        }
    }

    {
        let dbg = db.lock().await;
        let next_global = global_watermark(&cursors, global, global);
        if next_global > global {
            let _ = dbg.listener_cursor_advance(
                REPLICATION_LISTENER,
                next_global,
                current_unix_seconds(),
            );
            advanced = true;
        }
        debug!(
            "event consumer: global cursor {} -> {}",
            global, next_global
        );
    }
    if advanced {
        // Settle barrier: a drain that advanced a cursor handled real events.
        // Bump after the advance so a test polling the event-version can wait on
        // a drain instead of polling S3 / sleeping (event-driven writes no run).
        super::state_store::bump_replication_event_version();
    }
}

/// One rule's pass over its own batch (`groups`, already past its cursor).
/// `Break` = abort the drain (a lease or state read failed): hold the cursor.
#[allow(clippy::too_many_arguments)]
async fn drain_rule_rows(
    config: &crate::config::SharedConfig,
    db: &Arc<Mutex<ConfigDb>>,
    engine: &Arc<crate::deltaglider::DynEngine>,
    gate: &crate::maintenance::gate::MaintenanceGate,
    replication: &crate::config_sections::ReplicationConfig,
    lease: &dyn CoordinationLease,
    claims: &mut std::collections::HashMap<String, RuleClaim>,
    instance_id: &str,
    now: i64,
    lease_ttl: i64,
    global: i64,
    rule_name: &str,
    rule_cursor: i64,
    groups: &BTreeMap<(&str, &str), Vec<&EventOutboxRecord>>,
    failed_ids: &mut BTreeMap<String, std::collections::BTreeSet<i64>>,
) -> std::ops::ControlFlow<()> {
    for ((bucket, key), recs) in groups {
        let (bucket, key) = (*bucket, *key);
        let matched = match_rules(&replication.rules, bucket, key);
        for rule in matched.into_iter().filter(|r| r.name == rule_name) {
            let sub: Vec<&EventOutboxRecord> = recs
                .iter()
                .copied()
                .filter(|r| r.id > rule_cursor)
                .collect();
            let kinds: Vec<&str> = sub.iter().map(|r| r.kind.as_str()).collect();
            let action = compact_key_events(&kinds);
            if action == KeyAction::Noop {
                continue;
            }
            let max_id_for_key = sub.iter().map(|r| r.id).max().unwrap_or(0);
            let mut hold = || {
                failed_ids
                    .entry(rule.name.clone())
                    .or_default()
                    .insert(max_id_for_key);
            };
            // Maintenance gate: the destination bucket is being rewritten in
            // place (re-encryption). Stall this key's events — the cursor
            // does not advance past them, so they replay after the job ends.
            if gate.is_busy(&rule.destination.bucket) {
                debug!(
                    "event consumer: rule '{}' deferred — destination '{}' under maintenance",
                    rule.name, rule.destination.bucket
                );
                hold();
                continue;
            }
            // The per-rule lease is the one the scheduler, run-now and other
            // instances take (the coordination lease chosen at startup), so
            // they and this consumer exclude each other.
            let claim = match claims.get(&rule.name) {
                Some(RuleClaim::Held { since }) => {
                    // Pause/delete since the claim: stop at this key.
                    let since = *since;
                    match live_rule_gate(db, config, &rule.name).await {
                        RuleClaim::Held { .. } => RuleClaim::Held { since },
                        other => {
                            let _ = lease
                                .release(LeaseSubsystem::Replication, &rule.name, instance_id)
                                .await;
                            claims.insert(rule.name.clone(), other);
                            other
                        }
                    }
                }
                Some(claim) => *claim,
                None => {
                    let claim =
                        claim_rule(lease, db, config, &rule.name, instance_id, lease_ttl).await;
                    claims.insert(rule.name.clone(), claim);
                    claim
                }
            };
            match claim {
                RuleClaim::Held { since } => {
                    // A long drain must not outlive the lease: renew at half
                    // the TTL, and stop acting for the rule if it was lost.
                    let t = current_unix_seconds();
                    if t - since >= lease_ttl / 2 {
                        let renewed = lease
                            .renew(
                                LeaseSubsystem::Replication,
                                &rule.name,
                                instance_id,
                                t,
                                lease_ttl,
                            )
                            .await;
                        match renewed {
                            Ok(true) => {
                                claims.insert(rule.name.clone(), RuleClaim::Held { since: t });
                            }
                            lost_or_unknown => {
                                // Ok(false): lost, nothing to release. Err: the
                                // lease may still be ours — release it, so it
                                // does not block the rule for a whole TTL.
                                if lost_or_unknown.is_err() {
                                    let _ = lease
                                        .release(
                                            LeaseSubsystem::Replication,
                                            &rule.name,
                                            instance_id,
                                        )
                                        .await;
                                }
                                claims.insert(rule.name.clone(), RuleClaim::Busy);
                                hold();
                                continue;
                            }
                        }
                    }
                }
                RuleClaim::Busy => {
                    // Busy on another worker — leave for next tick (holds only
                    // this rule's cursor).
                    hold();
                    continue;
                }
                RuleClaim::Skip => continue,
                RuleClaim::Abort => return std::ops::ControlFlow::Break(()),
            }

            let outcome = apply_action(engine, db, rule, bucket, key, action).await;

            let dbg = db.lock().await;
            // Mid-drain heartbeat: a long drain (up to 500 engine copies)
            // must keep pinning the background pruner's staleness floor.
            let _ =
                dbg.listener_cursor_advance(REPLICATION_LISTENER, global, current_unix_seconds());

            match outcome {
                Ok(()) => {
                    let _ = dbg.replication_clear_object_failure(&rule.name, key);
                }
                Err(err) => {
                    warn!(
                        "event consumer: rule '{}' {:?} {}/{} failed: {}",
                        rule.name, action, bucket, key, err
                    );
                    // The failure ring has a foreign key to the state row; a
                    // rule the scheduler never ran has none yet.
                    let _ = dbg.replication_ensure_state(&rule.name, now);
                    let record = |msg: &str| {
                        let _ = dbg.replication_record_failure(
                            &rule.name,
                            crate::replication::state_store::FailureInsert {
                                run_id: None,
                                occurred_at: now,
                                source_key: key,
                                dest_key: key,
                                error_message: msg,
                            },
                            replication.max_failures_retained,
                        );
                    };
                    record(&err.to_string());
                    // A permanent key error is recorded and counts as handled:
                    // holding it would stall the rule, forever.
                    if err.downcast_ref::<PermanentKeyError>().is_some() {
                        continue;
                    }
                    // A destination-wide or transient failure (bucket gone,
                    // quota, throttle, 5xx, timeout) is not about THIS key:
                    // it never counts toward giving up, or a short outage
                    // gave up on every key and dropped its events.
                    let signal = crate::transfer::error_signal(
                        &err.to_string(),
                        &[key, bucket, &rule.destination.bucket],
                    );
                    if !failure_counts_toward_give_up(&signal) {
                        hold();
                        continue;
                    }
                    let attempts = dbg
                        .replication_record_object_failure(&rule.name, key, &err.to_string(), now)
                        .unwrap_or(0);
                    if key_attempts_exhausted(attempts) {
                        warn!(
                            "event consumer: rule '{}' gives up on {}/{} after {} failed \
                             attempts; the reconcile run retries it",
                            rule.name, bucket, key, attempts
                        );
                        record(&format!(
                            "event-driven replication gave up after {attempts} attempts; \
                             the next reconcile run retries the object"
                        ));
                    } else {
                        hold();
                    }
                }
            }
        }
    }

    std::ops::ControlFlow::Continue(())
}

/// Pure: may this failure (its text with user names removed) count toward
/// giving up on the key? Only a key-specific fault may. Destination-wide and
/// transient faults hold the key until they clear.
fn failure_counts_toward_give_up(signal: &str) -> bool {
    !(super::worker::is_destination_fatal(signal)
        || super::worker::is_backend_throttled(signal)
        || crate::transfer::is_transient_copy_error(signal)
        || signal.to_ascii_lowercase().contains("overloaded"))
}

/// Pure: has a key used up its attempts?
fn key_attempts_exhausted(consecutive_failures: u32) -> bool {
    consecutive_failures >= MAX_EVENT_KEY_ATTEMPTS
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
                Err(e) => return Err(classify_engine_error(e)),
            };
            let dest_meta = match engine.head(&rule.destination.bucket, &dest_key).await {
                Ok(meta) => Some(meta),
                Err(crate::deltaglider::EngineError::NotFound(_)) => None,
                // The destination can never hold this key (for example a `.`
                // segment on a filesystem destination): do not try the copy.
                Err(e @ crate::deltaglider::EngineError::InvalidArgument(_))
                | Err(
                    e @ crate::deltaglider::EngineError::Storage(
                        crate::storage::StorageError::InvalidKey(_),
                    ),
                ) => return Err(classify_engine_error(e)),
                Err(_) => None,
            };
            let (include_globs, exclude_globs) = compile_rule_globs(rule)?;
            let decision = should_replicate(
                key,
                &src_meta,
                dest_meta.as_ref(),
                rule.conflict,
                rule.strict_content_diff,
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
                    emit_replication_copied(db, rule, key, &dest_key, outcome.content_length())
                        .await;
                    Ok(())
                }
                Decision::Skip { .. } => Ok(()),
            }
        }
        KeyAction::Delete => {
            if !rule.replicate_deletes {
                return Ok(());
            }
            // Faithful mirror (matches the reconcile worker's execute_delete): a
            // source-absent dest key is removed regardless of provenance — the
            // destination is dedicated to the rule. The delete EVENT is the
            // source-absence signal, but re-confirm it (a re-create may have
            // landed after the event) so we never delete a dest whose source
            // came back. Delete ONLY on a confirmed source NoSuchKey.
            match engine.head(bucket, key).await {
                // Source reappeared → do NOT delete; the copy path will re-sync.
                Ok(_) => Ok(()),
                Err(crate::deltaglider::EngineError::NotFound(_)) => {
                    match engine.delete(&rule.destination.bucket, &dest_key).await {
                        Ok(_) => Ok(()),
                        // Dest already gone → nothing to delete.
                        Err(crate::deltaglider::EngineError::NotFound(_)) => Ok(()),
                        Err(e) => Err(classify_engine_error(e)),
                    }
                }
                Err(e) => Err(classify_engine_error(e)),
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
    content_length: u64,
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
            "content_length": content_length,
            "trigger": "event",
        }),
    ));
}

#[cfg(test)]
mod rule_gate_tests {
    use super::*;
    use crate::replication::state_store::ReplicationState;

    fn state(paused: bool) -> ReplicationState {
        ReplicationState {
            rule_name: "r".into(),
            last_run_at: None,
            next_due_at: 0,
            last_status: String::new(),
            objects_copied_lifetime: 0,
            bytes_copied_lifetime: 0,
            paused,
            continuation_token: None,
            leader_instance_id: None,
            leader_expires_at: None,
        }
    }

    #[test]
    fn rule_gate_truth_table() {
        assert_eq!(rule_gate::<()>(&Ok(Some(state(false)))), RuleGate::Proceed);
        assert_eq!(rule_gate::<()>(&Ok(None)), RuleGate::Proceed);
        assert_eq!(
            rule_gate::<()>(&Ok(Some(state(true)))),
            RuleGate::SkipPaused
        );
        assert_eq!(rule_gate::<&str>(&Err("db locked")), RuleGate::AbortDrain);
    }
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
            strict_content_diff: false,
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

#[cfg(test)]
mod classify_tests {
    use super::*;
    use crate::deltaglider::EngineError;

    /// Keys a destination can never accept are permanent (the cursor moves
    /// on); everything else is retried.
    #[test]
    fn invalid_keys_are_permanent_other_errors_are_not() {
        let permanent = |e: EngineError| classify_engine_error(e).is::<PermanentKeyError>();
        assert!(permanent(EngineError::InvalidArgument("x".into())));
        assert!(permanent(EngineError::Storage(
            crate::storage::StorageError::InvalidKey("x".into())
        )));
        assert!(!permanent(EngineError::Storage(
            crate::storage::StorageError::Other("x".into())
        )));
        assert!(!permanent(EngineError::NotFound("k".into())));
    }
}

#[cfg(test)]
mod claim_rule_tests {
    use super::*;
    use crate::config::Config;
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;

    /// A lease another worker already holds for rule "r"; records releases.
    struct HeldElsewhere {
        released: StdMutex<Vec<String>>,
    }

    #[async_trait]
    impl CoordinationLease for HeldElsewhere {
        async fn try_acquire(
            &self,
            _: LeaseSubsystem,
            rule: &str,
            _: &str,
            _: i64,
            _: i64,
        ) -> Result<bool, String> {
            Ok(rule != "r")
        }
        async fn renew(
            &self,
            _: LeaseSubsystem,
            _: &str,
            _: &str,
            _: i64,
            _: i64,
        ) -> Result<bool, String> {
            Ok(false)
        }
        async fn release(&self, _: LeaseSubsystem, rule: &str, _: &str) -> Result<(), String> {
            self.released.lock().unwrap().push(rule.to_string());
            Ok(())
        }
        async fn is_held(&self, _: LeaseSubsystem, rule: &str, _: i64) -> Result<bool, String> {
            Ok(rule == "r")
        }
    }

    fn setup(rules: &[&str]) -> (Arc<Mutex<ConfigDb>>, crate::config::SharedConfig) {
        let db = Arc::new(Mutex::new(ConfigDb::in_memory("testpass").unwrap()));
        let mut cfg = Config::default();
        cfg.replication.rules = rules.iter().map(|n| tests_rule(n)).collect();
        (db, Arc::new(tokio::sync::RwLock::new(cfg)))
    }

    fn tests_rule(name: &str) -> ReplicationRule {
        serde_yaml::from_str(&format!(
            "name: {name}\nsource: {{bucket: src}}\ndestination: {{bucket: dest}}\n"
        ))
        .unwrap()
    }

    /// The consumer defers (holds the rule's events) when the coordination
    /// lease — the one the scheduler takes — is held by another worker.
    #[tokio::test]
    async fn consumer_defers_to_the_coordination_lease_holder() {
        let (db, config) = setup(&["r"]);
        let lease = HeldElsewhere {
            released: StdMutex::new(Vec::new()),
        };
        let claim = claim_rule(&lease, &db, &config, "r", "consumer", 60).await;
        assert_eq!(claim, RuleClaim::Busy);
        assert!(
            lease.released.lock().unwrap().is_empty(),
            "released a lease it never held"
        );
    }

    #[tokio::test]
    async fn claim_is_held_for_a_live_rule_and_released_for_paused_or_deleted() {
        let (db, config) = setup(&["live", "paused"]);
        db.lock()
            .await
            .replication_ensure_state("paused", 0)
            .unwrap();
        db.lock()
            .await
            .replication_set_paused("paused", true)
            .unwrap();
        let lease = LocalLease::new(db.clone());

        let claim = claim_rule(&lease, &db, &config, "live", "c", 60).await;
        assert!(matches!(claim, RuleClaim::Held { .. }), "{claim:?}");
        // Held: a rival cannot take it.
        assert!(!lease
            .try_acquire(LeaseSubsystem::Replication, "live", "rival", 110, 60)
            .await
            .unwrap());

        for name in ["paused", "deleted"] {
            let claim = claim_rule(&lease, &db, &config, name, "c", 60).await;
            assert_eq!(claim, RuleClaim::Skip, "{name}");
            // Released: a rival can take it at once.
            assert!(
                lease
                    .try_acquire(LeaseSubsystem::Replication, name, "rival", 101, 60)
                    .await
                    .unwrap(),
                "{name}: lease left held"
            );
        }
    }
}

#[cfg(test)]
mod seed_tests {
    use super::*;

    fn put_event(db: &ConfigDb, key: &str) -> i64 {
        db.event_outbox_insert(&NewEvent::new(
            EventKind::ObjectCreated,
            "b",
            key,
            EventSource::S3Api,
            1,
            serde_json::json!({}),
        ))
        .unwrap()
    }

    /// The seed runs on every enabled tick. With an empty outbox it wrote no
    /// cursor row, so the next tick took the first live event for history,
    /// seeded past it, and that event was never replicated.
    #[tokio::test]
    async fn seed_does_not_swallow_the_first_live_event() {
        let db = Arc::new(Mutex::new(ConfigDb::in_memory("pw").unwrap()));
        seed_cursor_if_absent(&db).await; // first enabled tick, empty outbox
        let first = put_event(&*db.lock().await, "k1");
        seed_cursor_if_absent(&db).await; // next tick, before any drain
        let cursor = db
            .lock()
            .await
            .listener_cursor_load(REPLICATION_LISTENER)
            .unwrap();
        assert!(
            cursor < first,
            "event {first} was seeded past (cursor {cursor})"
        );
    }

    /// History from before the enable is still skipped once.
    #[tokio::test]
    async fn seed_skips_history_once() {
        let db = Arc::new(Mutex::new(ConfigDb::in_memory("pw").unwrap()));
        let old = put_event(&*db.lock().await, "old");
        seed_cursor_if_absent(&db).await;
        let cursor = db
            .lock()
            .await
            .listener_cursor_load(REPLICATION_LISTENER)
            .unwrap();
        assert_eq!(cursor, old);
    }
}

#[cfg(test)]
mod per_rule_cursor_tests {
    use super::*;
    use crate::config::Config;
    use crate::deltaglider::{DeltaGliderEngine, DynEngine};
    use crate::storage::{FilesystemBackend, StorageBackend};

    /// Lease that another worker holds for one rule; free for the rest.
    struct BusyFor(&'static str);

    #[async_trait::async_trait]
    impl CoordinationLease for BusyFor {
        async fn try_acquire(
            &self,
            _: LeaseSubsystem,
            rule: &str,
            _: &str,
            _: i64,
            _: i64,
        ) -> Result<bool, String> {
            Ok(rule != self.0)
        }
        async fn renew(
            &self,
            _: LeaseSubsystem,
            _: &str,
            _: &str,
            _: i64,
            _: i64,
        ) -> Result<bool, String> {
            Ok(true)
        }
        async fn release(&self, _: LeaseSubsystem, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn is_held(&self, _: LeaseSubsystem, rule: &str, _: i64) -> Result<bool, String> {
            Ok(rule == self.0)
        }
    }

    fn rule(name: &str, dst: &str) -> ReplicationRule {
        serde_yaml::from_str(&format!(
            "name: {name}\nenabled: true\nsource: {{bucket: src}}\ndestination: {{bucket: {dst}}}\n"
        ))
        .unwrap()
    }

    async fn fixture(
        rules: Vec<ReplicationRule>,
    ) -> (
        tempfile::TempDir,
        Arc<Mutex<ConfigDb>>,
        crate::config::SharedConfig,
        Arc<DynEngine>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let backend: Box<dyn StorageBackend> = Box::new(
            FilesystemBackend::new(dir.path().to_path_buf())
                .await
                .unwrap(),
        );
        let engine: Arc<DynEngine> = Arc::new(DeltaGliderEngine::new_with_backend(
            Arc::new(backend),
            &Config::default(),
            None,
        ));
        for b in ["src", "dst-a", "dst-b"] {
            engine.create_bucket(b).await.unwrap();
        }
        let mut cfg = Config::default();
        cfg.replication.enabled = true;
        cfg.replication.rules = rules;
        let db = Arc::new(Mutex::new(ConfigDb::in_memory("pw").unwrap()));
        (dir, db, Arc::new(tokio::sync::RwLock::new(cfg)), engine)
    }

    async fn put(db: &Arc<Mutex<ConfigDb>>, engine: &DynEngine, key: &str) -> i64 {
        put_bytes(db, engine, key, b"data").await
    }

    async fn put_bytes(
        db: &Arc<Mutex<ConfigDb>>,
        engine: &DynEngine,
        key: &str,
        body: &[u8],
    ) -> i64 {
        engine
            .store("src", key, body, None, Default::default())
            .await
            .unwrap();
        db.lock()
            .await
            .event_outbox_insert(&NewEvent::new(
                EventKind::ObjectCreated,
                "src",
                key,
                EventSource::S3Api,
                1,
                serde_json::json!({}),
            ))
            .unwrap()
    }

    async fn drain(
        config: &crate::config::SharedConfig,
        db: &Arc<Mutex<ConfigDb>>,
        engine: &Arc<DynEngine>,
        lease: &dyn CoordinationLease,
    ) {
        let replication = config.read().await.replication.clone();
        let gate = crate::maintenance::gate::MaintenanceGate::default();
        drain_once(config, db, engine, &gate, &replication, lease, "c", 100).await;
    }

    async fn cursor(db: &Arc<Mutex<ConfigDb>>, listener: &str) -> i64 {
        db.lock().await.listener_cursor_load(listener).unwrap()
    }

    /// Rule "a" is busy on another worker. Before: one global cursor, so rule
    /// "b" re-read (and waited on) every event "a" held. Now "b" moves on and
    /// only "a" holds; the global cursor is the slowest rule.
    #[tokio::test]
    async fn a_busy_rule_does_not_hold_another_rules_cursor() {
        let (_d, db, config, engine) = fixture(vec![rule("a", "dst-a"), rule("b", "dst-b")]).await;
        let id = put(&db, &engine, "k.bin").await;
        drain(&config, &db, &engine, &BusyFor("a")).await;
        assert!(engine.head("dst-b", "k.bin").await.is_ok(), "b copied");
        assert!(engine.head("dst-a", "k.bin").await.is_err(), "a was busy");
        assert_eq!(cursor(&db, &rule_listener("b")).await, id);
        assert_eq!(cursor(&db, &rule_listener("a")).await, 0);
        assert_eq!(cursor(&db, REPLICATION_LISTENER).await, 0);

        // "a" frees up: it catches up from its own cursor.
        drain(&config, &db, &engine, &BusyFor("none")).await;
        assert!(engine.head("dst-a", "k.bin").await.is_ok());
        // (>=: b's copy appended a ReplicationObjectCopied event, which no
        // rule matches, so it counts as handled.)
        assert!(cursor(&db, &rule_listener("a")).await >= id);
        assert!(cursor(&db, REPLICATION_LISTENER).await >= id);
    }

    /// Upgrade: a rule without a cursor row starts at the global cursor, so
    /// no event is skipped or replayed from zero; a removed rule's row goes.
    #[tokio::test]
    async fn rule_cursors_seed_from_the_global_cursor() {
        let db = ConfigDb::in_memory("pw").unwrap();
        db.listener_cursor_advance(&rule_listener("gone"), 3, 1)
            .unwrap();
        let cursors = load_rule_cursors(&db, &["r"], 42, 1).unwrap();
        assert_eq!(cursors.get("r"), Some(&42));
        assert_eq!(db.listener_cursor_load(&rule_listener("r")).unwrap(), 42);
        assert!(db
            .listener_cursor_load_full(&rule_listener("gone"))
            .unwrap()
            .is_none());
    }

    /// A key that keeps failing holds its rule's cursor for
    /// MAX_EVENT_KEY_ATTEMPTS drains, then the rule gives up on it (with a
    /// failure row) and moves on. The fault must be about the KEY (here: its
    /// delta reference is gone); a destination-wide fault never gives up.
    #[tokio::test]
    async fn a_failing_key_stops_holding_the_cursor_after_n_attempts() {
        let (d, db, config, engine) = fixture(vec![rule("a", "dst-a")]).await;
        let body: Vec<u8> = (0..65_536u32).map(|n| (n % 7) as u8).collect();
        let id = put_bytes(&db, &engine, "k.zip", &body).await;
        let mut refs = 0;
        for entry in walkdir::WalkDir::new(d.path()).into_iter().flatten() {
            if entry.file_name() == "reference.bin" {
                std::fs::remove_file(entry.path()).unwrap();
                refs += 1;
            }
        }
        assert_eq!(
            refs, 1,
            "the delta object's reference must exist to break it"
        );
        let lease = BusyFor("none");
        for attempt in 1..MAX_EVENT_KEY_ATTEMPTS {
            drain(&config, &db, &engine, &lease).await;
            assert_eq!(
                cursor(&db, &rule_listener("a")).await,
                0,
                "attempt {attempt}: the cursor must hold while retries remain"
            );
        }
        drain(&config, &db, &engine, &lease).await;
        assert_eq!(
            cursor(&db, &rule_listener("a")).await,
            id,
            "gave up, moved on"
        );
        let failures = db
            .lock()
            .await
            .replication_recent_failures("a", 50)
            .unwrap();
        assert!(
            failures.iter().any(|f| f.error_message.contains("gave up")),
            "{failures:?}"
        );
    }

    #[test]
    fn rule_watermark_ignores_rows_at_or_below_the_cursor() {
        let rows: Vec<EventOutboxRecord> = [1, 2, 3, 4]
            .iter()
            .map(|id| EventOutboxRecord {
                id: *id,
                ..rows_template()
            })
            .collect();
        let failed = std::collections::BTreeSet::from([4]);
        assert_eq!(rule_watermark(&rows, Some(&failed), 2), 3);
        assert_eq!(rule_watermark(&rows, None, 4), 4);
        assert_eq!(
            rule_watermark(&rows, Some(&std::collections::BTreeSet::from([2])), 2),
            4
        );
    }

    fn rows_template() -> EventOutboxRecord {
        let db = ConfigDb::in_memory("pw").unwrap();
        db.event_outbox_insert(&NewEvent::new(
            EventKind::ObjectCreated,
            "b",
            "k",
            EventSource::S3Api,
            1,
            serde_json::json!({}),
        ))
        .unwrap();
        db.event_outbox_since(0, 1).unwrap().remove(0)
    }

    #[test]
    fn only_key_specific_failures_count_toward_give_up() {
        for wide in [
            "Storage error: Bucket not found: <name>",
            "NoSuchBucket",
            "QuotaExceeded",
            "Backend throttled: SlowDown",
            "S3 error: service unavailable (status=503)",
            "operation timed out",
            "Service overloaded: all delta codec slots busy",
        ] {
            assert!(!failure_counts_toward_give_up(wide), "{wide}");
        }
        for key_fault in [
            "Missing reference for deltaspace: x",
            "Checksum mismatch for <name>: expected a, got b",
        ] {
            assert!(failure_counts_toward_give_up(key_fault), "{key_fault}");
        }
    }

    #[test]
    fn global_watermark_is_the_slowest_rule() {
        let c = BTreeMap::from([("a".to_string(), 5), ("b".to_string(), 9)]);
        assert_eq!(global_watermark(&c, 3, 12), 5);
        assert_eq!(global_watermark(&BTreeMap::new(), 3, 12), 12);
        assert_eq!(global_watermark(&c, 7, 12), 7, "never moves back");
    }

    // ── review second pass (failing tests for findings) ──────────────────

    /// Review-2: the drain reads DRAIN_BATCH rows from the SLOWEST rule's
    /// cursor. Once a busy rule is one batch behind, every drain re-reads
    /// the same rows, all at or below the other rule's cursor, so the other
    /// rule stalls again: the head-of-line block, one batch later.
    #[tokio::test]
    async fn review2_a_busy_rule_does_not_stall_another_rule_past_one_batch() {
        let (_d, db, config, engine) = fixture(vec![rule("a", "dst-a"), rule("b", "dst-b")]).await;
        for i in 0..DRAIN_BATCH {
            put(&db, &engine, &format!("k{i}.bin")).await;
        }
        drain(&config, &db, &engine, &BusyFor("a")).await;
        let late = put(&db, &engine, "late.bin").await;
        for _ in 0..3 {
            drain(&config, &db, &engine, &BusyFor("a")).await;
        }
        assert!(
            engine.head("dst-b", "late.bin").await.is_ok(),
            "rule b waits on busy rule a"
        );
        assert!(cursor(&db, &rule_listener("b")).await >= late);
    }

    /// Review-2: a destination outage makes EVERY key fail together, so every
    /// key reaches MAX_EVENT_KEY_ATTEMPTS (about 2.5 min at the 30 s tick) and
    /// the rule gives up on all of them. Once the outage ends, nothing copies
    /// those events until the next reconcile run (default interval 24 h).
    #[tokio::test]
    async fn review2_a_short_destination_outage_does_not_drop_events() {
        let (_d, db, config, engine) = fixture(vec![rule("a", "dst-late")]).await;
        put(&db, &engine, "k.bin").await;
        let lease = BusyFor("none");
        for _ in 0..MAX_EVENT_KEY_ATTEMPTS {
            drain(&config, &db, &engine, &lease).await;
        }
        engine.create_bucket("dst-late").await.unwrap(); // the outage ends
        drain(&config, &db, &engine, &lease).await;
        assert!(
            engine.head("dst-late", "k.bin").await.is_ok(),
            "the event was dropped: only the 24 h reconcile copies it now"
        );
    }
}
