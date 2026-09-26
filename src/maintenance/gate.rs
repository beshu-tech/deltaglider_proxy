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

use std::collections::HashSet;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Method, Request};
use axum::middleware::Next;
use axum::response::Response;
use dashmap::DashMap;

/// Lock-free busy-bucket set (reads) + per-bucket in-flight write counters.
#[derive(Debug)]
pub struct MaintenanceGate {
    busy: ArcSwap<HashSet<String>>,
    /// Serialises the set_busy/clear read-modify-write on `busy`. Without it two
    /// concurrent writers (worker clear("a") + admin set_busy("b")) both clone
    /// the same {a} snapshot and the last store wins → a finished bucket A is
    /// stuck busy forever, 503-ing every client write (H36). Reads stay lock-free.
    write_lock: parking_lot::Mutex<()>,
    inflight_writes: DashMap<String, i64>,
}

impl Default for MaintenanceGate {
    fn default() -> Self {
        Self {
            busy: ArcSwap::from_pointee(HashSet::new()),
            write_lock: parking_lot::Mutex::new(()),
            inflight_writes: DashMap::new(),
        }
    }
}

impl MaintenanceGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Is this bucket currently gated? (Bucket names compare lowercased —
    /// the same normalization the routing layer uses.)
    pub fn is_busy(&self, bucket: &str) -> bool {
        self.busy.load().contains(&bucket.to_ascii_lowercase())
    }

    /// Add a bucket to the busy set (idempotent).
    pub fn set_busy(&self, bucket: &str) {
        let key = bucket.to_ascii_lowercase();
        // Hold write_lock across the RMW so a concurrent clear() can't clobber it.
        let _g = self.write_lock.lock();
        let current = self.busy.load();
        if current.contains(&key) {
            return;
        }
        let mut next = HashSet::clone(&current);
        next.insert(key);
        self.busy.store(Arc::new(next));
    }

    /// Remove a bucket from the busy set (idempotent).
    pub fn clear(&self, bucket: &str) {
        let key = bucket.to_ascii_lowercase();
        let _g = self.write_lock.lock();
        let current = self.busy.load();
        if !current.contains(&key) {
            return;
        }
        let mut next = HashSet::clone(&current);
        next.remove(&key);
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
    // The busy set doesn't carry the job kind — name both candidates.
    gate.is_busy(&bucket).then(|| {
        crate::api::errors::S3Error::SlowDown(format!(
            "bucket '{bucket}' is temporarily read-only while a background job \
             (re-encryption or migration) finishes — please retry shortly"
        ))
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_set_busy_and_clear_do_not_strand_a_bucket() {
        // H36: worker clear("a") racing admin set_busy("b") must not leave A
        // stuck busy (unsynchronized RMW would let the last store clobber the
        // other's change). Hammer the race; A must always end cleared, B set.
        use std::sync::Arc;
        for _ in 0..200 {
            let g = Arc::new(MaintenanceGate::new());
            g.set_busy("a"); // A starts busy (a running job)
            let g1 = Arc::clone(&g);
            let g2 = Arc::clone(&g);
            let t1 = std::thread::spawn(move || g1.clear("a")); // job A finishes
            let t2 = std::thread::spawn(move || g2.set_busy("b")); // job B arms
            t1.join().unwrap();
            t2.join().unwrap();
            assert!(!g.is_busy("a"), "A must be cleared, not stranded busy");
            assert!(g.is_busy("b"), "B must be busy");
        }
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
