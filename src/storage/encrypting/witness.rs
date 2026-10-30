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
pub struct WitnessGate {
    state: ArcSwap<WitnessGateState>,
    /// [`WitnessGate::now_ms`] of the last successful sync; [`NEVER`] = none.
    last_ok_ms: std::sync::atomic::AtomicI64,
    /// How long a sync verdict holds; 0 = gate inactive.
    ttl_ms: std::sync::atomic::AtomicI64,
}

/// `last_ok_ms` before the first successful sync.
const NEVER: i64 = i64::MIN;

impl Default for WitnessGate {
    fn default() -> Self {
        Self {
            state: Default::default(),
            last_ok_ms: std::sync::atomic::AtomicI64::new(NEVER),
            ttl_ms: Default::default(),
        }
    }
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

impl WitnessGate {
    /// Now, in ms on the monotonic clock of this process. Every age of a
    /// verdict is judged on it: a wall-clock step (NTP, an operator) must
    /// neither keep a stale verdict alive nor refuse a fresh one.
    pub fn now_ms() -> i64 {
        static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        EPOCH
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_millis() as i64
    }

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
        self.synced_at(Self::now_ms());
    }

    /// Record a successful sync that STARTED at `started_ms` (a
    /// [`Self::now_ms`] reading): the verdict's age counts from there, so a
    /// slow sync does not stretch it.
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
        self.refusal_at(backend, bucket, Self::now_ms())
    }

    /// [`Self::refusal`] at `now_ms` (a [`Self::now_ms`] reading).
    fn refusal_at(&self, backend: &str, bucket: &str, now_ms: i64) -> Option<String> {
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
        let last_ok = self.last_ok_ms.load(Ordering::Relaxed);
        let age = now_ms.saturating_sub(last_ok);
        (witnessed && (last_ok == NEVER || age > ttl)).then(|| {
            let since = if last_ok == NEVER {
                "since this instance started".to_string()
            } else {
                format!("for {}s", age / 1000)
            };
            format!(
                "bucket '{bucket}' on backend '{backend}': this instance cannot confirm that \
                 its encryption of the bucket agrees with the other instances (no successful \
                 check of the coordination bucket {since}), so it refuses writes to the bucket \
                 until a check succeeds. Reads keep working"
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const TTL: Duration = Duration::from_secs(2);

    fn witnessed_gate() -> WitnessGate {
        let gate = WitnessGate::default();
        gate.activate(TTL);
        let mut st = WitnessGateState::default();
        st.witnessed
            .insert(("local-disk".into(), WITNESS_BACKEND_DEFAULT.into()));
        gate.set(st);
        gate
    }

    /// Fail closed before the first sync, even while the monotonic clock is
    /// still younger than the TTL (it starts near 0 with the process).
    #[test]
    fn never_synced_refuses_from_the_first_ms() {
        let gate = witnessed_gate();
        for now in [0, 1, 1_999, 10_000] {
            let msg = gate.refusal_at("local-disk", "releases", now);
            assert!(msg.is_some_and(|m| m.contains("since this instance started")));
        }
    }

    #[test]
    fn a_verdict_holds_for_the_ttl_from_the_sync_start() {
        let gate = witnessed_gate();
        gate.synced_at(5_000);
        assert!(gate.refusal_at("local-disk", "releases", 5_000).is_none());
        assert!(gate.refusal_at("local-disk", "releases", 7_000).is_none());
        let msg = gate.refusal_at("local-disk", "releases", 7_001).unwrap();
        assert!(msg.contains("for 2s"), "{msg}");
        assert!(gate.refusal_at("other", "releases", 99_000).is_none());
    }

    /// The age is judged on the process's monotonic clock, never on the
    /// wall clock: `now_ms` never goes back.
    #[test]
    fn now_ms_is_monotonic() {
        let a = WitnessGate::now_ms();
        std::thread::sleep(Duration::from_millis(5));
        let b = WitnessGate::now_ms();
        assert!(a >= 0 && b >= a + 5, "{a} {b}");
    }

    #[test]
    fn an_inactive_gate_refuses_nothing() {
        let gate = WitnessGate::default();
        assert!(gate.refusal_at("local-disk", "releases", 0).is_none());
    }
}
