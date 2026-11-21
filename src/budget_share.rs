// SPDX-License-Identifier: BUSL-1.1

//! THE rule for a budget that several principals share: read spool space,
//! open multipart uploads, multipart part bytes.
//!
//! A principal alone may use most of the cap, and every other principal can
//! always reach its own share while the global cap has room. A request of
//! `n` by principal P is admitted when EITHER
//!
//! - (A) P's own use plus `n` is at most the share (`share`% of the cap), OR
//! - (B) the total use of all principals plus `n` is at most the rest
//!   (`100 - share`% of the cap).
//!
//! With the default share of 25, P alone can use 75 %, and the last 25 % is
//! reachable by every other principal through (A). A budget that can hold a
//! single request larger than (B) allows (a read of one huge file) adds a
//! third clause (C): P holds nothing and nobody else holds anything.
//!
//! The function does not look at the global cap: the caller checks it too.

/// The two limits that decide [`Shares::admits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shares {
    /// (A): the use of one principal that is always admitted.
    own_cap: u64,
    /// (B): the use of all principals that admits a principal over its share.
    shared_cap: u64,
}

impl Shares {
    /// The limits for a budget `cap` and a share of `percent` (1-100; 100
    /// turns the shares off). `at_least` is the smallest share (one unit of
    /// the budget: a request must be able to fit).
    pub fn new(cap: u64, percent: u8, at_least: u64) -> Self {
        let percent = u128::from(percent.clamp(1, 100));
        let of = |p: u128| (u128::from(cap) * p / 100) as u64;
        Self {
            own_cap: of(percent).max(at_least).min(cap.max(at_least)),
            shared_cap: of(100 - percent),
        }
    }

    /// (A): what one principal may always use.
    pub fn own_cap(&self) -> u64 {
        self.own_cap
    }

    /// (B): the use of all principals up to which a principal may pass its
    /// own share.
    pub fn shared_cap(&self) -> u64 {
        self.shared_cap
    }

    /// The most that one principal reaches alone, with no clause (C):
    /// the larger of (A) and (B).
    pub fn lone_reach(&self) -> u64 {
        self.own_cap.max(self.shared_cap)
    }

    /// Pure: is a request of `n` admitted for a principal that uses `own`
    /// now, when all principals together use `total` (`total` includes
    /// `own`)? `alone_ok`: clause (C) applies.
    pub fn admits(&self, own: u64, total: u64, n: u64, alone_ok: bool) -> bool {
        own.saturating_add(n) <= self.own_cap
            || total.saturating_add(n) <= self.shared_cap
            || (alone_ok && own == 0 && total == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashMap;

    #[test]
    fn the_default_share_lets_one_principal_use_three_quarters() {
        let s = Shares::new(100, 25, 1);
        assert_eq!((s.own_cap(), s.shared_cap(), s.lone_reach()), (25, 75, 75));
        // Alone: up to 75, not 76.
        assert!(s.admits(0, 0, 75, false));
        assert!(s.admits(70, 70, 5, false));
        assert!(!s.admits(70, 70, 6, false));
        // Above its share (30) with the others at 30: up to a total of 75, no further.
        assert!(s.admits(30, 60, 15, false));
        assert!(!s.admits(30, 60, 16, false));
        // A principal within its own share is admitted at any total.
        assert!(s.admits(0, 100, 25, false));
        assert!(s.admits(20, 100, 5, false));
        assert!(!s.admits(20, 100, 6, false));
    }

    #[test]
    fn clause_c_needs_an_idle_budget_and_is_off_for_multipart() {
        let s = Shares::new(100, 25, 1);
        assert!(!s.admits(0, 0, 90, false));
        assert!(s.admits(0, 0, 90, true));
        assert!(!s.admits(0, 1, 90, true), "someone else holds space");
        assert!(!s.admits(5, 5, 90, true), "P holds space itself");
    }

    #[test]
    fn a_share_of_100_is_the_whole_budget_and_small_caps_keep_one_unit() {
        let s = Shares::new(100, 100, 1);
        assert_eq!((s.own_cap(), s.shared_cap()), (100, 0));
        assert!(s.admits(50, 50, 50, false));
        assert!(!s.admits(50, 50, 51, false));
        let tiny = Shares::new(3, 25, 1);
        assert_eq!(tiny.own_cap(), 1, "at least one unit");
        assert_eq!(Shares::new(u64::MAX, 100, 1).own_cap(), u64::MAX);
        // The floor never lifts the share above the cap.
        assert_eq!(Shares::new(4, 25, 10).own_cap(), 10);
    }

    proptest! {
        /// (i) The function never refuses a principal within its own share.
        #[test]
        fn a_principal_within_its_share_is_never_refused(
            cap in 1u64..10_000, percent in 1u8..=100,
            own in 0u64..10_000, others in 0u64..10_000, n in 0u64..10_000,
        ) {
            let s = Shares::new(cap, percent, 1);
            if own.saturating_add(n) <= s.own_cap() {
                prop_assert!(s.admits(own, own + others, n, false));
            }
        }
    }

    #[derive(Debug, Clone)]
    enum Op {
        Take(usize, u64),
        Give(usize, u64),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0usize..4, 1u64..40).prop_map(|(p, n)| Op::Take(p, n)),
            (0usize..4, 1u64..40).prop_map(|(p, n)| Op::Give(p, n)),
        ]
    }

    /// The model of a caller: the share function AND the global cap.
    struct Model {
        cap: u64,
        shares: Shares,
        held: HashMap<usize, u64>,
    }

    impl Model {
        fn total(&self) -> u64 {
            self.held.values().sum()
        }
        fn take(&mut self, p: usize, n: u64, alone_ok: bool) -> bool {
            let own = self.held.get(&p).copied().unwrap_or(0);
            let ok =
                self.total() + n <= self.cap && self.shares.admits(own, self.total(), n, alone_ok);
            if ok {
                *self.held.entry(p).or_default() += n;
            }
            ok
        }
        fn give(&mut self, p: usize, n: u64) {
            let own = self.held.get(&p).copied().unwrap_or(0);
            let back = n.min(own);
            if back > 0 {
                *self.held.get_mut(&p).unwrap() -= back;
            }
        }
        /// What the principals hold above their own share.
        fn excess(&self) -> u64 {
            self.held
                .values()
                .map(|h| h.saturating_sub(self.shares.own_cap()))
                .sum()
        }
    }

    proptest! {
        /// With any sequence of requests and releases by 4 principals, what is
        /// held above the shares never exceeds (B): the rest of the cap stays
        /// for principals that are within their own share, and one of them is
        /// refused only when the global cap is full.
        #[test]
        fn what_is_held_above_the_shares_stays_below_the_rest(
            cap in 10u64..200, percent in 1u8..=100,
            ops in proptest::collection::vec(op(), 0..120),
        ) {
            let shares = Shares::new(cap, percent, 1);
            let mut m = Model { cap, shares, held: HashMap::new() };
            for op in ops {
                match op {
                    Op::Take(p, n) => {
                        let own = m.held.get(&p).copied().unwrap_or(0);
                        let total = m.total();
                        let got = m.take(p, n, false);
                        // Refused within its own share: only the global cap says no.
                        if !got && own + n <= shares.own_cap() {
                            prop_assert!(total + n > cap);
                        }
                    }
                    Op::Give(p, n) => m.give(p, n),
                }
                prop_assert!(m.excess() <= shares.shared_cap().max(shares.own_cap()),
                    "excess {} over {:?}", m.excess(), shares);
                prop_assert!(m.total() <= cap);
            }
        }

        /// One abuser (any sequence) never leaves a fresh principal without its
        /// share: the fresh one is admitted up to its own share, global cap
        /// included.
        #[test]
        fn a_lone_abuser_leaves_every_other_principal_its_share(
            cap in 20u64..400, percent in 1u8..=50,
            ops in proptest::collection::vec(op(), 0..120),
        ) {
            let shares = Shares::new(cap, percent, 1);
            let mut m = Model { cap, shares, held: HashMap::new() };
            for op in ops {
                match op {
                    Op::Take(_, n) => { m.take(0, n, false); }
                    Op::Give(_, n) => m.give(0, n),
                }
                prop_assert!(m.held.get(&0).copied().unwrap_or(0) <= shares.lone_reach(),
                    "alone above its reach");
            }
            let fresh = shares.own_cap();
            prop_assert!(m.take(1, fresh, false), "a fresh principal could not take its share");
        }

        /// (ii) One principal alone never exceeds the larger of its share and
        /// the rest, unless clause (C) lets one request through an idle budget.
        #[test]
        fn one_principal_alone_stays_within_its_reach(
            cap in 10u64..400, percent in 1u8..=100,
            ops in proptest::collection::vec((1u64..60, any::<bool>()), 0..80),
        ) {
            let shares = Shares::new(cap, percent, 1);
            let mut m = Model { cap, shares, held: HashMap::new() };
            for (n, give) in ops {
                if give { m.give(0, n) } else { m.take(0, n, false); }
                prop_assert!(m.total() <= shares.lone_reach().max(1));
            }
            // Clause C: one request through the idle budget, then nothing more.
            let mut idle = Model { cap, shares, held: HashMap::new() };
            let big = cap;
            prop_assert!(idle.take(0, big, true));
            prop_assert!(!idle.take(0, 1, true));
            prop_assert!(!idle.take(1, 1, true) || shares.admits(0, big, 1, false));
        }
    }
}
