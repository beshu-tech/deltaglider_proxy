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
    /// First step of an acquire / renew by client `c`. An acquire writes
    /// first (create-only PUT) and reads only when that PUT finds a lock
    /// object; a renew reads.
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
    /// Release by the read-then-delete path (no kept ETag).
    Release,
    /// Release by `DELETE If-Match` on the kept ETag of our last write.
    ReleaseKept(u64),
}

#[derive(Debug, Clone, Default)]
struct Client {
    /// Holding (guard alive): time of the last confirmed read.
    confirmed_at: Option<i64>,
    lost: bool,
    /// A read awaiting its write: (step, what it saw, when).
    in_flight: Option<(Step, Option<Stored>, i64)>,
    owner_seq: u64,
    /// The ETag of this client's last successful write of the lock.
    kept: Option<u64>,
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

    fn put(&mut self, owner: String, epoch: u64) -> u64 {
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
        self.etags
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
                if step == Step::Acquire && self.obj.is_none() {
                    // Write first: the create-only PUT finds no object.
                    let owner = self.owner(c);
                    let etag = self.put(owner, 1);
                    let cl = &mut self.clients[c];
                    cl.confirmed_at = Some(self.now);
                    cl.lost = false;
                    cl.kept = Some(etag);
                    return;
                }
                // The create answered 412 (or a renew): read the lock.
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
                        let won = match plan_lock_acquire(&observed, read_at, &owner) {
                            LockAction::Blocked => None,
                            // Our own live lock (a write whose response was
                            // lost): held as it is, by its ETag.
                            LockAction::Adopt { etag } if self.etag_is(&etag) => etag.parse().ok(),
                            LockAction::Adopt { .. } => None,
                            LockAction::Create if self.obj.is_none() => Some(self.put(owner, 1)),
                            LockAction::Create => None,
                            LockAction::Steal { etag, next_epoch } if self.etag_is(&etag) => {
                                Some(self.put(owner, next_epoch))
                            }
                            LockAction::Steal { .. } => None,
                        };
                        if let Some(etag) = won {
                            self.clients[c].confirmed_at = Some(read_at);
                            self.clients[c].lost = false;
                            self.clients[c].kept = Some(etag);
                        }
                    }
                    Step::Renew => match plan_lock_renew(&observed, read_at, &owner) {
                        RenewLockAction::Renew { etag, epoch } if self.etag_is(&etag) => {
                            let etag = self.put(owner, epoch);
                            self.clients[c].confirmed_at = Some(read_at);
                            self.clients[c].kept = Some(etag);
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
                            // A renew PUT that lost drops the kept ETag.
                            if matches!(
                                plan_lock_renew(&observed, read_at, &owner),
                                RenewLockAction::Renew { .. }
                            ) {
                                self.clients[c].kept = None;
                            }
                        }
                    },
                    Step::ReleaseKept(etag) => {
                        // DELETE If-Match: removes only our own last write.
                        if self.etag_is(&etag.to_string()) {
                            self.obj = None;
                        }
                    }
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
                // The guard stops the heartbeat, then releases: a DELETE
                // If-Match on the kept ETag, or (none kept) a fresh read and
                // an owner-scoped delete (next Write).
                let seen = self.obj.clone();
                let cl = &mut self.clients[c];
                cl.confirmed_at = None;
                cl.lost = false;
                let step = match cl.kept.take() {
                    Some(etag) => Step::ReleaseKept(etag),
                    None => Step::Release,
                };
                cl.in_flight = Some((step, seen, self.now));
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
