// SPDX-License-Identifier: BUSL-1.1

use super::{hold_check, HoldCheck};
use crate::coordination::reference_lock::{
    plan_lock_acquire, plan_lock_renew, LockAction, Observed, RefLock, RenewLockAction,
};
use proptest::prelude::*;
use std::time::Duration;

const CLIENTS: usize = 3;

#[derive(Debug, Clone)]
enum Op {
    /// Read step of an acquire / renew / release, by client `c`.
    Read(usize),
    /// Conditional-write step of what `c` read for.
    Write(usize),
    /// A commit attempt by `c`: the guard writes only when it trusts.
    Commit(usize),
    /// Drop `c`'s guard (starts a release).
    Drop(usize),
    /// Server time moves on.
    Tick(i64),
}

#[derive(Debug, Clone)]
struct Stored {
    lock: RefLock,
    etag: u64,
    written_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
enum Step {
    Acquire,
    Renew,
    Release,
}

#[derive(Debug, Clone, Default)]
struct Client {
    /// Holding (guard alive): time of the last confirmed read.
    confirmed_at: Option<i64>,
    lost: bool,
    /// A read awaiting its write: (step, what it saw, when).
    in_flight: Option<(Step, Option<Stored>, i64)>,
    owner_seq: u64,
}

struct World {
    ttl: i64,
    now: i64,
    obj: Option<Stored>,
    etags: u64,
    clients: Vec<Client>,
}

impl World {
    fn owner(&self, c: usize) -> String {
        format!("c{c}-{}", self.clients[c].owner_seq)
    }

    fn observed(&self, seen: &Option<Stored>) -> Observed {
        match seen {
            None => Observed::Absent,
            Some(s) => {
                let mut lock = s.lock.clone();
                // Expiry by server age, as `read_lock` re-bases it.
                lock.expires_at = s.written_at + self.ttl;
                Observed::Held {
                    lock,
                    etag: s.etag.to_string(),
                }
            }
        }
    }

    fn put(&mut self, owner: String, epoch: u64) {
        self.etags += 1;
        self.obj = Some(Stored {
            lock: RefLock {
                owner,
                node_id: "n".into(),
                epoch,
                expires_at: self.now + self.ttl,
                ttl_secs: Some(self.ttl),
            },
            etag: self.etags,
            written_at: self.now,
        });
    }

    fn etag_is(&self, etag: &str) -> bool {
        self.obj.as_ref().map(|s| s.etag.to_string()).as_deref() == Some(etag)
    }

    fn renew_interval(&self) -> Duration {
        Duration::from_millis(self.ttl as u64 * 1000 / 4)
    }

    fn trusts(&self, c: usize) -> bool {
        let cl = &self.clients[c];
        match cl.confirmed_at {
            Some(at) => {
                let age = Duration::from_secs((self.now - at).max(0) as u64);
                hold_check(cl.lost, age, self.renew_interval()) == HoldCheck::Trust
            }
            None => false,
        }
    }

    fn apply(&mut self, op: Op) {
        match op {
            Op::Tick(dt) => self.now += dt,
            Op::Read(c) => {
                let cl = &self.clients[c];
                if cl.in_flight.is_some() {
                    return; // one renew at a time (the guard's `renewing`)
                }
                let step = match (cl.confirmed_at, cl.lost) {
                    (None, _) => Step::Acquire,
                    (Some(_), false) => Step::Renew,
                    (Some(_), true) => return,
                };
                self.clients[c].in_flight = Some((step, self.obj.clone(), self.now));
            }
            Op::Write(c) => {
                let Some((step, seen, read_at)) = self.clients[c].in_flight.take() else {
                    return;
                };
                let owner = self.owner(c);
                let observed = self.observed(&seen);
                match step {
                    Step::Acquire => {
                        let won = match plan_lock_acquire(&observed, read_at) {
                            LockAction::Blocked => false,
                            LockAction::Create if self.obj.is_none() => {
                                self.put(owner, 1);
                                true
                            }
                            LockAction::Create => false,
                            LockAction::Steal { etag, next_epoch } if self.etag_is(&etag) => {
                                self.put(owner, next_epoch);
                                true
                            }
                            LockAction::Steal { .. } => false,
                        };
                        if won {
                            self.clients[c].confirmed_at = Some(read_at);
                            self.clients[c].lost = false;
                        }
                    }
                    Step::Renew => match plan_lock_renew(&observed, read_at, &owner) {
                        RenewLockAction::Renew { etag, epoch } if self.etag_is(&etag) => {
                            self.put(owner, epoch);
                            self.clients[c].confirmed_at = Some(read_at);
                        }
                        _ => {
                            // "Lost" only when the lock truly is not ours
                            // (no false loss from a racing renew).
                            let ours_and_live = self.obj.as_ref().is_some_and(|s| {
                                s.lock.owner == owner && s.written_at + self.ttl >= read_at
                            }) && seen.as_ref().map(|s| s.etag)
                                == self.obj.as_ref().map(|s| s.etag);
                            assert!(!ours_and_live, "renew reported a live lock as lost");
                            self.clients[c].lost = true;
                        }
                    },
                    Step::Release => {
                        if let Some(s) = &seen {
                            if s.lock.owner == owner && self.etag_is(&s.etag.to_string()) {
                                self.obj = None;
                            }
                        }
                    }
                }
            }
            Op::Commit(c) => {
                if self.trusts(c) {
                    // While c trusts its hold, nobody else may.
                    for other in 0..CLIENTS {
                        assert!(
                            other == c || !self.trusts(other),
                            "clients {c} and {other} both trust the lock at t={}",
                            self.now
                        );
                    }
                    // And the server still names c the owner.
                    let owner = self.owner(c);
                    assert_eq!(
                        self.obj.as_ref().map(|s| s.lock.owner.clone()),
                        Some(owner),
                        "client {c} commits without owning the lock"
                    );
                }
            }
            Op::Drop(c) => {
                // The guard lets a renew in flight finish first.
                if self.clients[c].confirmed_at.is_none() || self.clients[c].in_flight.is_some() {
                    return;
                }
                // The guard stops the heartbeat, then releases: a fresh
                // read, then an owner-scoped delete (next Write).
                let seen = self.obj.clone();
                let cl = &mut self.clients[c];
                cl.confirmed_at = None;
                cl.lost = false;
                cl.in_flight = Some((Step::Release, seen, self.now));
            }
        }
    }
}

fn op(ttl: i64) -> impl Strategy<Value = Op> {
    let c = 0..CLIENTS;
    prop_oneof![
        3 => c.clone().prop_map(Op::Read),
        3 => c.clone().prop_map(Op::Write),
        3 => c.clone().prop_map(Op::Commit),
        1 => c.prop_map(Op::Drop),
        2 => (0..=ttl / 2).prop_map(Op::Tick),
    ]
}

proptest! {
    #[test]
    fn at_most_one_client_trusts_the_lock(
        (ttl, ops) in (4i64..=120).prop_flat_map(|ttl| {
            (Just(ttl), proptest::collection::vec(op(ttl), 0..120))
        })
    ) {
        let mut w = World {
            ttl,
            now: 1_000,
            obj: None,
            etags: 0,
            clients: vec![Client::default(); CLIENTS],
        };
        for op in ops {
            // A released client's next hold uses a fresh owner token.
            if let Op::Read(c) = &op {
                if w.clients[*c].confirmed_at.is_none() && w.clients[*c].in_flight.is_none() {
                    w.clients[*c].owner_seq += 1;
                }
            }
            w.apply(op);
            for c in 0..CLIENTS {
                w.apply(Op::Commit(c));
            }
        }
    }
}
