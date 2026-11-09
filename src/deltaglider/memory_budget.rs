// SPDX-License-Identifier: BUSL-1.1

//! The RAM budget of request bodies (`DGP_INGEST_MEMORY_MAX_BYTES`).
//!
//! A body at most the spool threshold is read into memory when this budget
//! has room for its declared length; otherwise it goes to a spool file
//! (see `deltaglider::ingest`). The budget NEVER waits: a full budget sends
//! the body to disk, so it cannot deadlock and never queues a small body
//! behind a large one. A permit lives inside one request's body and is
//! released when the body drops.
//!
//! One budget per process: `AppState` owns it, every engine (including one
//! rebuilt on a config reload) gets the same handle, so a reload never
//! grants a second budget. It is not a process `OnceLock`: tests build
//! their own, small or unbounded.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// The default of `DGP_INGEST_MEMORY_MAX_BYTES`: 1 GiB.
pub const DEFAULT_INGEST_MEMORY_MAX_BYTES: u64 = 1024 * 1024 * 1024;

const KIB: u64 = 1024;

/// A no-wait byte budget for request bodies in memory, counted in KiB.
#[derive(Clone)]
pub struct MemoryBudget(Arc<Inner>);

struct Inner {
    /// The budget, in KiB. `0`: every non-empty body takes the spool path.
    max_kib: u64,
    /// KiB held by live permits.
    held_kib: AtomicU64,
    /// The most KiB ever held at once.
    peak_kib: AtomicU64,
}

/// A share of the budget, held by one body in memory; released on drop.
pub struct MemoryPermit {
    budget: Arc<Inner>,
    kib: u64,
}

impl std::fmt::Debug for MemoryPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryPermit")
            .field("bytes", &self.bytes())
            .finish()
    }
}

impl MemoryPermit {
    /// The bytes this permit holds (its request, rounded up to KiB).
    pub fn bytes(&self) -> u64 {
        self.kib.saturating_mul(KIB)
    }
}

impl Drop for MemoryPermit {
    fn drop(&mut self) {
        self.budget.held_kib.fetch_sub(self.kib, Ordering::AcqRel);
    }
}

impl Default for MemoryBudget {
    /// [`DEFAULT_INGEST_MEMORY_MAX_BYTES`], without reading the environment.
    fn default() -> Self {
        Self::new(DEFAULT_INGEST_MEMORY_MAX_BYTES)
    }
}

impl MemoryBudget {
    /// A budget of `max_bytes` (rounded up to KiB). `0` grants only empty
    /// bodies.
    pub fn new(max_bytes: u64) -> Self {
        Self(Arc::new(Inner {
            max_kib: max_bytes.div_ceil(KIB),
            held_kib: AtomicU64::new(0),
            peak_kib: AtomicU64::new(0),
        }))
    }

    /// The budget from `DGP_INGEST_MEMORY_MAX_BYTES`. Read once, where the
    /// process builds its `AppState`; never on the request path.
    pub fn from_env() -> Self {
        Self::new(crate::config::env_parse_with_default(
            "DGP_INGEST_MEMORY_MAX_BYTES",
            DEFAULT_INGEST_MEMORY_MAX_BYTES,
        ))
    }

    /// A budget that grants every request (tests).
    pub fn unbounded() -> Self {
        Self::new(u64::MAX)
    }

    /// `bytes` of the budget now, or `None`. Never waits. An empty body
    /// costs nothing and is always granted: it has no bytes to put on disk.
    pub fn try_reserve(&self, bytes: u64) -> Option<MemoryPermit> {
        let inner = &self.0;
        let kib = bytes.div_ceil(KIB);
        if inner.max_kib == 0 && kib > 0 {
            return None;
        }
        let held = inner
            .held_kib
            .try_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                held.checked_add(kib).filter(|&n| n <= inner.max_kib)
            })
            .ok()?
            + kib;
        inner.peak_kib.fetch_max(held, Ordering::AcqRel);
        Some(MemoryPermit {
            budget: inner.clone(),
            kib,
        })
    }

    /// The budget, in bytes (a multiple of KiB).
    pub fn max_bytes(&self) -> u64 {
        self.0.max_kib.saturating_mul(KIB)
    }

    /// Bytes held by live permits now.
    pub fn held_bytes(&self) -> u64 {
        self.0.held_kib.load(Ordering::Acquire).saturating_mul(KIB)
    }

    /// The most bytes ever held at once.
    pub fn peak_bytes(&self) -> u64 {
        self.0.peak_kib.load(Ordering::Acquire).saturating_mul(KIB)
    }

    /// Do both handles draw on one budget?
    pub fn same_budget(&self, other: &MemoryBudget) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_until_full_then_never_waits() {
        let budget = MemoryBudget::new(10 * KIB);
        let a = budget.try_reserve(6 * KIB).unwrap();
        assert!(budget.try_reserve(5 * KIB).is_none(), "over the budget");
        let b = budget.try_reserve(4 * KIB).unwrap();
        assert_eq!(budget.held_bytes(), 10 * KIB);
        drop(a);
        assert_eq!(budget.held_bytes(), 4 * KIB);
        assert!(budget.try_reserve(6 * KIB).is_some());
        drop(b);
        assert_eq!(budget.held_bytes(), 0);
        assert_eq!(budget.peak_bytes(), 10 * KIB);
    }

    #[test]
    fn rounds_up_to_kib_and_zero_grants_only_empty_bodies() {
        let budget = MemoryBudget::new(2 * KIB);
        let p = budget.try_reserve(1).unwrap();
        assert_eq!(p.bytes(), KIB);
        assert!(budget.try_reserve(KIB + 1).is_none());
        let off = MemoryBudget::new(0);
        // An empty body costs nothing (a folder marker has no file).
        assert_eq!(off.try_reserve(0).map(|p| p.bytes()), Some(0));
        assert!(
            off.try_reserve(1).is_none(),
            "0 sends every other body to the spool"
        );
        assert!(MemoryBudget::unbounded().try_reserve(u64::MAX).is_some());
    }

    #[test]
    fn clones_share_one_budget() {
        let a = MemoryBudget::new(4 * KIB);
        let b = a.clone();
        let _p = a.try_reserve(4 * KIB).unwrap();
        assert!(b.try_reserve(1).is_none());
        assert!(a.same_budget(&b));
        assert!(!a.same_budget(&MemoryBudget::new(4 * KIB)));
    }
}

/// Model test: random sequences of reservations and releases, with the
/// invariants checked after every step. Every state change is one atomic
/// update, so a sequential model covers the interleavings.
#[cfg(test)]
mod memory_budget_model_tests {
    use super::*;
    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    enum Op {
        Try(u64),
        Release(usize),
    }

    fn op(max: u64) -> impl Strategy<Value = Op> {
        prop_oneof![
            (0..=max + 3 * KIB).prop_map(Op::Try),
            any::<usize>().prop_map(Op::Release),
        ]
    }

    proptest! {
        #[test]
        fn held_never_exceeds_max_and_drop_restores(
            (max, ops) in (0u64..64 * KIB).prop_flat_map(|max| {
                (Just(max), proptest::collection::vec(op(max), 0..80))
            })
        ) {
            let budget = MemoryBudget::new(max);
            let mut held: Vec<MemoryPermit> = Vec::new();
            let mut peak = 0;
            for op in ops {
                match op {
                    Op::Try(n) => {
                        let free = budget.max_bytes() - budget.held_bytes();
                        let got = budget.try_reserve(n);
                        // Never waits: granted exactly when it fits now.
                        prop_assert_eq!(
                            got.is_some(),
                            n.div_ceil(KIB) * KIB <= free && (max > 0 || n == 0),
                            "try {} with {} free", n, free
                        );
                        held.extend(got);
                    }
                    Op::Release(i) if !held.is_empty() => {
                        let i = i % held.len();
                        drop(held.swap_remove(i));
                    }
                    Op::Release(_) => {}
                }
                let sum: u64 = held.iter().map(MemoryPermit::bytes).sum();
                prop_assert_eq!(budget.held_bytes(), sum, "accounting drifted");
                prop_assert!(sum <= budget.max_bytes(), "held {} > max", sum);
                peak = peak.max(sum);
                prop_assert_eq!(budget.peak_bytes(), peak);
            }
            held.clear();
            prop_assert_eq!(budget.held_bytes(), 0, "budget leaked");
        }
    }
}
