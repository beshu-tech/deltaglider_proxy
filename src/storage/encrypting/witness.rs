// SPDX-License-Identifier: BUSL-1.1

//! The node-local gate of the multi-instance encryption witness (issue
//! #116 P3): which stored buckets this node refuses to write. The witness
//! poller (`coordination::encryption_witness`) sets it; every encrypting
//! wrapper of an engine lineage consults it before a write.

use arc_swap::ArcSwap;
use std::sync::Arc;

/// The stored buckets whose writes this node refuses (the multi-instance
/// encryption witness, issue #116 P3). Indexed by `(raw backend, real
/// bucket)`; the bucket `*` stands for every bucket on the backend that has
/// no entry of its own. Inactive (refuses nothing) on a single instance.
#[derive(Default)]
pub struct WitnessGate {
    state: ArcSwap<WitnessGateState>,
    /// Unix ms of the last successful sync; 0 = never.
    last_ok_ms: std::sync::atomic::AtomicI64,
    /// How long a sync verdict holds; 0 = gate inactive.
    ttl_ms: std::sync::atomic::AtomicI64,
}

/// What the witness decided at its last judgement.
#[derive(Default, Clone, Debug)]
pub struct WitnessGateState {
    /// Storages this node refuses, with the refusal message.
    pub refused: std::collections::HashMap<(String, String), String>,
    /// Storages the witness covers: when the last successful sync is older
    /// than the TTL, writes to them are refused (a stale verdict must not
    /// let this node write a form that the others may have changed).
    pub witnessed: std::collections::HashSet<(String, String)>,
}

/// The bucket name that stands for a backend's buckets with no entry.
pub const WITNESS_BACKEND_DEFAULT: &str = "*";

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl WitnessGate {
    /// Turn the gate on with a verdict lifetime of `ttl`. Until the first
    /// successful sync, every witnessed storage is refused (fail closed).
    pub fn activate(&self, ttl: std::time::Duration) {
        use std::sync::atomic::Ordering;
        self.ttl_ms
            .store((ttl.as_millis() as i64).max(1), Ordering::Relaxed);
    }

    /// Replace the judgement.
    pub fn set(&self, state: WitnessGateState) {
        self.state.store(Arc::new(state));
    }

    /// Record a successful sync (the verdict is fresh from now).
    pub fn synced(&self) {
        self.synced_at(unix_ms());
    }

    /// Record a successful sync that STARTED at `started_ms` (unix ms): the
    /// verdict's age counts from there, so a slow sync does not stretch it.
    pub fn synced_at(&self, started_ms: i64) {
        self.last_ok_ms
            .store(started_ms, std::sync::atomic::Ordering::Relaxed);
    }

    /// Pure lookup of `(backend, bucket)` in `map`, with the backend's `*`
    /// entry standing for a bucket that has none of its own.
    fn lookup<'a, V>(
        map: impl Fn(&(String, String)) -> Option<&'a V>,
        has_own: impl Fn(&(String, String)) -> bool,
        backend: &str,
        bucket: &str,
    ) -> Option<&'a V> {
        let own = (backend.to_string(), bucket.to_string());
        if has_own(&own) {
            return map(&own);
        }
        map(&(backend.to_string(), WITNESS_BACKEND_DEFAULT.to_string()))
    }

    /// The refusal of a write to `bucket` on `backend`, if any.
    pub fn refusal(&self, backend: &str, bucket: &str) -> Option<String> {
        use std::sync::atomic::Ordering;
        let ttl = self.ttl_ms.load(Ordering::Relaxed);
        if ttl == 0 {
            return None;
        }
        let st = self.state.load();
        let key = |b: &str| (backend.to_string(), b.to_string());
        let own = key(bucket);
        let witnessed_own = st.witnessed.contains(&own);
        let has_own = |k: &(String, String)| st.refused.contains_key(k) || st.witnessed.contains(k);
        if let Some(msg) = Self::lookup(|k| st.refused.get(k), has_own, backend, bucket) {
            return Some(msg.clone());
        }
        let witnessed = witnessed_own || st.witnessed.contains(&key(WITNESS_BACKEND_DEFAULT));
        let age = unix_ms() - self.last_ok_ms.load(Ordering::Relaxed);
        (witnessed && age > ttl).then(|| {
            format!(
                "bucket '{bucket}' on backend '{backend}': this instance cannot confirm that \
                 its encryption of the bucket agrees with the other instances (no successful \
                 check of the coordination bucket for {}s), so it refuses writes to the bucket \
                 until a check succeeds. Reads keep working",
                age / 1000
            )
        })
    }
}
