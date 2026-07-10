// SPDX-License-Identifier: GPL-3.0-only

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
use axum::response::{IntoResponse, Response};
use dashmap::DashMap;

/// Lock-free busy-bucket set + per-bucket in-flight write counters.
#[derive(Debug)]
pub struct MaintenanceGate {
    busy: ArcSwap<HashSet<String>>,
    inflight_writes: DashMap<String, i64>,
}

impl Default for MaintenanceGate {
    fn default() -> Self {
        Self {
            busy: ArcSwap::from_pointee(HashSet::new()),
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

/// First path segment of an S3 request = the bucket (lowercased).
/// Root-level requests (ListBuckets, health) have no bucket.
fn bucket_from_path(path: &str) -> Option<String> {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let bucket = trimmed.split('/').next().unwrap_or(trimmed);
    if bucket.is_empty() {
        None
    } else {
        Some(bucket.to_ascii_lowercase())
    }
}

fn is_write_method(method: &Method) -> bool {
    matches!(*method, Method::PUT | Method::POST | Method::DELETE)
}

/// Axum middleware on the S3 router: 503-SlowDown writes to busy buckets,
/// count in-flight writes for everything else. Reads always pass.
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

    if gate.is_busy(&bucket) {
        // The busy set doesn't carry the job kind — name both candidates.
        return crate::api::errors::S3Error::SlowDown(format!(
            "bucket '{bucket}' is temporarily read-only while a background job \
             (re-encryption or migration) finishes — please retry shortly"
        ))
        .into_response();
    }

    // RAII: the guard decrements even if this future is dropped mid-body on a
    // client disconnect (straight-line write_finished would leak the counter).
    let _write = gate.begin_write(&bucket);
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!drain.is_finished(), "drain must block while a write is in flight");

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
        assert_eq!(
            bucket_from_path("/bucket/key/with/slashes"),
            Some("bucket".into())
        );
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
