// SPDX-License-Identifier: BUSL-1.1

//! The foreign-key sentinel: this proxy does not write a bucket that holds
//! objects sealed with a key that it does not have.
//!
//! Two proxies with separate configs can point at one stored bucket. When
//! one of them encrypts and the other has no key (or another key), the
//! second one writes objects that the first cannot read: plaintext that the
//! read rule refuses, or seals under a key it does not hold. The witness
//! (`coordination::encryption_witness`) catches this only between nodes
//! that share a coordination bucket. The sentinel needs no agreement: the
//! evidence is in the data bucket itself, because every sealed object names
//! its key by fingerprint (the `dg-sealed` metadata and the `!!2` listing
//! token both start with it).
//!
//! Evidence comes from three places:
//!
//! 1. **The key claim**: a proxy that encrypts a bucket claims it for its
//!    key, with one small object that it creates with a conditional write
//!    (`If-None-Match: *` on S3, a hard link on a filesystem;
//!    [`crate::storage::listing_facts::KEY_CLAIM_KEY`]). Two proxies with
//!    different keys cannot both claim one bucket, also not when both
//!    start on an empty bucket at the same moment, and a claim does not
//!    depend on the size of the bucket. `claim_action` decides what a
//!    writer does with it. A proxy reads the claim again every
//!    `CLAIM_TTL` (its own key) or `OPEN_CLAIM_TTL` (no claim, or a
//!    claim of another key).
//! 2. **Sightings**, at no cost: every sealed object and listing token that
//!    this wrapper reads records the fingerprint of its key when the ring
//!    does not hold it.
//! 3. **A probe**, before the first write to a bucket and then every
//!    `PROBE_TTL`: one listing page from the root, the listing tokens of
//!    that page (S3: one more LIST of the facts namespace) and a HEAD of a
//!    few baselines and objects spread over the page. It finds the buckets
//!    that an earlier release encrypted, which have no claim. A writer
//!    probes before it claims, so it never claims a bucket that holds
//!    another key.
//!
//! The verdict is computed against the CURRENT keyring on every write, so
//! an operator who adds the missing key (as the write key, or as a read-only
//! `legacy_key`/keyring entry) unblocks the writes at once. Every piece of
//! evidence expires: a sighting after `SIGHTING_TTL`, a claim after its
//! TTL (then it is read again). A failed probe or claim exchange lets the
//! write go ahead and is retried after `FAILED_TTL`, with one warning per
//! failure streak.
//!
//! Deletes are not checked: the proxy must stay able to remove what it
//! wrote by mistake. All state takes an explicit `now`, so the TTL rules
//! are unit-tested without a clock.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::v2::FP_LEN;

/// How long a probe verdict holds.
pub const PROBE_TTL: Duration = Duration::from_secs(600);
/// How long a failed probe or claim exchange waits before the next try.
pub const FAILED_TTL: Duration = Duration::from_secs(60);
/// How long a fingerprint counts after it was last seen.
pub const SIGHTING_TTL: Duration = Duration::from_secs(3600);
/// Keys that the probe lists from the root of the bucket.
pub const PROBE_LIST: u32 = 200;
/// Objects (baselines first) that the probe sends a HEAD for.
pub const PROBE_HEADS: usize = 8;
/// How long the claim holds that names this proxy's write key.
pub const CLAIM_TTL: Duration = Duration::from_secs(600);
/// How long a missing claim, or a claim of another key, holds before the
/// next read: a proxy that writes plaintext sees a new claim this soon, and
/// a removed foreign claim stops refusing this soon.
pub const OPEN_CLAIM_TTL: Duration = Duration::from_secs(60);

/// The fingerprint of a key, as the objects carry it.
pub type Fingerprint = [u8; FP_LEN];

/// What the sentinel knows about one stored bucket.
#[derive(Default)]
struct BucketState {
    /// Each fingerprint seen, with when it was last seen and an object
    /// that carries it.
    seen: HashMap<Fingerprint, (Instant, String)>,
    /// When the probe verdict expires; `None` = never probed.
    probe_until: Option<Instant>,
    /// The last probe failed (one warning per failure streak).
    probe_failed: bool,
    /// The claim as last read or written (`None` = no claim).
    claim: Option<Fingerprint>,
    /// When the claim must be read again; `None` = never read.
    claim_until: Option<Instant>,
    /// The last claim exchange failed: no new attempt before `claim_until`.
    claim_failed: bool,
    /// One probe or claim exchange at a time per bucket: concurrent first
    /// writes wait for one exchange instead of each sending their own.
    lock: Arc<tokio::sync::Mutex<()>>,
}

/// The sentinel state of one raw backend, shared by every engine of the
/// lineage (an engine rebuild keeps the evidence).
#[derive(Default)]
pub struct KeySentinel {
    buckets: parking_lot::Mutex<HashMap<String, BucketState>>,
}

/// Where a refusal's key was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    /// The key claim of the bucket names it.
    Claim,
    /// This stored object carries it.
    Object(String),
}

/// A write that the sentinel refuses: the key that this proxy lacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub fingerprint: Fingerprint,
    pub evidence: Evidence,
}

impl KeySentinel {
    fn with<T>(&self, bucket: &str, f: impl FnOnce(&mut BucketState) -> T) -> T {
        f(self.buckets.lock().entry(bucket.to_string()).or_default())
    }

    /// The single-flight lock of `bucket`'s probe and claim exchanges.
    pub fn lock(&self, bucket: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.with(bucket, |s| s.lock.clone())
    }

    /// Record that `object` in `bucket` is sealed with the key `fp`.
    pub fn sighted(&self, bucket: &str, fp: Fingerprint, object: &str, now: Instant) {
        self.with(bucket, |s| {
            s.seen.insert(fp, (now, object.to_string()));
        });
    }

    /// Does `bucket` need a probe before a write?
    pub fn probe_due(&self, bucket: &str, now: Instant) -> bool {
        self.with(bucket, |s| s.probe_until.is_none_or(|until| now >= until))
    }

    /// Record the result of a probe: the fingerprints found (with an
    /// object each), or `None` when the probe failed. True when this is
    /// the first failure of a streak (the caller warns once).
    pub fn probed(
        &self,
        bucket: &str,
        found: Option<Vec<(Fingerprint, String)>>,
        now: Instant,
    ) -> bool {
        self.with(bucket, |s| {
            let first_failure = found.is_none() && !s.probe_failed;
            s.probe_failed = found.is_none();
            s.probe_until = Some(
                now + if found.is_some() {
                    PROBE_TTL
                } else {
                    FAILED_TTL
                },
            );
            for (fp, object) in found.into_iter().flatten() {
                s.seen.insert(fp, (now, object));
            }
            first_failure
        })
    }

    /// Must the claim be read (again) before a write that writes with
    /// `write`? When it expired, yes. Inside its TTL, also when the cached
    /// claim calls for a change (the first claim, a rotation, a removal),
    /// unless the last exchange failed: then the TTL is the backoff.
    pub fn claim_due(
        &self,
        bucket: &str,
        now: Instant,
        held: impl Fn(&Fingerprint) -> bool,
        write: Option<Fingerprint>,
    ) -> bool {
        self.with(bucket, |s| match s.claim_until {
            None => true,
            Some(until) if now >= until => true,
            Some(_) if s.claim_failed => false,
            Some(_) => !matches!(
                claim_action(s.claim.map(Some), held, write),
                ClaimAction::Keep | ClaimAction::Refuse(_)
            ),
        })
    }

    /// Record the claim of `bucket` as read or written. `own`: it names
    /// this proxy's write key (`CLAIM_TTL`); else `OPEN_CLAIM_TTL`.
    pub fn claim_seen(&self, bucket: &str, claim: Option<Fingerprint>, own: bool, now: Instant) {
        self.with(bucket, |s| {
            s.claim = claim;
            s.claim_failed = false;
            s.claim_until = Some(now + if own { CLAIM_TTL } else { OPEN_CLAIM_TTL });
        });
    }

    /// The claim exchange failed: keep the cached claim and back off for
    /// `FAILED_TTL`. True when this is the first failure of a streak.
    pub fn claim_failed(&self, bucket: &str, now: Instant) -> bool {
        self.with(bucket, |s| {
            let first = !s.claim_failed;
            s.claim_failed = true;
            s.claim_until = Some(now + FAILED_TTL);
            first
        })
    }

    /// Forget `bucket` (it was deleted: a new bucket of that name starts
    /// without evidence).
    pub fn forget(&self, bucket: &str) {
        self.buckets.lock().remove(bucket);
    }

    /// The refusal of a write to `bucket`, if live evidence names a key
    /// that `held` does not hold: the claim while it is fresh, then the
    /// sightings that did not expire. No I/O: the background writers ask
    /// it too (`write_refusal`).
    pub fn refusal(
        &self,
        bucket: &str,
        now: Instant,
        held: impl Fn(&Fingerprint) -> bool,
    ) -> Option<Refusal> {
        let mut buckets = self.buckets.lock();
        let s = buckets.get_mut(bucket)?;
        let claim_fresh = s.claim_until.is_some_and(|until| now < until);
        if let Some(fp) = s.claim.filter(|fp| claim_fresh && !held(fp)) {
            return Some(Refusal {
                fingerprint: fp,
                evidence: Evidence::Claim,
            });
        }
        s.seen
            .retain(|_, (at, _)| now.saturating_duration_since(*at) < SIGHTING_TTL);
        let (fp, object) = first_foreign(&s.seen, held)?;
        Some(Refusal {
            fingerprint: fp,
            evidence: Evidence::Object(object),
        })
    }
}

/// What a writer does with the key claim of a bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimAction {
    /// Nothing to change.
    Keep,
    /// Claim the bucket for this proxy's write key.
    Create,
    /// Replace the claim with this proxy's write key: a key rotation, or
    /// a claim that is not a fingerprint label.
    Replace,
    /// Remove the claim: the bucket stops encrypting.
    Remove,
    /// The claim names a key that this proxy does not have.
    Refuse(Fingerprint),
}

/// THE claim rule (pure). `claim`: `None` = no claim; `Some(None)` = a
/// claim that is not a fingerprint label; `Some(Some(fp))`. `held`: is a
/// key in this proxy's ring. `write`: the key that this proxy writes the
/// bucket with, `None` for plaintext.
///
/// A claim of a key that this proxy does not have refuses every write. A
/// claim of a key in the ring belongs to this proxy's history, so the
/// writer may move it to its write key (a rotation) or remove it (the
/// bucket stops encrypting). An unreadable claim is replaced by a writer
/// that encrypts and ignored by one that does not.
pub fn claim_action(
    claim: Option<Option<Fingerprint>>,
    held: impl Fn(&Fingerprint) -> bool,
    write: Option<Fingerprint>,
) -> ClaimAction {
    match (claim, write) {
        (None, None) | (Some(None), None) => ClaimAction::Keep,
        (None, Some(_)) => ClaimAction::Create,
        (Some(None), Some(_)) => ClaimAction::Replace,
        (Some(Some(c)), _) if !held(&c) => ClaimAction::Refuse(c),
        (Some(Some(c)), Some(w)) if c == w => ClaimAction::Keep,
        (Some(Some(_)), Some(_)) => ClaimAction::Replace,
        (Some(Some(_)), None) => ClaimAction::Remove,
    }
}

/// The public label of a key: `fp-` and the hex of its fingerprint. The
/// claim's text, the generated key id, and the name in every message.
pub fn label(fp: &Fingerprint) -> String {
    format!("fp-{}", hex::encode(fp))
}

/// The fingerprint that a claim's text names, or `None`.
pub fn parse_label(text: &str) -> Option<Fingerprint> {
    let bytes = hex::decode(text.trim().strip_prefix("fp-")?).ok()?;
    bytes.try_into().ok()
}

/// The first fingerprint (in a stable order) of `seen` that `held` does not
/// hold, with its example object. Pure.
fn first_foreign(
    seen: &HashMap<Fingerprint, (Instant, String)>,
    held: impl Fn(&Fingerprint) -> bool,
) -> Option<(Fingerprint, String)> {
    seen.iter()
        .filter(|(fp, _)| !held(fp))
        .map(|(fp, (_, object))| (*fp, object.clone()))
        .min()
}

/// The operator's text of a refused write (logged; the client gets less,
/// see `api::errors`). Names the key by its public label, never by key
/// material.
pub fn refusal_detail(bucket: &str, r: &Refusal, write_key: Option<&str>) -> String {
    let how = match write_key {
        None => "without proxy encryption".to_string(),
        Some(id) => format!("under its key '{id}'"),
    };
    let evidence = match &r.evidence {
        Evidence::Claim => "named by the key claim of the bucket".to_string(),
        Evidence::Object(o) => format!("for example '{o}'"),
    };
    format!(
        "bucket '{bucket}' holds objects that were encrypted with a key that this proxy does \
         not have (key {}, {evidence}). This proxy would write {how}, and the proxy that holds \
         that key could not read the result, so it refuses the write. Add that key to this \
         proxy's encryption config of the bucket, or send the writes through the proxy that \
         holds it. Reads and deletes still work",
        label(&r.fingerprint)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Fingerprint = [1; FP_LEN];
    const B: Fingerprint = [2; FP_LEN];
    const C: Fingerprint = [3; FP_LEN];

    /// The ring holds A and B; C is another proxy's key.
    fn held(fp: &Fingerprint) -> bool {
        *fp == A || *fp == B
    }

    fn none_held(_: &Fingerprint) -> bool {
        false
    }

    #[test]
    fn claim_action_truth_table() {
        use ClaimAction::*;
        assert_eq!(claim_action(None, held, None), Keep);
        assert_eq!(claim_action(None, held, Some(A)), Create);
        assert_eq!(claim_action(Some(Some(A)), held, Some(A)), Keep);
        // Rotation: the claimed key is still in the ring.
        assert_eq!(claim_action(Some(Some(A)), held, Some(B)), Replace);
        // The bucket stops encrypting: the owner removes its claim.
        assert_eq!(claim_action(Some(Some(A)), held, None), Remove);
        // Another proxy's key: refused, writing plaintext or another key.
        assert_eq!(claim_action(Some(Some(C)), held, None), Refuse(C));
        assert_eq!(claim_action(Some(Some(C)), held, Some(A)), Refuse(C));
        // A claim that is not a label.
        assert_eq!(claim_action(Some(None), held, None), Keep);
        assert_eq!(claim_action(Some(None), held, Some(A)), Replace);
    }

    #[test]
    fn a_label_round_trips() {
        let fp = [0xab; FP_LEN];
        assert_eq!(label(&fp), "fp-abababababababab");
        assert_eq!(parse_label(&label(&fp)), Some(fp));
        assert_eq!(parse_label(" fp-abababababababab\n"), Some(fp));
        for bad in [
            "",
            "fp-",
            "fp-abab",
            "abababababababab",
            "fp-zzabababababababab",
        ] {
            assert_eq!(parse_label(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_bucket_without_evidence_is_written() {
        let (s, t) = (KeySentinel::default(), Instant::now());
        assert!(s.probe_due("b", t));
        assert_eq!(s.refusal("b", t, none_held), None);
        s.probed("b", Some(vec![]), t);
        assert!(!s.probe_due("b", t));
        assert!(s.probe_due("b", t + PROBE_TTL));
        assert_eq!(s.refusal("b", t, none_held), None);
    }

    #[test]
    fn a_sighting_of_a_key_not_held_refuses_until_it_expires() {
        let (s, t) = (KeySentinel::default(), Instant::now());
        s.sighted("b", C, "integrity/report.html", t);
        let r = s.refusal("b", t, held).expect("refused");
        assert_eq!(r.fingerprint, C);
        assert_eq!(r.evidence, Evidence::Object("integrity/report.html".into()));
        assert_eq!(s.refusal("other", t, held), None);
        // A key in the ring (the write key, or a read-only legacy key) is
        // not foreign.
        s.sighted("b2", A, "x", t);
        assert_eq!(s.refusal("b2", t, held), None);
        assert_eq!(s.refusal("b", t + SIGHTING_TTL, held), None, "expired");
    }

    /// Review finding: a cached foreign claim refused every write until a
    /// restart, because the refusal ignored the claim's TTL and so the
    /// claim was never read again.
    #[test]
    fn a_foreign_claim_refuses_only_while_it_is_fresh() {
        let (s, t) = (KeySentinel::default(), Instant::now());
        s.claim_seen("b", Some(C), false, t);
        let r = s.refusal("b", t, held).expect("refused");
        assert_eq!(r.evidence, Evidence::Claim);
        assert!(!s.claim_due("b", t, held, None));
        let later = t + OPEN_CLAIM_TTL;
        assert_eq!(s.refusal("b", later, held), None, "stale: read again");
        assert!(s.claim_due("b", later, held, None));
        // The re-read finds the claim gone: writes go ahead.
        s.claim_seen("b", None, false, later);
        assert_eq!(s.refusal("b", later, held), None);
    }

    /// Review finding: after a failed claim write, every write tried again
    /// (a GET, a failed PUT and a warning per write). The TTL is the
    /// backoff, and only the first failure of a streak warns.
    #[test]
    fn a_failed_claim_exchange_backs_off() {
        let (s, t) = (KeySentinel::default(), Instant::now());
        assert!(s.claim_due("b", t, held, Some(A)));
        assert!(s.claim_failed("b", t), "first failure warns");
        assert!(!s.claim_due("b", t, held, Some(A)), "backoff");
        assert!(s.claim_due("b", t + FAILED_TTL, held, Some(A)));
        assert!(
            !s.claim_failed("b", t + FAILED_TTL),
            "the streak warns once"
        );
        s.claim_seen("b", Some(A), true, t + FAILED_TTL);
        assert!(s.claim_failed("b", t + CLAIM_TTL * 2), "a new streak warns");
    }

    /// Inside the TTL, a change of the write key (a rotation) or the first
    /// claim is acted on at once; a claim that matches is not read again.
    #[test]
    fn claim_due_acts_on_a_change_inside_the_ttl() {
        let (s, t) = (KeySentinel::default(), Instant::now());
        s.claim_seen("b", Some(A), true, t);
        assert!(!s.claim_due("b", t, held, Some(A)));
        assert!(s.claim_due("b", t, held, Some(B)), "rotation");
        assert!(s.claim_due("b", t, held, None), "stops encrypting");
        s.claim_seen("c", None, false, t);
        assert!(s.claim_due("c", t, held, Some(A)), "first claim");
        assert!(!s.claim_due("c", t, held, None));
    }

    #[test]
    fn a_failed_probe_retries_soon_and_warns_once() {
        let (s, t) = (KeySentinel::default(), Instant::now());
        assert!(s.probed("b", None, t));
        assert!(!s.probe_due("b", t));
        assert!(s.probe_due("b", t + FAILED_TTL));
        assert!(!s.probed("b", None, t + FAILED_TTL));
        s.probed("b", Some(vec![]), t + FAILED_TTL);
        assert!(!s.probe_due("b", t + FAILED_TTL * 2));
        assert!(s.probed("b", None, t + PROBE_TTL * 2));
    }

    #[test]
    fn forget_drops_every_piece_of_evidence() {
        let (s, t) = (KeySentinel::default(), Instant::now());
        s.sighted("b", C, "x", t);
        s.claim_seen("b", Some(C), false, t);
        s.forget("b");
        assert_eq!(s.refusal("b", t, held), None);
        assert!(s.probe_due("b", t) && s.claim_due("b", t, held, None));
    }

    #[test]
    fn the_first_foreign_key_is_stable() {
        let now = Instant::now();
        let seen = HashMap::from([
            (B, (now, "b".to_string())),
            (C, (now, "c".to_string())),
            (A, (now, "held".to_string())),
        ]);
        let f = first_foreign(&seen, |fp| *fp == A).unwrap();
        assert_eq!(f, (B, "b".to_string()));
    }

    #[test]
    fn the_detail_names_the_key_the_evidence_and_the_write() {
        let r = Refusal {
            fingerprint: C,
            evidence: Evidence::Object("fw/app.zip".into()),
        };
        let d = refusal_detail("releases", &r, None);
        assert!(d.contains(&label(&C)) && d.contains("fw/app.zip"), "{d}");
        assert!(d.contains("without proxy encryption"), "{d}");
        let r = Refusal {
            fingerprint: C,
            evidence: Evidence::Claim,
        };
        let d = refusal_detail("releases", &r, Some("k1"));
        assert!(
            d.contains("key claim") && d.contains("under its key 'k1'"),
            "{d}"
        );
    }
}
