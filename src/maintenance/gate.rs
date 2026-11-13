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
    /// The disk monitor's verdict (`disk_watch`): why a STORING write into
    /// a bucket is refused now (the disk of its filesystem backend is at
    /// level critical). A write that frees space never asks it
    /// ([`WriteKind::FreesSpace`]). Set once at boot.
    disk_refusal: std::sync::OnceLock<ProbeCell>,
    /// The job that arms each busy key (same keys as `busy`).
    busy_job: ArcSwap<HashMap<String, i64>>,
    /// Jobs that wait for DISK space, with the unix second at which the wait
    /// ends: while a job waits, a delete into its bucket is not refused (the
    /// only way to free space on that disk besides a cancel). Memory only: a
    /// restart arms everything again. The worker sets it
    /// ([`MaintenanceGate::mark_disk_deferred`]) and clears it before the job
    /// runs again ([`MaintenanceGate::clear_disk_deferred`]).
    disk_deferred: DashMap<i64, i64>,
}

/// Pure: the run-level verdict of one refusal ([`MaintenanceGate::run_blocked`]).
pub fn run_verdict(refusal: Option<WriteRefusal>, replicate_deletes: bool) -> Option<String> {
    match refusal {
        None => None,
        Some(WriteRefusal::Disk(_)) if replicate_deletes => None,
        Some(other) => Some(other.into_reason()),
    }
}

/// What a background write does to the disk of its bucket. The disk
/// refusal ([`MaintenanceGate::set_disk_refusal_probe`]) applies to
/// [`WriteKind::Store`] only: reads and deletes must work at level critical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    /// Puts bytes on the disk: a copy, a transition, a rewrite.
    Store,
    /// Frees space or moves no bytes: a delete.
    FreesSpace,
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
            disk_refusal: std::sync::OnceLock::new(),
            busy_job: ArcSwap::from_pointee(HashMap::new()),
            disk_deferred: DashMap::new(),
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

/// Pure: the job that arms each key of [`busy_set`].
pub(crate) fn busy_jobs(arms: &[GateArm], unwinding: &HashSet<i64>) -> HashMap<String, i64> {
    arms.iter()
        .filter(|a| !(a.migrate_source && unwinding.contains(&a.job_id)))
        .map(|a| (a.key.to_ascii_lowercase(), a.job_id))
        .collect()
}

/// Pure: is this request one that frees space (a delete)? `DeleteObject`,
/// `DeleteBucket` and `AbortMultipartUpload` are `DELETE`; `DeleteObjects` is
/// `POST ?delete`. The rest of a write method stores bytes.
pub fn request_frees_space(method: &Method, path: &str, query: Option<&str>) -> bool {
    match *method {
        Method::DELETE => true,
        // As s3s routes it (`RequestTarget`): `?delete` on a BUCKET path only.
        Method::POST => crate::api::request_target::RequestTarget::parse(path, query)
            .is_ok_and(|target| target.is_batch_delete(method)),
        _ => false,
    }
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

    /// Is this bucket currently gated against a STORE? (Bucket names compare
    /// lowercased — the same normalization the routing layer uses.) A write
    /// that frees space asks [`Self::is_busy_for`]: a job that waits for disk
    /// space lets it pass.
    pub fn is_busy(&self, bucket: &str) -> bool {
        self.busy.load().contains_key(&bucket.to_ascii_lowercase())
    }

    /// [`Self::is_busy`] for a write of `kind`: THE per-item question of the
    /// bulk loops, one rule with the page check of a delete job
    /// ([`Self::write_blocked`]) and the request gate
    /// ([`write_gate_refusal_q`]): a write that frees space passes a bucket
    /// whose job waits for disk space.
    pub fn is_busy_for(&self, bucket: &str, kind: WriteKind) -> bool {
        self.busy_kind_for(bucket, kind).is_some()
    }

    /// Install the write-refusal probe: the encryption witness and the key
    /// sentinel (boot).
    pub fn set_write_refusal_probe(&self, probe: WriteRefusalProbe) {
        let _ = self.write_refusal.set(ProbeCell(probe));
    }

    /// Install the disk refusal probe (boot): see [`Self::store_blocked`].
    pub fn set_disk_refusal_probe(&self, probe: WriteRefusalProbe) {
        let _ = self.disk_refusal.set(ProbeCell(probe));
    }

    /// THE check of a background writer that FREES space or stores no
    /// bytes (a delete) before it writes into `bucket`: why it must stop as
    /// deferred (a maintenance job rewrites the bucket, or the encryption
    /// witness refuses this node's writes to it), or `None`. Memory only.
    /// It never reports a full disk. A writer that stores bytes asks
    /// [`Self::store_blocked`] (source guard
    /// `background_writers_ask_write_blocked`).
    pub fn write_blocked(&self, bucket: &str) -> Option<String> {
        self.blocked(bucket, WriteKind::FreesSpace)
    }

    /// [`Self::write_blocked`] for a writer that stores bytes (a copy, a
    /// transition): it also stops while the disk of the bucket's filesystem
    /// backend is at level critical.
    pub fn store_blocked(&self, bucket: &str) -> Option<String> {
        self.blocked(bucket, WriteKind::Store)
    }

    /// [`Self::write_blocked`] or [`Self::store_blocked`] by `kind`.
    pub fn blocked(&self, bucket: &str, kind: WriteKind) -> Option<String> {
        self.refusal_of(bucket, kind).map(WriteRefusal::into_reason)
    }

    /// Why a replication RUN into `bucket` must not start (or must stop) as
    /// deferred, or `None`. A maintenance job or the encryption witness
    /// defers the whole run. A full disk (level critical) defers it only when
    /// the rule has nothing but copies to do: a rule that replicates deletes
    /// still has work that frees space, so its run goes on, and each COPY
    /// is refused on its own ([`admit_stores`], the walk's
    /// `Event::CopyDeferred`).
    pub fn run_blocked(&self, bucket: &str, replicate_deletes: bool) -> Option<String> {
        run_verdict(self.refusal_of(bucket, WriteKind::Store), replicate_deletes)
    }

    /// [`Self::blocked`], with the cause kept apart.
    fn refusal_of(&self, bucket: &str, kind: WriteKind) -> Option<WriteRefusal> {
        if let Some(job) = self.busy_kind_for(bucket, kind) {
            return Some(WriteRefusal::Maintenance(format!(
                "bucket '{bucket}' has an active maintenance job ({job})"
            )));
        }
        let ask =
            |cell: &std::sync::OnceLock<ProbeCell>| cell.get().and_then(|probe| (probe.0)(bucket));
        ask(&self.write_refusal)
            .map(WriteRefusal::Refused)
            .or_else(|| match kind {
                WriteKind::Store => ask(&self.disk_refusal).map(WriteRefusal::Disk),
                WriteKind::FreesSpace => None,
            })
    }

    /// THE admission of one background write that STORES bytes (a
    /// replication copy) into `bucket`: count it as in flight FIRST, then
    /// ask [`Self::store_blocked`]. A maintenance job that arms at any
    /// moment either sees the count (its drain waits for the write) or the
    /// write sees the job (the guard drops, the caller defers). Checking
    /// first and counting after left a window where the drain saw 0 and
    /// the write still landed (F9). Hold the guard until the write ends.
    /// Source guard `background_writers_are_admitted`.
    pub fn admit_background_write(
        self: &Arc<Self>,
        bucket: &str,
    ) -> Result<Admitted, WriteRefusal> {
        admit_stores(Some(self), &[bucket])
    }

    /// [`Self::admit_background_write`] for an action of a known kind: the
    /// event consumer asks it per action, so a delete (it frees space) is
    /// never refused for a full disk.
    pub fn admit_background(
        self: &Arc<Self>,
        bucket: &str,
        kind: WriteKind,
    ) -> Result<Admitted, WriteRefusal> {
        admit_buckets(Some(self), &[(bucket, kind)])
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
        self.busy_job.store(Arc::new(busy_jobs(&arms, &unwinding)));
    }

    /// The job `job_id` waits for disk space until `until` (unix seconds):
    /// deletes into its bucket pass meanwhile. See `disk_deferred`.
    pub fn mark_disk_deferred(&self, job_id: i64, until: i64) {
        self.disk_deferred.insert(job_id, until);
    }

    /// The job runs again (or is gone): the gate is whole.
    pub fn clear_disk_deferred(&self, job_id: i64) {
        self.disk_deferred.remove(&job_id);
    }

    /// Did the job wait for disk space, whatever the time now? A job is
    /// claimed only AFTER its wait ended (`until` is past), so this ignores
    /// the time: the deletes that the wait admitted may still be running.
    pub fn has_disk_mark(&self, job_id: i64) -> bool {
        self.disk_deferred.contains_key(&job_id)
    }

    /// Close the wait NOW (a delete is refused from this moment) but keep the
    /// mark: it ends only after the deletes that the wait admitted were
    /// drained ([`Self::clear_disk_deferred`]).
    pub fn close_disk_deferred(&self, job_id: i64) {
        if let Some(mut until) = self.disk_deferred.get_mut(&job_id) {
            *until = 0;
        }
    }

    /// Does the job wait for disk space now?
    pub fn is_disk_deferred(&self, job_id: i64) -> bool {
        self.disk_deferred
            .get(&job_id)
            .is_some_and(|until| *until > crate::maintenance::store::current_unix_seconds())
    }

    /// [`Self::busy_kind`] for a write of `kind`: a job that waits for disk
    /// space does not hold back a write that frees space.
    pub fn busy_kind_for(&self, bucket: &str, kind: WriteKind) -> Option<String> {
        let key = bucket.to_ascii_lowercase();
        let busy = self.busy.load().get(&key).cloned()?;
        if kind == WriteKind::FreesSpace {
            let waiting = self
                .busy_job
                .load()
                .get(&key)
                .is_some_and(|id| self.is_disk_deferred(*id));
            if waiting {
                return None;
            }
        }
        Some(busy)
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
        // SeqCst fence: a drain arms the gate (`busy` store), then reads the
        // counts; a request counts, then reads `busy`
        // ([`check_verified_request`], with the same fence). Each side
        // writes one location and reads the other, so without both fences
        // each could miss the other's write. With them, one side sees the
        // other: the drain sees the count, or the request sees the gate.
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
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
///
/// `admit_all` is for writes that free space or store no bytes (deletes):
/// it never reports a full disk. [`admit_stores`] is for writes that store
/// bytes; [`admit_buckets`] takes the kind per bucket.
pub fn admit_all(
    gate: Option<&Arc<MaintenanceGate>>,
    buckets: &[&str],
) -> Result<Admitted, WriteRefusal> {
    let kinds: Vec<(&str, WriteKind)> = buckets
        .iter()
        .map(|b| (*b, WriteKind::FreesSpace))
        .collect();
    admit_buckets(gate, &kinds)
}

/// [`admit_all`] for writes that STORE bytes into `buckets`: the gate also
/// refuses while the disk of a bucket's filesystem backend is at level
/// critical.
pub fn admit_stores(
    gate: Option<&Arc<MaintenanceGate>>,
    buckets: &[&str],
) -> Result<Admitted, WriteRefusal> {
    let kinds: Vec<(&str, WriteKind)> = buckets.iter().map(|b| (*b, WriteKind::Store)).collect();
    admit_buckets(gate, &kinds)
}

/// THE admission, with the kind of write per bucket (a lifecycle transition
/// stores into its destination and only deletes from its source).
pub fn admit_buckets(
    gate: Option<&Arc<MaintenanceGate>>,
    buckets: &[(&str, WriteKind)],
) -> Result<Admitted, WriteRefusal> {
    let mut guards = Vec::with_capacity(buckets.len());
    for (bucket, _) in buckets {
        guards.push(gate.map(|g| g.begin_background_write(bucket)));
    }
    if let Some(gate) = gate {
        if let Some(refusal) = buckets
            .iter()
            .find_map(|(b, kind)| gate.refusal_of(b, *kind))
        {
            return Err(refusal); // `guards` drop: no longer counted.
        }
    }
    Ok(Admitted {
        buckets: buckets.iter().map(|(b, _)| b.to_string()).collect(),
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
    /// The disk of the bucket's filesystem backend is at level critical: a
    /// copy waits, a delete does not (it frees space), so the writer defers
    /// the ITEM and goes on with what frees space.
    Disk(String),
}

impl WriteRefusal {
    pub fn reason(&self) -> &str {
        match self {
            WriteRefusal::Maintenance(r) | WriteRefusal::Refused(r) | WriteRefusal::Disk(r) => r,
        }
    }

    pub fn into_reason(self) -> String {
        match self {
            WriteRefusal::Maintenance(r) | WriteRefusal::Refused(r) | WriteRefusal::Disk(r) => r,
        }
    }

    pub fn is_maintenance(&self) -> bool {
        matches!(self, WriteRefusal::Maintenance(_))
    }

    /// A full disk and nothing else: the refusal of a copy only.
    pub fn is_disk(&self) -> bool {
        matches!(self, WriteRefusal::Disk(_))
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
    /// The proof of a write into `bucket`, if this count covers it
    /// (case-insensitive). A writer that takes a [`DestWrite`] cannot run
    /// outside the count of its destination bucket.
    pub fn proof_for<'a>(&'a self, bucket: &'a str) -> Option<DestWrite<'a>> {
        self.bucket
            .eq_ignore_ascii_case(bucket)
            .then_some(DestWrite { bucket })
    }
}

/// See [`WriteGuard::proof_for`].
#[derive(Clone, Copy)]
pub struct DestWrite<'a> {
    bucket: &'a str,
}

impl DestWrite<'_> {
    pub fn bucket(&self) -> &str {
        self.bucket
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
    write_gate_refusal_q(gate, method, path, None)
}

/// [`write_gate_refusal`] with the query, which tells `DeleteObjects` (`POST
/// ?delete`) from a write: a delete into a bucket whose job waits for disk
/// space is not refused ([`MaintenanceGate::busy_kind_for`]).
pub fn write_gate_refusal_q(
    gate: &MaintenanceGate,
    method: &Method,
    path: &str,
    query: Option<&str>,
) -> Option<crate::api::errors::S3Error> {
    if !is_write_method(method) {
        return None;
    }
    let bucket = bucket_from_path(path)?;
    let kind = if request_frees_space(method, path, query) {
        WriteKind::FreesSpace
    } else {
        WriteKind::Store
    };
    gate.busy_kind_for(&bucket, kind)
        .map(|job| crate::api::errors::S3Error::SlowDown(refusal_message(&bucket, &job)))
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
    check_verified_request_q(extensions, method, path, None)
}

/// [`check_verified_request`] with the query of the request (see
/// [`write_gate_refusal_q`]). A caller that has none gets the strict answer.
pub fn check_verified_request_q(
    extensions: &axum::http::Extensions,
    method: &Method,
    path: &str,
    query: Option<&str>,
) -> Result<(), crate::api::errors::S3Error> {
    // Count FIRST, then ask the gates (acquire-then-recheck, H37): a job
    // that arms later either sees this count or refuses the request.
    let count = extensions.get::<RequestWriteCount>();
    if let (Some(gate), true) = (
        extensions.get::<Arc<MaintenanceGate>>(),
        is_write_method(method),
    ) {
        if let Some(bucket) = bucket_from_path(path) {
            match count {
                Some(count) => count.admit(gate, &bucket),
                // Fail closed: a write that has no slot to be counted in
                // would be invisible to a drain.
                None => {
                    tracing::error!(
                        "write request to '{bucket}' has no in-flight count slot; refused"
                    );
                    return Err(crate::api::errors::S3Error::SlowDown(
                        "the request could not be counted; retry".to_string(),
                    ));
                }
            }
            // Pairs with the fence in `inflight_writes`: the count is
            // visible before the gate is read.
            std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        }
    }
    let verdict = verified_request_refusal(extensions, method, path, query);
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
    query: Option<&str>,
) -> Result<(), crate::api::errors::S3Error> {
    if let Some(gate) = extensions.get::<crate::coordination::health::BackendHealthGate>() {
        if let Some(refusal) = crate::coordination::health::reserved_bucket_refusal(gate, path) {
            return Err(refusal);
        }
    }
    if let Some(gate) = extensions.get::<Arc<MaintenanceGate>>() {
        if let Some(refusal) = write_gate_refusal_q(gate, method, path, query) {
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

    /// The run-level verdict: a full disk defers a run that only copies, and
    /// never a run that also deletes; every other refusal defers either.
    #[test]
    fn a_full_disk_defers_only_a_run_that_has_nothing_to_delete() {
        let disk = || Some(WriteRefusal::Disk("full".into()));
        let witness = || Some(WriteRefusal::Refused("witness".into()));
        let job = || Some(WriteRefusal::Maintenance("job".into()));
        assert_eq!(run_verdict(None, false), None);
        assert_eq!(run_verdict(None, true), None);
        assert_eq!(run_verdict(disk(), false).as_deref(), Some("full"));
        assert_eq!(run_verdict(disk(), true), None, "its deletes free space");
        for deletes in [false, true] {
            assert_eq!(run_verdict(witness(), deletes).as_deref(), Some("witness"));
            assert_eq!(run_verdict(job(), deletes).as_deref(), Some("job"));
        }
        // Through the gate.
        let g = MaintenanceGate::new();
        g.set_disk_refusal_probe(Arc::new(|b: &str| (b == "full").then(|| "full".into())));
        assert_eq!(g.run_blocked("full", false).as_deref(), Some("full"));
        assert_eq!(g.run_blocked("full", true), None);
        assert_eq!(g.run_blocked("roomy", false), None);
        // The refusal of a copy is a disk refusal, apart from the others.
        assert!(matches!(
            admit_stores(Some(&Arc::new(g)), &["full"]),
            Err(r) if r.is_disk()
        ));
    }

    fn arm(job_id: i64, key: &str, kind: &str) -> GateArm {
        GateArm {
            job_id,
            key: key.into(),
            kind: kind.into(),
            migrate_source: false,
        }
    }

    fn gate_with(arms: &[GateArm]) -> MaintenanceGate {
        let g = MaintenanceGate::new();
        g.busy.store(Arc::new(busy_set(arms, &HashSet::new())));
        g.busy_job.store(Arc::new(busy_jobs(arms, &HashSet::new())));
        g
    }

    /// Which requests a job that waits for DISK space lets through: the
    /// ones that free space (`DELETE`, `POST ?delete`), and only while the
    /// wait lasts and only into the bucket of that job. A write, a part, a
    /// `CompleteMultipartUpload` (a `POST` without `?delete`) stay refused.
    #[test]
    fn a_job_that_waits_for_disk_lets_deletes_pass_and_nothing_else() {
        let now = crate::maintenance::store::current_unix_seconds();
        let g = gate_with(&[arm(7, "b", "reencrypt"), arm(8, "other", "migrate")]);
        let refused = |m: &Method, path: &str, q: Option<&str>| {
            write_gate_refusal_q(&g, m, path, q).is_some()
        };
        let all = |expect_delete_refused: bool, label: &str| {
            assert_eq!(
                refused(&Method::DELETE, "/b/k", None),
                expect_delete_refused,
                "{label}: DeleteObject"
            );
            assert_eq!(
                refused(&Method::POST, "/b", Some("delete")),
                expect_delete_refused,
                "{label}: DeleteObjects"
            );
            assert!(refused(&Method::PUT, "/b/k", None), "{label}: PutObject");
            assert!(
                refused(&Method::PUT, "/b/k", Some("partNumber=1&uploadId=x")),
                "{label}: UploadPart"
            );
            assert!(
                refused(&Method::POST, "/b/k", Some("uploadId=x")),
                "{label}: Complete"
            );
            assert!(
                refused(&Method::POST, "/b/k", Some("uploads")),
                "{label}: Create"
            );
            assert!(!refused(&Method::GET, "/b/k", None), "{label}: a read");
        };
        // Not waiting: everything is refused.
        all(true, "armed");
        // Waiting for disk: deletes into `b` pass, nothing else does.
        g.mark_disk_deferred(7, now + 60);
        all(false, "waiting");
        assert!(g.write_blocked("b").is_none(), "a background delete passes");
        assert!(g.store_blocked("b").is_some(), "a background copy does not");
        assert!(
            refused(&Method::DELETE, "/other/k", None),
            "another job's bucket is still armed"
        );
        // The wait ends (its time passes): the gate is whole again.
        g.mark_disk_deferred(7, now - 1);
        all(true, "expired");
        // The job runs again: cleared.
        g.mark_disk_deferred(7, now + 60);
        g.clear_disk_deferred(7);
        all(true, "cleared");
    }

    #[test]
    fn the_frees_space_requests_by_table() {
        // (method, path, query, frees space): every shape of a POST and DELETE.
        let t = [
            (Method::DELETE, "/b/k", None, true),
            (Method::DELETE, "/b/k", Some("uploadId=x"), true),
            (Method::DELETE, "/b", None, true),
            // DeleteObjects: `?delete` on a bucket path.
            (Method::POST, "/b", Some("delete"), true),
            (Method::POST, "/b/", Some("delete"), true),
            (Method::POST, "/b", Some("delete="), true),
            (Method::POST, "/b", Some("x=1&delete"), true),
            // s3s reads these as writes: a part, a Complete, a Create.
            (Method::POST, "/b/k", Some("delete"), false),
            (Method::POST, "/b/k", Some("uploadId=X&delete"), false),
            (Method::POST, "/b/k", Some("uploadId=X"), false),
            (Method::POST, "/b/k", Some("uploads"), false),
            (Method::POST, "/b/k", Some("uploads&delete"), false),
            (Method::POST, "/b", Some("deleted"), false),
            (Method::POST, "/b", Some("uploads"), false),
            (Method::POST, "/b", None, false),
            (Method::POST, "/", Some("delete"), false),
            (Method::PUT, "/b", Some("delete"), false),
            (Method::PUT, "/b/k", Some("partNumber=1&uploadId=x"), false),
            (Method::GET, "/b", Some("delete"), false),
        ];
        for (m, path, q, want) in t {
            assert_eq!(request_frees_space(&m, path, q), want, "{m} {path} {q:?}");
        }
    }

    /// Reads and deletes must work at level critical: the disk refusal
    /// stops a write that STORES bytes, and no other.
    #[test]
    fn the_disk_refusal_stops_stores_and_never_a_delete() {
        let g = Arc::new(MaintenanceGate::new());
        g.set_disk_refusal_probe(Arc::new(|b: &str| {
            (b == "full").then(|| "the disk is full".into())
        }));
        // A delete asks no disk: every way of asking for a delete passes.
        assert_eq!(g.write_blocked("full"), None);
        assert_eq!(g.blocked("full", WriteKind::FreesSpace), None);
        assert!(admit_all(Some(&g), &["full"]).is_ok());
        assert!(admit_buckets(Some(&g), &[("full", WriteKind::FreesSpace)]).is_ok());
        // A store asks it.
        assert_eq!(g.store_blocked("full").as_deref(), Some("the disk is full"));
        assert_eq!(
            g.blocked("full", WriteKind::Store).as_deref(),
            Some("the disk is full")
        );
        assert_eq!(g.store_blocked("roomy"), None);
        assert!(matches!(
            admit_stores(Some(&g), &["roomy", "full"]),
            Err(WriteRefusal::Disk(m)) if m == "the disk is full"
        ));
        assert!(admit_stores(Some(&g), &["roomy"]).is_ok());
        assert!(
            g.admit_background_write("full").is_err(),
            "a copy is a store"
        );
        // A transition: it deletes from `full` and stores into `roomy`.
        assert!(admit_buckets(
            Some(&g),
            &[("full", WriteKind::FreesSpace), ("roomy", WriteKind::Store)]
        )
        .is_ok());
        // ... and the other way round stops.
        assert!(admit_buckets(
            Some(&g),
            &[("roomy", WriteKind::FreesSpace), ("full", WriteKind::Store)]
        )
        .is_err());
        // A refused admission counts nothing (the guards dropped).
        assert_eq!(g.inflight_background_writes("full"), 0);
        // A maintenance job wins for both kinds.
        let arm = GateArm {
            job_id: 1,
            key: "roomy".into(),
            kind: "reencrypt".into(),
            migrate_source: false,
        };
        g.busy.store(Arc::new(busy_set(&[arm], &HashSet::new())));
        for kind in [WriteKind::Store, WriteKind::FreesSpace] {
            assert!(matches!(
                g.refusal_of("roomy", kind),
                Some(WriteRefusal::Maintenance(_))
            ));
        }
    }

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

    /// Does the fn `name` of the source file `rel` take a `DestWrite`
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
        signature[..end].contains("DestWrite")
    }

    /// Request-scoped writers that take their count as a `DestWrite`
    /// parameter: the type proves it, the caller counts.
    const PROOF_PARAMETER: [(&str, &str, &str); 1] = [(
        "src/api/admin/objects.rs",
        "copy_one",
        "takes the bulk loop's DestWrite",
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
            .filter(|c| {
                !c.method
                    && (c.named("admit_all") || c.named("admit_stores") || c.named("admit_buckets"))
            })
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
            // takes the caller's `DestWrite` (listed in `PROOF_PARAMETER`).
            let names = [
                "admit_background_write",
                "admit_all",
                "admit_stores",
                "admit_buckets",
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

    /// A write request that has no count slot (it did not pass the
    /// middleware) fails closed: refused, never an uncounted write.
    #[test]
    fn a_write_without_a_count_slot_is_refused() {
        let gate = Arc::new(MaintenanceGate::new());
        let mut ext = axum::http::Extensions::new();
        ext.insert(gate.clone());
        assert!(check_verified_request(&ext, &Method::PUT, "/b/k").is_err());
        assert_eq!(gate.inflight_writes("b"), 0);
        // A read needs no slot; a write with a slot passes.
        assert!(check_verified_request(&ext, &Method::GET, "/b/k").is_ok());
        ext.insert(RequestWriteCount::default());
        assert!(check_verified_request(&ext, &Method::PUT, "/b/k").is_ok());
        assert_eq!(gate.inflight_writes("b"), 1);
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
