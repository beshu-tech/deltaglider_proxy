// SPDX-License-Identifier: BUSL-1.1

//! Per-bucket WRITE gate for maintenance jobs.
//!
//! While a bucket has an active job, S3 **write** requests
//! (PUT/POST/DELETE — uploads, multipart ops, deletes) are rejected with
//! `503 SlowDown` so SDKs back off and retry after the job finishes.
//! **Reads stay up**: the engine's read path serves mixed
//! encrypted/plaintext state transparently, and keeping GET/HEAD/LIST
//! available means public download buckets see zero downtime.
//!
//! Blocking writes is a CORRECTNESS requirement, not UX: the worker
//! rewrites objects via retrieve→store, and a client PUT landing between
//! those two steps would be silently overwritten with stale bytes.
//!
//! ## Why not the admission chain
//!
//! The admission chain is an immutable artifact compiled from config and
//! rebuilt WHOLESALE by `rebuild_bucket_derived_snapshots` on every
//! config apply — a dynamic per-bucket block injected there would
//! silently vanish on the next unrelated apply mid-job. This gate is a
//! separate, permanent middleware layer on the S3 router whose CONTENTS
//! (the busy set) are swapped lock-free; config applies cannot disturb
//! it. The admin router never passes through it, so the job's own engine
//! calls and the admin API stay unblocked (admin object WRITE endpoints
//! check the gate explicitly instead).
//!
//! ## In-flight write draining
//!
//! A write admitted moments BEFORE the gate armed could still land after
//! the worker rewrote that key. The gate therefore counts in-flight S3
//! writes per bucket; the worker waits for the gated bucket's counter to
//! reach zero before scanning anything (bounded by the server's request
//! timeout — no request can legitimately outlive it).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Method, Request};
use axum::middleware::Next;
use axum::response::Response;
use dashmap::DashMap;

use super::store::GateArm;
use crate::config_db::JobsDb;

/// Lock-free busy-bucket set (reads) + per-bucket in-flight write counters.
///
/// The busy set is DERIVED: [`MaintenanceGate::sync_from`] recomputes it
/// from the active job rows (`JobsDb::maintenance_gate_arms`) after every
/// row transition, at boot, and on every worker poll tick. There is no hand
/// `set_busy`/`clear` outside tests, so a missed or reordered call can
/// neither strand a finished bucket busy nor open a bucket whose row is
/// still active. The one early release is [`MaintenanceGate::release_for_unwind`].
#[derive(Debug)]
pub struct MaintenanceGate {
    /// Busy key (lowercased) → the kind of the job that holds it.
    busy: ArcSwap<HashMap<String, String>>,
    /// Serialises the recompute-and-store of `busy`, and holds the ids of
    /// the jobs whose pre-flip unwind released their source bucket. Without
    /// it two concurrent writers both build from their own view and the
    /// last store wins (H36). Reads stay lock-free.
    unwinding: parking_lot::Mutex<HashSet<i64>>,
    inflight_writes: DashMap<String, i64>,
    /// The encryption witness of this node (multi-instance only): why it
    /// refuses writes to a bucket now. Set once at boot.
    write_refusal: std::sync::OnceLock<ProbeCell>,
}

/// Why this node refuses writes to a bucket (memory only).
pub type WriteRefusalProbe = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

struct ProbeCell(WriteRefusalProbe);

impl std::fmt::Debug for ProbeCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WriteRefusalProbe")
    }
}

impl Default for MaintenanceGate {
    fn default() -> Self {
        Self {
            busy: ArcSwap::from_pointee(HashMap::new()),
            unwinding: parking_lot::Mutex::new(HashSet::new()),
            inflight_writes: DashMap::new(),
            write_refusal: std::sync::OnceLock::new(),
        }
    }
}

/// Pure: the busy set for these arms, each key with the kind of its job. A
/// job in `unwinding` keeps every key except its migrate source bucket.
/// Keys compare lowercased.
pub(crate) fn busy_set(arms: &[GateArm], unwinding: &HashSet<i64>) -> HashMap<String, String> {
    arms.iter()
        .filter(|a| !(a.migrate_source && unwinding.contains(&a.job_id)))
        .map(|a| (a.key.to_ascii_lowercase(), a.kind.clone()))
        .collect()
}

/// Pure: the words for a job kind in the gate's 503 text.
pub fn kind_words(kind: &str) -> &'static str {
    match kind {
        "reencrypt" => "a re-encryption",
        "migrate" => "a migration to another backend",
        "backfill-metadata" => "a metadata backfill",
        _ => "a maintenance job",
    }
}

/// Pure: the 503 SlowDown text for a write to a gated bucket.
pub fn refusal_message(bucket: &str, kind: &str) -> String {
    format!(
        "bucket '{bucket}' is temporarily read-only while {} finishes — please retry shortly",
        kind_words(kind)
    )
}

impl MaintenanceGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Is this bucket currently gated? (Bucket names compare lowercased —
    /// the same normalization the routing layer uses.)
    pub fn is_busy(&self, bucket: &str) -> bool {
        self.busy.load().contains_key(&bucket.to_ascii_lowercase())
    }

    /// Install the encryption witness probe (boot, multi-instance only).
    pub fn set_write_refusal_probe(&self, probe: WriteRefusalProbe) {
        let _ = self.write_refusal.set(ProbeCell(probe));
    }

    /// THE check of a background writer before it writes into `bucket`:
    /// why it must stop as deferred (a maintenance job rewrites the bucket,
    /// or the encryption witness refuses this node's writes to it), or
    /// `None`. Memory only. Every replication / lifecycle gate check goes
    /// through here (source guard `background_writers_ask_write_blocked`).
    pub fn write_blocked(&self, bucket: &str) -> Option<String> {
        if let Some(kind) = self.busy_kind(bucket) {
            return Some(format!(
                "bucket '{bucket}' has an active maintenance job ({kind})"
            ));
        }
        self.write_refusal.get().and_then(|probe| (probe.0)(bucket))
    }

    /// The kind of the job that gates this bucket, if one does.
    pub fn busy_kind(&self, bucket: &str) -> Option<String> {
        self.busy.load().get(&bucket.to_ascii_lowercase()).cloned()
    }

    /// Recompute the busy set from the active job rows. Call it with the DB
    /// guard of the row transition still held, so two syncs store in the
    /// order of the transitions. A DB that cannot be read changes nothing
    /// (fail closed: the keys stay armed until the next sync).
    pub fn sync_from(&self, db: &JobsDb) {
        let arms = match db.maintenance_gate_arms() {
            Ok(arms) => arms,
            Err(e) => {
                tracing::warn!("maintenance: write gate not synced (the DB read failed): {e}");
                return;
            }
        };
        let mut unwinding = self.unwinding.lock();
        // A settled job leaves no unwind release behind.
        unwinding.retain(|id| arms.iter().any(|a| a.job_id == *id));
        self.busy.store(Arc::new(busy_set(&arms, &unwinding)));
    }

    /// A migrate that stops before its flip (cancel or failure) makes the
    /// source authoritative again, so clients may write to it at once, not
    /// after the staged-copy cleanup. The row is still active until the
    /// worker settles it; this is the one release that its row does not
    /// drive. The staging route stays gated until the row settles.
    pub fn release_for_unwind(&self, job_id: i64, db: &JobsDb) {
        self.unwinding.lock().insert(job_id);
        self.sync_from(db);
    }

    /// Test-only: add a bucket by hand. Production code derives the set.
    #[cfg(test)]
    pub fn set_busy(&self, bucket: &str) {
        let _g = self.unwinding.lock();
        let mut next = HashMap::clone(&self.busy.load());
        next.insert(bucket.to_ascii_lowercase(), "reencrypt".to_string());
        self.busy.store(Arc::new(next));
    }

    /// Test-only: remove a bucket by hand. Production code derives the set.
    #[cfg(test)]
    pub fn clear(&self, bucket: &str) {
        let _g = self.unwinding.lock();
        let mut next = HashMap::clone(&self.busy.load());
        next.remove(&bucket.to_ascii_lowercase());
        self.busy.store(Arc::new(next));
    }

    /// Number of S3 write requests currently in flight for `bucket`.
    pub fn inflight_writes(&self, bucket: &str) -> i64 {
        self.inflight_writes
            .get(&bucket.to_ascii_lowercase())
            .map(|v| *v)
            .unwrap_or(0)
    }

    /// Public because long-running ADMIN write loops (bulk copy/move/
    /// delete) must participate in the drain alongside the S3 middleware.
    pub fn write_started(&self, bucket: &str) {
        *self
            .inflight_writes
            .entry(bucket.to_ascii_lowercase())
            .or_insert(0) += 1;
    }

    pub fn write_finished(&self, bucket: &str) {
        let key = bucket.to_ascii_lowercase();
        if let Some(mut v) = self.inflight_writes.get_mut(&key) {
            *v -= 1;
            if *v <= 0 {
                drop(v);
                // Best-effort cleanup; a racing increment simply re-creates
                // the entry, counts stay correct because entry() starts at 0.
                self.inflight_writes.remove_if(&key, |_, v| *v <= 0);
            }
        }
    }

    /// RAII in-flight-write registration. Increments the counter now and
    /// decrements on drop, so a cancelled request future (client disconnect
    /// mid-body → hyper drops the handler) still releases its slot. Straight-
    /// line `write_started`/`write_finished` leaks the counter on cancellation,
    /// permanently marking the bucket busy and failing every later drain (H12).
    pub fn begin_write(self: &Arc<Self>, bucket: &str) -> WriteGuard {
        self.write_started(bucket);
        WriteGuard {
            gate: Arc::clone(self),
            bucket: bucket.to_ascii_lowercase(),
        }
    }
}

/// Decrements the gate's in-flight-write counter on drop — cancellation-safe.
#[must_use = "dropping the guard immediately ends the in-flight-write window"]
pub struct WriteGuard {
    gate: Arc<MaintenanceGate>,
    bucket: String,
}

impl Drop for WriteGuard {
    fn drop(&mut self) {
        self.gate.write_finished(&self.bucket);
    }
}

/// First path segment of an S3 request = the bucket (lowercased), decoded
/// as s3s decodes it (`/rele%61ses/k` is bucket `releases`). Root-level
/// requests (ListBuckets, health) have no bucket. A path that does not
/// decode has none either: s3s rejects it with `InvalidURI`.
/// Shared with the backend-health gate (`coordination::health`).
pub(crate) fn bucket_from_path(path: &str) -> Option<String> {
    crate::api::request_target::RequestTarget::parse(path, None)
        .ok()?
        .bucket()
        .map(str::to_ascii_lowercase)
}

fn is_write_method(method: &Method) -> bool {
    matches!(*method, Method::PUT | Method::POST | Method::DELETE)
}

/// Axum middleware on the S3 router: counts in-flight writes. It does NOT
/// refuse anything: it runs before s3s verifies the signature, and the
/// refusal names the background job, so an unverified caller would learn
/// which buckets are busy. The refusal is [`check_verified_request`].
///
/// Registering here, before the busy check, keeps the acquire-then-recheck
/// order (H37): a write that the drain did not see is caught by the later
/// check. RAII releases the slot on a mid-body disconnect (H12).
pub async fn maintenance_gate_middleware(request: Request<Body>, next: Next) -> Response {
    let Some(gate) = request.extensions().get::<Arc<MaintenanceGate>>().cloned() else {
        // Gate not wired (shouldn't happen in production) — never block.
        return next.run(request).await;
    };
    if !is_write_method(request.method()) {
        return next.run(request).await;
    }
    let Some(bucket) = bucket_from_path(request.uri().path()) else {
        return next.run(request).await;
    };
    let _write = gate.begin_write(&bucket);
    next.run(request).await
}

/// The maintenance refusal for one request: 503 SlowDown for a write to a
/// busy bucket. Reads always pass.
pub fn write_gate_refusal(
    gate: &MaintenanceGate,
    method: &Method,
    path: &str,
) -> Option<crate::api::errors::S3Error> {
    if !is_write_method(method) {
        return None;
    }
    let bucket = bucket_from_path(path)?;
    gate.busy_kind(&bucket)
        .map(|kind| crate::api::errors::S3Error::SlowDown(refusal_message(&bucket, &kind)))
}

/// The request gates whose refusal reveals internal state (a busy bucket,
/// an unhealthy backend and its cause). Run them ONLY once the caller's
/// credentials are verified: from the s3s access hook, and from the
/// form-POST handler after its policy signature check. A forged signature
/// then gets 403, never the gate's 503. The gates come from the request
/// extensions the S3 router installs; a missing one never blocks.
pub fn check_verified_request(
    extensions: &axum::http::Extensions,
    method: &Method,
    path: &str,
) -> Result<(), crate::api::errors::S3Error> {
    if let Some(gate) = extensions.get::<crate::coordination::health::BackendHealthGate>() {
        if let Some(refusal) = crate::coordination::health::reserved_bucket_refusal(gate, path) {
            return Err(refusal);
        }
    }
    if let Some(gate) = extensions.get::<Arc<MaintenanceGate>>() {
        if let Some(refusal) = write_gate_refusal(gate, method, path) {
            return Err(refusal);
        }
    }
    if let Some(gate) = extensions.get::<crate::coordination::health::BackendHealthGate>() {
        if let Some(refusal) = crate::coordination::health::health_gate_refusal(gate, path) {
            return Err(refusal);
        }
    }
    Ok(())
}

/// The backend-health check of background work (lifecycle, maintenance).
/// Client requests to a bucket whose backend is gated (unreachable, or its
/// credentials refused) get a fast 503. A background job sent its whole
/// volume to that backend anyway: every request waited out its timeout,
/// fed the passive health signal, and kept the backend gated. Each job
/// asks this check before it starts and before each page, and defers while
/// the answer is `Some`.
pub(crate) struct BackendCheck {
    state: Arc<crate::api::handlers::AppState>,
    config: crate::config::SharedConfig,
    /// Buckets whose backend the job uses (resolved at each check: a
    /// config apply can move a bucket).
    buckets: Vec<String>,
    /// Backends the job uses by name (a migrate's source and target).
    backends: Vec<String>,
}

impl BackendCheck {
    pub(crate) fn new(
        state: Arc<crate::api::handlers::AppState>,
        config: crate::config::SharedConfig,
        buckets: Vec<String>,
        backends: Vec<String>,
    ) -> Self {
        Self {
            state,
            config,
            buckets,
            backends,
        }
    }

    /// Why the job must wait: the first gated backend and its cause, or
    /// `None`. Fail-open like the request gate: no unhealthy backend, a
    /// config that an apply holds, an unknown backend or no verdict.
    pub(crate) fn gated(&self) -> Option<String> {
        // The encryption witness refuses this node's writes into a bucket of
        // the job: wait like for a gated backend (memory only).
        if let Some(why) = self.buckets.iter().find_map(|b| {
            use crate::storage::StorageBackend;
            self.state.engine.load().storage().write_refusal(b)
        }) {
            return Some(why);
        }
        let health = &self.state.backend_health;
        if health.unhealthy_names().is_empty() {
            return None;
        }
        let cfg = self.config.try_read().ok()?;
        let by_bucket = self.buckets.iter().filter_map(|bucket| {
            use crate::storage::StorageBackend;
            let routed = self
                .state
                .engine
                .load()
                .storage()
                .resolved_backend_name(bucket);
            crate::coordination::health::gated_backend(&cfg, bucket, routed)
        });
        let by_name = self
            .backends
            .iter()
            .filter_map(|name| Some((name.clone(), cfg.backend_by_name(name)?.clone())));
        by_bucket.chain(by_name).find_map(|(name, def)| {
            let verdict = health.get(&name, &def)?;
            verdict
                .is_gating()
                .then(|| format!("backend '{name}' is unavailable: {}", verdict.cause()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Source guard: replication and lifecycle ask `write_blocked` (the
    /// maintenance job AND the encryption witness), never `is_busy` alone.
    #[test]
    fn background_writers_ask_write_blocked() {
        let mut offenders = Vec::new();
        for (rel, text) in crate::source_scan::prod_sources("src") {
            if !(rel.starts_with("src/replication") || rel.starts_with("src/lifecycle")) {
                continue;
            }
            if crate::source_scan::prod_text(&text).contains(".is_busy(") {
                offenders.push(rel);
            }
        }
        assert!(
            offenders.is_empty(),
            "use MaintenanceGate::write_blocked: {offenders:?}"
        );
    }

    /// Owner decision (#10 of the P3 review): a maintenance job (re-encrypt,
    /// migrate, backfill) whose bucket the encryption witness refuses waits
    /// like for a gated backend (the worker defers the row with its
    /// cursor), and the background writers' gate check names it.
    #[tokio::test]
    async fn a_witness_refusal_gates_jobs_and_background_writers() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config::from_yaml_str(&format!(
            "storage:\n  filesystem: {}\n",
            dir.path().display()
        ))
        .unwrap();
        let engine = crate::deltaglider::DynEngine::new(&cfg, None)
            .await
            .unwrap();
        let witness = engine.encryption_policies().witness_gate();
        witness.activate(std::time::Duration::from_secs(60));
        witness.synced();
        witness.set(crate::storage::WitnessGateState {
            refused: HashMap::from([(
                ("default".to_string(), "b".to_string()),
                "witness mismatch".to_string(),
            )]),
            witnessed: Default::default(),
        });
        let state = crate::api::handlers::AppState::for_tests(engine);
        let check = BackendCheck::new(
            state.clone(),
            cfg.into_shared(),
            vec!["b".to_string()],
            Vec::new(),
        );
        assert_eq!(check.gated().as_deref(), Some("witness mismatch"));
        let gate = MaintenanceGate::new();
        assert_eq!(gate.write_blocked("b"), None, "no probe: single instance");
        let probe_state = state.clone();
        gate.set_write_refusal_probe(Arc::new(move |b: &str| {
            probe_state.engine.load().write_refusal(b)
        }));
        assert_eq!(gate.write_blocked("b").as_deref(), Some("witness mismatch"));
        assert_eq!(gate.write_blocked("other"), None);
        gate.set_busy("other");
        assert!(gate
            .write_blocked("other")
            .unwrap()
            .contains("has an active maintenance job"));
    }

    use crate::maintenance::store::current_unix_seconds;

    const MIGRATE_PARAMS: &str = r#"{"target_backend":"t","delete_source":false,"transient_key":"__dgmigrate_m_0","from_backend":"s"}"#;

    fn busy(g: &MaintenanceGate) -> Vec<String> {
        let mut v: Vec<String> = g.busy.load().keys().cloned().collect();
        v.sort();
        v
    }

    /// R8: the busy set is the active rows, whatever the gate held before —
    /// a key armed by hand (or left over) goes, a key of an active row stays
    /// until its row settles or its phase stops gating it.
    #[test]
    fn sync_from_derives_the_busy_set_from_active_rows() {
        let db = JobsDb::in_memory("testpass").unwrap();
        let g = MaintenanceGate::new();
        g.set_busy("stale");
        let r = db
            .maintenance_create_job("reencrypt", "Enc", "counting", None, "admin", 1)
            .unwrap()
            .unwrap();
        let m = db
            .maintenance_create_job("migrate", "m", "copy", Some(MIGRATE_PARAMS), "admin", 1)
            .unwrap()
            .unwrap();
        g.sync_from(&db);
        assert_eq!(busy(&g), vec!["__dgmigrate_m_0", "enc", "m"]);

        // A row released back to `queued` (a graceful stop) is still active.
        for _ in [r, m] {
            db.maintenance_claim_next_job("w", current_unix_seconds(), 60)
                .unwrap()
                .unwrap();
        }
        db.maintenance_release_for_resume(r, "w").unwrap();
        g.sync_from(&db);
        assert!(g.is_busy("enc"), "a job left resumable keeps its gate");

        // Post-flip migrate gates nothing; a settled row gates nothing.
        db.maintenance_update_progress(m, "cleanup", None, 0, 0, 0, 0, None)
            .unwrap();
        g.sync_from(&db);
        assert_eq!(busy(&g), vec!["enc"]);
        db.maintenance_finish(r, "completed", None).unwrap();
        g.sync_from(&db);
        assert!(busy(&g).is_empty());
    }

    #[test]
    fn sync_from_an_unreadable_db_changes_nothing() {
        let db = JobsDb::in_memory("testpass").unwrap();
        db.maintenance_create_job("reencrypt", "b", "counting", None, "admin", 1)
            .unwrap()
            .unwrap();
        let g = MaintenanceGate::new();
        g.sync_from(&db);
        db.conn
            .execute_batch("ALTER TABLE maintenance_jobs RENAME TO away")
            .unwrap();
        g.sync_from(&db);
        assert!(g.is_busy("b"), "a failed read must not open the gate");
    }

    /// jobs.md: a cancel or failure before the flip releases the source
    /// bucket at once; the staging route stays gated until the row settles,
    /// and the release does not outlive the row.
    #[test]
    fn release_for_unwind_opens_only_the_source_until_the_row_settles() {
        let db = JobsDb::in_memory("testpass").unwrap();
        let m = db
            .maintenance_create_job("migrate", "m", "verify", Some(MIGRATE_PARAMS), "admin", 1)
            .unwrap()
            .unwrap();
        let g = MaintenanceGate::new();
        g.sync_from(&db);
        g.release_for_unwind(m, &db);
        assert_eq!(busy(&g), vec!["__dgmigrate_m_0"]);
        g.sync_from(&db); // a poll tick mid-unwind keeps the release
        assert_eq!(busy(&g), vec!["__dgmigrate_m_0"]);
        db.maintenance_finish(m, "failed", Some("x")).unwrap();
        g.sync_from(&db);
        assert!(busy(&g).is_empty());
        assert!(
            g.unwinding.lock().is_empty(),
            "the release ends with the row"
        );
        // A later job on the same bucket gates it again.
        db.maintenance_create_job("reencrypt", "m", "counting", None, "admin", 2)
            .unwrap()
            .unwrap();
        g.sync_from(&db);
        assert!(g.is_busy("m"));
    }

    /// A reencrypt (or backfill) bucket is not a migrate source: an unwind
    /// id never opens it.
    #[test]
    fn busy_set_releases_only_a_migrate_source() {
        let arms = [
            GateArm {
                job_id: 1,
                key: "B".into(),
                kind: "reencrypt".into(),
                migrate_source: false,
            },
            GateArm {
                job_id: 2,
                key: "m".into(),
                kind: "migrate".into(),
                migrate_source: true,
            },
        ];
        let unwinding = HashSet::from([1, 2]);
        assert_eq!(
            busy_set(&arms, &unwinding),
            HashMap::from([("b".into(), "reencrypt".into())])
        );
        assert_eq!(busy_set(&arms, &HashSet::new()).len(), 2);
    }

    #[test]
    fn busy_set_round_trips_case_insensitively() {
        let g = MaintenanceGate::new();
        assert!(!g.is_busy("pippo"));
        g.set_busy("Pippo");
        assert!(g.is_busy("pippo"));
        assert!(g.is_busy("PIPPO"));
        g.set_busy("pippo"); // idempotent
        g.clear("pipPO");
        assert!(!g.is_busy("pippo"));
        g.clear("pippo"); // idempotent
    }

    #[test]
    fn inflight_write_counter_tracks_and_cleans_up() {
        let g = MaintenanceGate::new();
        assert_eq!(g.inflight_writes("b"), 0);
        g.write_started("b");
        g.write_started("B");
        assert_eq!(g.inflight_writes("b"), 2);
        g.write_finished("b");
        assert_eq!(g.inflight_writes("b"), 1);
        g.write_finished("b");
        assert_eq!(g.inflight_writes("b"), 0);
        assert!(g.inflight_writes.is_empty(), "zeroed entries are removed");
    }

    #[test]
    fn write_guard_decrements_on_drop_including_early_drop() {
        let g = Arc::new(MaintenanceGate::new());
        assert_eq!(g.inflight_writes("b"), 0);
        {
            let _w = g.begin_write("b");
            assert_eq!(g.inflight_writes("b"), 1);
            // Guard drops at end of scope even if the surrounding future is
            // cancelled — this is the cancellation-safety H12 relies on.
        }
        assert_eq!(g.inflight_writes("b"), 0, "guard must decrement on drop");

        // Explicit early drop (mimics a request future dropped mid-body).
        let w = g.begin_write("b");
        assert_eq!(g.inflight_writes("b"), 1);
        drop(w);
        assert_eq!(g.inflight_writes("b"), 0);
        assert!(g.inflight_writes.is_empty(), "zeroed entries are removed");
    }

    #[test]
    fn concurrent_guards_stack_and_unwind_independently() {
        let g = Arc::new(MaintenanceGate::new());
        let a = g.begin_write("b");
        let b = g.begin_write("b");
        assert_eq!(g.inflight_writes("b"), 2);
        drop(a);
        assert_eq!(g.inflight_writes("b"), 1);
        drop(b);
        assert_eq!(g.inflight_writes("b"), 0);
    }

    #[tokio::test]
    async fn drain_style_poll_waits_for_a_held_write_guard() {
        // Models the H22 contract: a background copy (replication/lifecycle) that
        // holds a begin_write guard keeps inflight_writes>0, so a maintenance
        // drain_inflight_writes-style poll BLOCKS until the copy's guard drops —
        // it cannot drain-through and rewrite the same key mid-copy.
        let g = Arc::new(MaintenanceGate::new());
        let guard = g.begin_write("b");
        assert_eq!(g.inflight_writes("b"), 1);

        // A drain poll on another task: completes only once the guard is dropped.
        let g2 = Arc::clone(&g);
        let drain = tokio::spawn(async move {
            while g2.inflight_writes("b") > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        });

        // Give the drain time to spin; it must NOT complete while we hold the guard.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            !drain.is_finished(),
            "drain must block while a write is in flight"
        );

        drop(guard);
        // Now it drains.
        tokio::time::timeout(std::time::Duration::from_secs(1), drain)
            .await
            .expect("drain must complete once the guard drops")
            .unwrap();
        assert_eq!(g.inflight_writes("b"), 0);
    }

    #[test]
    fn bucket_from_path_shapes() {
        assert_eq!(bucket_from_path("/"), None);
        assert_eq!(bucket_from_path(""), None);
        assert_eq!(bucket_from_path("/Bucket"), Some("bucket".into()));
        assert_eq!(bucket_from_path("/rele%61ses/k"), Some("releases".into()));
        assert_eq!(bucket_from_path("/b%2Fk"), Some("b".into()));
        assert_eq!(
            bucket_from_path("/bucket/key/with/slashes"),
            Some("bucket".into())
        );
    }

    #[test]
    fn write_gate_refuses_only_writes_to_a_busy_bucket() {
        let g = MaintenanceGate::new();
        g.set_busy("busy");
        assert!(write_gate_refusal(&g, &Method::PUT, "/busy/k").is_some());
        assert!(write_gate_refusal(&g, &Method::DELETE, "/Busy/k").is_some());
        assert!(write_gate_refusal(&g, &Method::POST, "/busy").is_some());
        assert!(write_gate_refusal(&g, &Method::GET, "/busy/k").is_none());
        assert!(write_gate_refusal(&g, &Method::PUT, "/idle/k").is_none());
        assert!(write_gate_refusal(&g, &Method::PUT, "/").is_none());
    }

    #[test]
    fn check_verified_request_without_gates_never_blocks() {
        let ext = axum::http::Extensions::new();
        assert!(check_verified_request(&ext, &Method::PUT, "/b/k").is_ok());
        let mut ext = axum::http::Extensions::new();
        let g = Arc::new(MaintenanceGate::new());
        g.set_busy("b");
        ext.insert(g);
        assert!(check_verified_request(&ext, &Method::PUT, "/b/k").is_err());
        assert!(check_verified_request(&ext, &Method::HEAD, "/b/k").is_ok());
    }

    /// Overview B6: a write during a backfill read "(re-encryption or
    /// migration)". The 503 names the job that holds the bucket.
    #[test]
    fn the_refusal_names_the_job_kind_that_holds_the_bucket() {
        let db = JobsDb::in_memory("testpass").unwrap();
        db.maintenance_create_job("backfill-metadata", "bf", "counting", None, "admin", 1)
            .unwrap()
            .unwrap();
        db.maintenance_create_job("migrate", "m", "copy", Some(MIGRATE_PARAMS), "admin", 1)
            .unwrap()
            .unwrap();
        let g = MaintenanceGate::new();
        g.sync_from(&db);
        let text = |path: &str| match write_gate_refusal(&g, &Method::PUT, path) {
            Some(crate::api::errors::S3Error::SlowDown(m)) => m,
            other => panic!("expected SlowDown, got {other:?}"),
        };
        assert!(
            text("/bf/k").contains("a metadata backfill"),
            "{}",
            text("/bf/k")
        );
        assert!(!text("/bf/k").contains("re-encryption"));
        assert!(text("/m/k").contains("a migration"));
        assert!(text("/__dgmigrate_m_0/k").contains("a migration"));
        assert_eq!(kind_words("reencrypt"), "a re-encryption");
        assert_eq!(kind_words("future-kind"), "a maintenance job");
    }

    #[test]
    fn write_method_classification() {
        assert!(is_write_method(&Method::PUT));
        assert!(is_write_method(&Method::POST));
        assert!(is_write_method(&Method::DELETE));
        assert!(!is_write_method(&Method::GET));
        assert!(!is_write_method(&Method::HEAD));
        assert!(!is_write_method(&Method::OPTIONS));
    }
}
