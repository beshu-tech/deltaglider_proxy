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
//! the worker rewrote that key. The gate therefore counts in-flight writes
//! per bucket: S3 write requests, admin bulk loops, and the background
//! writers (replication, lifecycle: [`admit_all`]). The worker waits for
//! the gated bucket's counter to reach zero before scanning anything,
//! bounded by the server's request timeout. No client request outlives
//! it, so a drain stuck on client writes fails the job; a background write
//! has no request deadline, so a drain that times out while one is in
//! flight defers the job (`maintenance::worker::drain_inflight_writes`).

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
    /// Of `inflight_writes`, the background writes ([`admit_all`]): a
    /// replication copy has no request deadline, so a drain that waits for
    /// one past its ceiling defers instead of failing.
    inflight_background: DashMap<String, i64>,
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
            inflight_background: DashMap::new(),
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

    /// Install the write-refusal probe: the encryption witness and the key
    /// sentinel (boot).
    pub fn set_write_refusal_probe(&self, probe: WriteRefusalProbe) {
        let _ = self.write_refusal.set(ProbeCell(probe));
    }

    /// THE check of a background writer before it writes into `bucket`:
    /// why it must stop as deferred (a maintenance job rewrites the bucket,
    /// or the encryption witness refuses this node's writes to it), or
    /// `None`. Memory only. Every replication / lifecycle gate check goes
    /// through here (source guard `background_writers_ask_write_blocked`).
    pub fn write_blocked(&self, bucket: &str) -> Option<String> {
        self.refusal_of(bucket).map(WriteRefusal::into_reason)
    }

    /// [`Self::write_blocked`], with the cause kept apart.
    fn refusal_of(&self, bucket: &str) -> Option<WriteRefusal> {
        if let Some(kind) = self.busy_kind(bucket) {
            return Some(WriteRefusal::Maintenance(format!(
                "bucket '{bucket}' has an active maintenance job ({kind})"
            )));
        }
        self.write_refusal
            .get()
            .and_then(|probe| (probe.0)(bucket))
            .map(WriteRefusal::Refused)
    }

    /// THE admission of one background write (replication copy, mirror
    /// delete, lifecycle act) into `bucket`: count it as in flight FIRST,
    /// then ask [`Self::write_blocked`]. A maintenance job that arms at any
    /// moment either sees the count (its drain waits for the write) or the
    /// write sees the job (the guard drops, the caller defers). Checking
    /// first and counting after left a window where the drain saw 0 and
    /// the write still landed (F9). Hold the guard until the write ends.
    /// Source guard `background_writers_are_admitted`.
    pub fn admit_background_write(
        self: &Arc<Self>,
        bucket: &str,
    ) -> Result<Admitted, WriteRefusal> {
        admit_all(Some(self), &[bucket])
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
            background: false,
        }
    }

    /// [`Self::begin_write`] for a background writer (counted apart too).
    ///
    /// Order (review of #166): the background count goes up BEFORE the
    /// total and down AFTER it (`WriteGuard::drop`), so a drain that reads
    /// the total, then the background count, never sees a background
    /// write in the total that the background count misses.
    fn begin_background_write(self: &Arc<Self>, bucket: &str) -> WriteGuard {
        let key = bucket.to_ascii_lowercase();
        bump(&self.inflight_background, &key, 1);
        self.write_started(&key);
        WriteGuard {
            gate: Arc::clone(self),
            bucket: key,
            background: true,
        }
    }

    /// A write of request work that runs on detached from its request (a
    /// DeleteObjects batch, the store of a CompleteMultipartUpload): no
    /// request deadline bounds it, so the drain counts it as background
    /// (a drain that times out on it defers its job instead of failing).
    pub fn begin_detached_write(self: &Arc<Self>, bucket: &str) -> WriteGuard {
        self.begin_background_write(bucket)
    }

    /// Of [`Self::inflight_writes`], the background writes.
    pub fn inflight_background_writes(&self, bucket: &str) -> i64 {
        self.inflight_background
            .get(&bucket.to_ascii_lowercase())
            .map(|v| *v)
            .unwrap_or(0)
    }
}

/// Proof that background writes into some buckets are admitted: each
/// bucket counted as in flight, then checked ([`admit_all`]). Only the
/// admission makes one, and the background writers' copy and delete
/// (`crate::transfer::admitted`) take it by reference, so a write cannot run without a
/// live admission of its bucket (dropping the guard before the write does
/// not compile). The counts end when it drops.
#[derive(Debug)]
#[must_use = "dropping the admission ends the in-flight-write window"]
pub struct Admitted {
    buckets: Vec<String>,
    /// `None` per bucket without a gate (no maintenance job can exist).
    _guards: Vec<Option<WriteGuard>>,
}

impl Admitted {
    /// Does this admission cover writes into `bucket`?
    /// Names compare trimmed and case-insensitive, as the rule's write
    /// buckets are built (`lifecycle::planner::rule_write_buckets` trims).
    pub fn covers(&self, bucket: &str) -> bool {
        self.buckets
            .iter()
            .any(|b| b.trim().eq_ignore_ascii_case(bucket.trim()))
    }
}

/// THE admission of background writes into `buckets`: each is counted,
/// THEN checked, so a maintenance job that arms at any moment either sees
/// the count or is seen (all counts drop, `Err`). Without a gate (`None`:
/// tests, a node with no maintenance plane) the writes are admitted
/// uncounted.
pub fn admit_all(
    gate: Option<&Arc<MaintenanceGate>>,
    buckets: &[&str],
) -> Result<Admitted, WriteRefusal> {
    let mut guards = Vec::with_capacity(buckets.len());
    for bucket in buckets {
        guards.push(gate.map(|g| g.begin_background_write(bucket)));
    }
    if let Some(gate) = gate {
        if let Some(refusal) = buckets.iter().find_map(|b| gate.refusal_of(b)) {
            return Err(refusal); // `guards` drop: no longer counted.
        }
    }
    Ok(Admitted {
        buckets: buckets.iter().map(|b| b.to_string()).collect(),
        _guards: guards,
    })
}

/// Why [`MaintenanceGate::admit_background_write`] refused a write. Either
/// way the writer defers (the key waits), it never records a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteRefusal {
    /// A maintenance job rewrites the bucket: it ends on its own.
    Maintenance(String),
    /// This node refuses writes to the bucket (the encryption witness): it
    /// may last.
    Refused(String),
}

impl WriteRefusal {
    pub fn reason(&self) -> &str {
        match self {
            WriteRefusal::Maintenance(r) | WriteRefusal::Refused(r) => r,
        }
    }

    pub fn into_reason(self) -> String {
        match self {
            WriteRefusal::Maintenance(r) | WriteRefusal::Refused(r) => r,
        }
    }

    pub fn is_maintenance(&self) -> bool {
        matches!(self, WriteRefusal::Maintenance(_))
    }
}

/// Decrements the gate's in-flight-write counter on drop — cancellation-safe.
#[must_use = "dropping the guard immediately ends the in-flight-write window"]
#[derive(Debug)]
pub struct WriteGuard {
    gate: Arc<MaintenanceGate>,
    bucket: String,
    background: bool,
}

impl WriteGuard {
    /// Does this count cover writes into `bucket`? (Case-insensitive.)
    pub fn covers(&self, bucket: &str) -> bool {
        self.bucket.eq_ignore_ascii_case(bucket)
    }
}

impl Drop for WriteGuard {
    fn drop(&mut self) {
        // The total first, then the background count (see
        // `begin_background_write`).
        self.gate.write_finished(&self.bucket);
        if self.background {
            bump(&self.gate.inflight_background, &self.bucket, -1);
        }
    }
}

/// The in-flight count of one S3 write request, shared with the request's
/// extensions. A request is counted only when it is VERIFIED and ADMITTED:
/// [`check_verified_request`] counts it, then asks the gate (count first,
/// so a job that arms at any moment either sees the count or refuses the
/// request), and ends the count at once on a refusal. The middleware only
/// puts the empty slot into the request. Nothing that is not yet verified
/// is in flight: once a bucket is gated, every request that is not yet
/// verified will be refused, so a drain ignores it, and a caller that
/// trickles a request head cannot hold a drain.
#[derive(Clone, Default)]
pub struct RequestWriteCount(Arc<parking_lot::Mutex<Option<WriteGuard>>>);

impl RequestWriteCount {
    /// Count the write now (once).
    fn admit(&self, gate: &Arc<MaintenanceGate>, bucket: &str) {
        let mut slot = self.0.lock();
        if slot.is_none() {
            *slot = Some(gate.begin_write(bucket));
        }
    }

    /// End the count (idempotent).
    pub fn release(&self) {
        self.0.lock().take();
    }
}

/// Add `by` to `map[key]` (lowercased already); a count at 0 goes.
fn bump(map: &DashMap<String, i64>, key: &str, by: i64) {
    let mut entry = map.entry(key.to_string()).or_insert(0);
    *entry += by;
    if *entry <= 0 {
        drop(entry);
        map.remove_if(key, |_, v| *v <= 0);
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

/// Axum middleware on the S3 router: gives each write request its empty
/// [`RequestWriteCount`]. It counts and refuses NOTHING: it runs before s3s
/// verifies the signature, and the refusal names the background job, so an
/// unverified caller would learn which buckets are busy. The count and the
/// refusal are [`check_verified_request`]. RAII ends the count when the
/// request ends, also on a mid-body disconnect (H12).
pub async fn maintenance_gate_middleware(request: Request<Body>, next: Next) -> Response {
    if !is_write_method(request.method()) {
        return next.run(request).await;
    }
    let count = RequestWriteCount::default();
    let mut request = request;
    request.extensions_mut().insert(count.clone());
    let response = next.run(request).await;
    // Held to here: the guard drops with the last clone.
    drop(count);
    response
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
    // Count FIRST, then ask the gates (acquire-then-recheck, H37): a job
    // that arms later either sees this count or refuses the request.
    let count = extensions.get::<RequestWriteCount>();
    if let (Some(count), Some(gate), true) = (
        count,
        extensions.get::<Arc<MaintenanceGate>>(),
        is_write_method(method),
    ) {
        if let Some(bucket) = bucket_from_path(path) {
            count.admit(gate, &bucket);
        }
    }
    let verdict = verified_request_refusal(extensions, method, path);
    if verdict.is_err() {
        if let Some(count) = count {
            count.release();
        }
    }
    verdict
}

fn verified_request_refusal(
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
    /// Wait while a bucket of the job is read-only (N2: encrypted, on a
    /// backend without atomic writes). Off for a migrate: moving the
    /// bucket to another backend is the way out of that state.
    read_only_waits: bool,
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
            read_only_waits: true,
        }
    }

    /// A check that does not wait for a read-only bucket (a migrate).
    pub(crate) fn ignoring_read_only(mut self) -> Self {
        self.read_only_waits = false;
        self
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
        if self.read_only_waits {
            if let Some(why) = self.buckets.iter().find_map(|b| {
                self.state
                    .atomic_write_block_now(b)
                    .map(|w| w.admin_message(b))
            }) {
                return Some(why);
            }
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
        for src in crate::source_scan::ast::src() {
            if !(src.rel.starts_with("src/replication") || src.rel.starts_with("src/lifecycle")) {
                continue;
            }
            // A method call or the UFCS form `MaintenanceGate::is_busy(&g, b)`.
            if src.prod.calls_named("is_busy").next().is_some() {
                offenders.push(src.rel.clone());
            }
        }
        assert!(
            offenders.is_empty(),
            "use MaintenanceGate::write_blocked: {offenders:?}"
        );
    }

    /// Does the fn `name` of the source file `rel` take a `WriteGuard`
    /// parameter? (Its signature text: the exemption holds only while the
    /// type is there.)
    fn takes_write_guard(rel: &str, name: &str) -> bool {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
        let Ok(text) = std::fs::read_to_string(path) else {
            return false;
        };
        let Some(at) = text.find(&format!("fn {name}(")) else {
            return false;
        };
        let signature = &text[at..];
        let end = signature.find(") ->").unwrap_or(signature.len());
        signature[..end].contains("WriteGuard")
    }

    /// Request-scoped writers that take their count as a `&WriteGuard`
    /// parameter: the type proves it, the caller counts.
    const PROOF_PARAMETER: [(&str, &str, &str); 1] = [(
        "src/api/admin/objects.rs",
        "copy_one",
        "takes the bulk loop's &WriteGuard",
    )];

    /// Code exempt from [`background_writers_are_admitted`], with why.
    const ADMISSION_EXEMPT: [(&str, &str); 5] = [
        ("src/deltaglider/", "the engine: its callers are admitted"),
        ("src/storage/", "the backends: below the engine"),
        (
            "src/transfer.rs",
            "the copy primitive: its callers are admitted",
        ),
        (
            "src/maintenance/",
            "the jobs that hold the gate and drain it",
        ),
        ("src/cli/", "offline CLI commands: no server, no gate"),
    ];

    /// Is `name` a writer the gate must count: a `transfer::copy_object_*`
    /// fn or `delete_batch`?
    fn writer_name(name: &str) -> bool {
        name.starts_with("copy_object_") || name == "delete_batch"
    }

    /// The writers the gate must count, found in `src`: every call of a
    /// `transfer::copy_object_*` fn and every `delete_batch` (method, UFCS
    /// path, or a `use … as alias` of either), but the proof-taking ones of
    /// `transfer::admitted`.
    fn writer_calls(src: &crate::source_scan::ast::Source) -> Vec<&crate::source_scan::ast::Call> {
        let aliases: Vec<&str> = src
            .prod
            .paths
            .iter()
            .filter(|p| p.segs.last().is_some_and(|s| writer_name(s)))
            .filter_map(|p| p.alias.as_deref())
            .collect();
        src.prod
            .calls
            .iter()
            .filter(|c| {
                if c.method {
                    return c.name == "delete_batch";
                }
                if c.segs.iter().any(|s| s == "admitted") {
                    return false;
                }
                let last = c.segs.last().map(String::as_str).unwrap_or("");
                writer_name(last) || (c.segs.len() == 1 && aliases.contains(&last))
            })
            .collect()
    }

    /// `admit_all(None, …)` of a bucket: an uncounted proof. Only an
    /// admission of no bucket (`admit_all(None, &[])`, a preview that writes
    /// nothing) may pass `None` literally; a writer passes its gate.
    fn uncounted_admissions(src: &crate::source_scan::ast::Source) -> Vec<usize> {
        src.prod
            .calls
            .iter()
            .filter(|c| !c.method && c.named("admit_all"))
            .filter(|c| c.arg_idents.first() == Some(&Some("None".to_string())))
            .filter(|c| {
                c.arg_idents_all.len() > 1 || !c.arg_strings.is_empty() || !c.arg_calls.is_empty()
            })
            .map(|c| c.line)
            .collect()
    }

    fn background(rel: &str) -> bool {
        rel.starts_with("src/replication/") || rel.starts_with("src/lifecycle/")
    }

    /// The writes of `transfer::admitted`, each of which takes an [`Admitted`] proof.
    const ADMITTED_WRITES: [&str; 4] = [
        "copy_known",
        "copy_from",
        "copy_with_retries",
        "delete_batch",
    ];

    fn admitted_writes(src: &crate::source_scan::ast::Source) -> usize {
        src.prod
            .calls
            .iter()
            .filter(|c| {
                !c.method
                    && c.segs.iter().any(|s| s == "admitted")
                    && ADMITTED_WRITES.iter().any(|n| c.named(n))
            })
            .count()
    }

    /// Offenders of the rule (see [`background_writers_are_admitted`]).
    fn unadmitted(sources: &[crate::source_scan::ast::Source]) -> Vec<String> {
        let mut offenders = Vec::new();
        for src in sources {
            if ADMISSION_EXEMPT
                .iter()
                .any(|(prefix, _)| src.rel.starts_with(prefix))
            {
                continue;
            }
            let uncounted = uncounted_admissions(src);
            if !uncounted.is_empty() {
                offenders.push(format!(
                    "{}:{uncounted:?} admit_all(None, …) of a bucket makes an uncounted proof",
                    src.rel
                ));
            }
            let writers = writer_calls(src);
            let lines: Vec<usize> = writers.iter().map(|c| c.line).collect();
            if background(&src.rel) {
                // A background writer writes only through `admitted::*`: the
                // proof is borrowed at the write, so the admission is alive
                // there (a dropped guard does not compile).
                if !lines.is_empty() {
                    offenders.push(format!(
                        "{}:{lines:?} writes without an Admitted proof (use transfer::admitted)",
                        src.rel
                    ));
                }
                if let Some(c) = src.prod.calls_named("begin_write").next() {
                    offenders.push(format!(
                        "{}:{} counts with begin_write and skips the re-check (use admit_all)",
                        src.rel, c.line
                    ));
                }
                continue;
            }
            if lines.is_empty() {
                continue;
            }
            // A request-scoped writer (S3 adapter, admin bulk loops, which
            // check the gate per item after counting) counts in the module
            // or one inside it.
            // The count is in the SAME function as the write, or the function
            // takes the caller's `&WriteGuard` (listed in `PROOF_PARAMETER`).
            let names = [
                "admit_background_write",
                "admit_all",
                "begin_write",
                "begin_detached_write",
            ];
            for call in writers {
                let counted =
                    src.prod.calls.iter().any(|other| {
                        other.func == call.func && names.iter().any(|n| other.named(n))
                    });
                let name = src.fn_name(call.func);
                let proven = PROOF_PARAMETER
                    .iter()
                    .any(|(rel, f, _)| *rel == src.rel && *f == name && takes_write_guard(rel, f));
                if !counted && !proven {
                    offenders.push(format!(
                        "{}:{} in {name} (needs one of {names:?} in the same function)",
                        src.rel, call.line
                    ));
                }
            }
        }
        offenders
    }

    /// Source guard (F9): every non-engine module that writes through
    /// `transfer::copy_object_*` or `delete_batch` is counted by the gate,
    /// so a maintenance job's `drain_inflight_writes` waits for it. The
    /// event consumer's copies and both mirror-delete paths were not, and
    /// the drain saw 0 while they wrote. A background writer (replication,
    /// lifecycle) writes only through `transfer::admitted`, whose every write
    /// borrows the `Admitted` proof that `admit_all` (count, then check)
    /// makes; it never counts with a bare `begin_write`.
    #[test]
    fn background_writers_are_admitted() {
        let sources = crate::source_scan::ast::src();
        let offenders = unadmitted(sources);
        assert!(
            offenders.is_empty(),
            "writers the maintenance gate does not count: {offenders:?}"
        );
        // The scan sees the writers it is about (not vacuous).
        let with = |f: &dyn Fn(&crate::source_scan::ast::Source) -> bool| -> Vec<&str> {
            sources
                .iter()
                .filter(|s| f(s))
                .map(|s| s.rel.as_str())
                .collect()
        };
        let admitted_writers = with(&|s| admitted_writes(s) > 0);
        for expected in [
            "src/replication/event_consumer.rs",
            "src/replication/mirror_delete.rs",
            "src/replication/worker.rs",
            "src/lifecycle/worker.rs",
        ] {
            assert!(
                admitted_writers.contains(&expected),
                "the scan missed {expected}: {admitted_writers:?}"
            );
        }
        let request_writers = with(&|s| !writer_calls(s).is_empty());
        for expected in ["src/api/admin/objects.rs", "src/s3_adapter_s3s/object.rs"] {
            assert!(
                request_writers.contains(&expected),
                "the scan missed {expected}: {request_writers:?}"
            );
        }
        // A background writer that writes directly or counts bare is caught,
        // wherever its admission is; a request writer without a count too.
        let fixture = [
            crate::source_scan::ast::parse(
                "src/replication/x.rs",
                "async fn f(e: E, g: G) { let _w = g.admit_background_write(b); \
                 crate::transfer::copy_object_from(e, r, None).await; }",
            ),
            crate::source_scan::ast::parse(
                "src/lifecycle/y.rs",
                "async fn f(e: E, g: G) { let _w = g.begin_write(b); \
                 admitted::delete_batch(&p, e, b, v, h).await; }",
            ),
            crate::source_scan::ast::parse(
                "src/replication/ok.rs",
                "async fn f(e: E, g: G) { let p = admit_all(g, &[b])?; \
                 admitted::copy_known(&p, e, r, None, None).await; }",
            ),
            crate::source_scan::ast::parse(
                "src/api/admin/z.rs",
                "async fn f(e: E) { e.delete_batch(b, v, h).await; }",
            ),
            // An aliased import and the UFCS form are writers too.
            crate::source_scan::ast::parse(
                "src/replication/alias.rs",
                "use crate::transfer::copy_object_known as cp; \
                 async fn f(e: E, g: G) { let _p = admit_all(g, &[b])?; cp(e, r, None, None).await; }",
            ),
            crate::source_scan::ast::parse(
                "src/lifecycle/ufcs.rs",
                "async fn f(e: E, g: G) { let _p = admit_all(g, &[b])?; \
                 DynEngine::delete_batch(&e, b, v, h).await; }",
            ),
            // The count in ANOTHER function of the module does not cover
            // the write.
            crate::source_scan::ast::parse(
                "src/api/admin/w.rs",
                "async fn count(g: G) { let _w = g.begin_write(b); } \
                 async fn f(e: E) { e.delete_batch(b, v, h).await; }",
            ),
            // A literal `None` gate for a bucket is an uncounted proof; an
            // admission of no bucket is not.
            crate::source_scan::ast::parse(
                "src/replication/none.rs",
                "async fn f(e: E) { let p = admit_all(None, &buckets)?;\n\
                 let q = admit_all(None, &[\"b\"])?;\n let r = admit_all(None, &[])?; }",
            ),
        ];
        let caught = unadmitted(&fixture);
        let files: Vec<&str> = caught
            .iter()
            .map(|c| c.split(':').next().unwrap())
            .collect();
        assert_eq!(
            files,
            [
                "src/replication/x.rs",
                "src/lifecycle/y.rs",
                "src/api/admin/z.rs",
                "src/replication/alias.rs",
                "src/lifecycle/ufcs.rs",
                "src/api/admin/w.rs",
                "src/replication/none.rs",
            ],
            "{caught:?}"
        );
        assert!(
            caught[6].contains("[1, 2]"),
            "the two uncounted ones: {caught:?}"
        );
    }

    /// The admission counts BEFORE it asks: a refusal leaves no count, an
    /// admitted write holds one until its guard drops.
    #[test]
    fn admit_background_write_counts_then_checks() {
        let g = Arc::new(MaintenanceGate::new());
        let w = g.admit_background_write("b").expect("idle bucket");
        assert_eq!(g.inflight_writes("b"), 1);
        g.set_busy("b");
        let refusal = g.admit_background_write("B").unwrap_err();
        assert!(refusal.is_maintenance(), "{refusal:?}");
        assert_eq!(g.inflight_writes("b"), 1, "a refused write leaves no count");
        drop(w);
        assert_eq!(g.inflight_writes("b"), 0);
        g.clear("b");
        g.set_write_refusal_probe(Arc::new(|b: &str| (b == "w").then(|| "witness".into())));
        assert_eq!(
            g.admit_background_write("w").unwrap_err(),
            WriteRefusal::Refused("witness".into())
        );
        assert_eq!(g.inflight_writes("w"), 0);
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

    /// A re-encrypt job whose rewrites are all conditional gates nothing;
    /// one that is not (or a row of an older release) gates its bucket. A
    /// run that judges the backend again moves the row between the two.
    #[test]
    fn a_reencrypt_with_conditional_rewrites_gates_nothing() {
        use crate::maintenance::conversion::ReencryptParams;
        let db = JobsDb::in_memory("testpass").unwrap();
        let g = MaintenanceGate::new();
        let ungated = ReencryptParams {
            conditional_writes: true,
            ..Default::default()
        };
        let json = ungated.to_json();
        let id = db
            .maintenance_create_job("reencrypt", "cas", "counting", json.as_deref(), "a", 1)
            .unwrap()
            .unwrap();
        db.maintenance_create_job("reencrypt", "noncas", "counting", None, "a", 1)
            .unwrap()
            .unwrap();
        g.sync_from(&db);
        assert_eq!(busy(&g), vec!["noncas"]);
        db.maintenance_set_params(id, None).unwrap();
        g.sync_from(&db);
        assert_eq!(busy(&g), vec!["cas", "noncas"]);
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

    /// The H22 contract on the real drain: a background copy that holds an
    /// admission keeps `inflight_writes` above 0, so
    /// `drain_inflight_writes_within` WAITS until the admission drops. It
    /// cannot drain through and let the job rewrite the key mid-copy.
    #[tokio::test]
    async fn the_drain_waits_for_a_held_admission() {
        let g = Arc::new(MaintenanceGate::new());
        let admission = admit_all(Some(&g), &["b"]).unwrap();
        assert_eq!(g.inflight_writes("b"), 1);

        let drain = tokio::spawn({
            let g = Arc::clone(&g);
            async move {
                crate::maintenance::worker::drain_inflight_writes_within(
                    &g,
                    "b",
                    std::time::Duration::from_secs(30),
                )
                .await
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !drain.is_finished(),
            "the drain must block while a write is in flight"
        );

        drop(admission);
        tokio::time::timeout(std::time::Duration::from_secs(5), drain)
            .await
            .expect("the drain completes once the admission drops")
            .unwrap()
            .expect("drained");
        assert_eq!(g.inflight_writes("b"), 0);
    }

    /// A router around the gate: the middleware, and a handler that does
    /// what the s3s access hook does for every request. `entered` fires
    /// before the hook, `release` lets the handler go on; `seen` records
    /// `(admitted, in flight at the hook's verdict)`.
    fn gated_app(
        gate: Arc<MaintenanceGate>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        seen: Arc<parking_lot::Mutex<Vec<(bool, i64)>>>,
    ) -> axum::Router {
        axum::Router::new()
            .route(
                "/:bucket/:key",
                axum::routing::put({
                    let gate = gate.clone();
                    move |request: Request<Body>| {
                        let (gate, entered, release, seen) =
                            (gate.clone(), entered.clone(), release.clone(), seen.clone());
                        async move {
                            entered.notify_one();
                            release.notified().await;
                            let verdict = check_verified_request(
                                request.extensions(),
                                request.method(),
                                request.uri().path(),
                            );
                            seen.lock()
                                .push((verdict.is_ok(), gate.inflight_writes("b")));
                            "done"
                        }
                    }
                }),
            )
            .layer(axum::middleware::from_fn(maintenance_gate_middleware))
            .layer(axum::Extension(gate))
    }

    fn put_request() -> Request<Body> {
        Request::builder()
            .method(Method::PUT)
            .uri("/b/k")
            .body(Body::empty())
            .unwrap()
    }

    /// A write request counts as in flight only when it is verified AND
    /// admitted: a refused request (503 on a busy bucket) never counts, an
    /// admitted one counts until its handler ends. A request that has not
    /// reached the verified check yet counts as nothing.
    #[tokio::test]
    async fn a_write_request_counts_only_when_verified_and_admitted() {
        use tower::ServiceExt;
        let gate = Arc::new(MaintenanceGate::new());
        let (entered, release) = (
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tokio::sync::Notify::new()),
        );
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let app = gated_app(gate.clone(), entered.clone(), release.clone(), seen.clone());

        // Not yet verified: in flight as nothing.
        let first = tokio::spawn(app.clone().oneshot(put_request()));
        entered.notified().await;
        assert_eq!(gate.inflight_writes("b"), 0, "unverified is not in flight");
        release.notify_one();
        first.await.unwrap().unwrap();

        gate.set_busy("b");
        let second = tokio::spawn(app.clone().oneshot(put_request()));
        entered.notified().await;
        release.notify_one();
        second.await.unwrap().unwrap();

        assert_eq!(
            *seen.lock(),
            vec![(true, 1), (false, 0)],
            "(admitted, counted in flight) per request"
        );
        assert_eq!(gate.inflight_writes("b"), 0, "nothing leaks");
    }

    /// A caller that never finishes its request (a trickled head, a flood
    /// of bad signatures) cannot hold a drain: the gate is busy, the
    /// unverified request is in flight as nothing, and the drain completes.
    #[tokio::test]
    async fn an_unverified_request_does_not_hold_a_drain() {
        use tower::ServiceExt;
        let gate = Arc::new(MaintenanceGate::new());
        let (entered, release) = (
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tokio::sync::Notify::new()),
        );
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let app = gated_app(gate.clone(), entered.clone(), release, seen.clone());
        gate.set_busy("b");
        let pending = tokio::spawn(app.oneshot(put_request()));
        entered.notified().await;
        crate::maintenance::worker::drain_inflight_writes_within(
            &gate,
            "b",
            std::time::Duration::from_millis(200),
        )
        .await
        .expect("the drain completes with a request still unverified");
        pending.abort();
    }

    /// No instant exists where a verified write starts after the drain
    /// read zero: the count and the check are one step in the right order
    /// (count first). Writers loop on the real check while a job arms the
    /// gate and drains; a write that is admitted after the drain read zero
    /// is the failure.
    #[test]
    fn no_verified_write_starts_after_the_drain_read_zero() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        for _round in 0..300 {
            let gate = Arc::new(MaintenanceGate::new());
            let zero_seen = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let late = Arc::new(AtomicUsize::new(0));
            let writers: Vec<_> = (0..3)
                .map(|_| {
                    let (gate, zero_seen, stop, late) =
                        (gate.clone(), zero_seen.clone(), stop.clone(), late.clone());
                    std::thread::spawn(move || {
                        while !stop.load(Ordering::SeqCst) {
                            let mut ext = axum::http::Extensions::new();
                            ext.insert(gate.clone());
                            ext.insert(RequestWriteCount::default());
                            let admitted =
                                check_verified_request(&ext, &Method::PUT, "/b/k").is_ok();
                            if admitted && zero_seen.load(Ordering::SeqCst) {
                                late.fetch_add(1, Ordering::SeqCst);
                            }
                            // The write: held for a moment, then ends with ext.
                            std::hint::spin_loop();
                        }
                    })
                })
                .collect();
            std::thread::sleep(std::time::Duration::from_micros(200));
            gate.set_busy("b");
            while gate.inflight_writes("b") > 0 {
                std::hint::spin_loop();
            }
            zero_seen.store(true, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_micros(300));
            stop.store(true, Ordering::SeqCst);
            for w in writers {
                w.join().unwrap();
            }
            assert_eq!(late.load(Ordering::SeqCst), 0, "a write started after zero");
        }
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
