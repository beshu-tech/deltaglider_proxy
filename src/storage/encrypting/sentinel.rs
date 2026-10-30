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
//!    (`If-None-Match: *` on S3, `create_new` on a filesystem;
//!    [`crate::storage::listing_facts::KEY_CLAIM_KEY`]). Two proxies with
//!    different keys cannot both claim one bucket, also not when both
//!    start on an empty bucket at the same moment, and a claim does not
//!    depend on the size of the bucket. [`claim_step`] decides what a
//!    writer does with it: create it, keep it, replace it after a key
//!    rotation (the old key is still in the ring), remove it when the
//!    bucket stops encrypting, or refuse the write. A proxy that writes
//!    plaintext only reads the claim, every [`OPEN_CLAIM_TTL`].
//! 2. **Sightings**, at no cost: every sealed object that this wrapper reads
//!    (GET, HEAD, a listing with metadata, the reference fence before a
//!    delta write) records the fingerprint of its key.
//! 3. **A probe**, before the first write to a bucket and then every
//!    [`PROBE_TTL`]: one listing page from the root, the listing tokens of
//!    that page (S3: one more LIST of the facts namespace) and a HEAD of a
//!    few baselines and objects spread over the page. It stops at the first
//!    key that this proxy does not have. It finds the buckets that an
//!    earlier release encrypted, which have no claim. A writer probes
//!    before it claims, so it never claims a bucket that holds another key.
//!
//! The verdict is computed against the CURRENT keyring on every write, so
//! an operator who adds the missing key (as the write key, or as a read-only
//! `legacy_key`/keyring entry) unblocks the writes at once. A sighting
//! expires after [`SIGHTING_TTL`] without a new one, so a bucket whose
//! foreign objects are gone becomes writable again.
//!
//! Deletes are not checked: the proxy must stay able to remove what it
//! wrote by mistake.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::seal::Keyring;
use super::v2::FP_LEN;

/// How long a probe verdict holds.
pub const PROBE_TTL: Duration = Duration::from_secs(600);
/// How long a probe that failed holds (the write goes ahead, fail open).
pub const FAILED_PROBE_TTL: Duration = Duration::from_secs(60);
/// How long a fingerprint counts after it was last seen.
pub const SIGHTING_TTL: Duration = Duration::from_secs(3600);
/// Keys that the probe lists from the root of the bucket.
pub const PROBE_LIST: usize = 200;
/// Objects (baselines first) that the probe sends a HEAD for.
pub const PROBE_HEADS: usize = 8;
/// How long the claim holds that this proxy keeps (its own key).
pub const CLAIM_TTL: Duration = Duration::from_secs(600);
/// How long a missing or refused claim holds before the next read: a
/// proxy that writes plaintext sees another proxy's new claim this soon.
pub const OPEN_CLAIM_TTL: Duration = Duration::from_secs(60);

/// The fingerprint of a key, as the objects carry it.
pub type Fingerprint = [u8; FP_LEN];

/// The evidence of one stored bucket.
#[derive(Default, Clone, Debug)]
struct BucketEvidence {
    /// Each fingerprint seen, with when it was last seen and an object
    /// that carries it.
    seen: HashMap<Fingerprint, (Instant, String)>,
    /// When the probe verdict expires; `None` = never probed.
    probe_valid_until: Option<Instant>,
    /// The key claim as last read or written (`None` = no claim).
    claim: Option<Fingerprint>,
    /// When the claim must be read again; `None` = never read.
    claim_valid_until: Option<Instant>,
}

/// The sentinel state of one raw backend, shared by every engine of the
/// lineage (an engine rebuild keeps the evidence).
#[derive(Default)]
pub struct KeySentinel {
    buckets: parking_lot::Mutex<HashMap<String, BucketEvidence>>,
    /// One probe at a time: concurrent first writes wait for one probe
    /// instead of each sending their own.
    probe: tokio::sync::Mutex<()>,
}

impl KeySentinel {
    /// Record that `object` in `bucket` is sealed with the key `fp`.
    pub fn sighted(&self, bucket: &str, fp: Fingerprint, object: &str) {
        self.sighted_at(bucket, fp, object, Instant::now());
    }

    fn sighted_at(&self, bucket: &str, fp: Fingerprint, object: &str, at: Instant) {
        self.buckets
            .lock()
            .entry(bucket.to_string())
            .or_default()
            .seen
            .insert(fp, (at, object.to_string()));
    }

    /// Does `bucket` need a probe before a write?
    pub fn needs_probe(&self, bucket: &str) -> bool {
        self.needs_probe_at(bucket, Instant::now())
    }

    fn needs_probe_at(&self, bucket: &str, now: Instant) -> bool {
        self.buckets
            .lock()
            .get(bucket)
            .and_then(|e| e.probe_valid_until)
            .is_none_or(|until| now >= until)
    }

    /// Record the result of a probe: the fingerprints found (with an
    /// object each), or `None` when the probe failed.
    pub fn probed(&self, bucket: &str, found: Option<Vec<(Fingerprint, String)>>) {
        let now = Instant::now();
        let ttl = if found.is_some() {
            PROBE_TTL
        } else {
            FAILED_PROBE_TTL
        };
        let mut buckets = self.buckets.lock();
        let e = buckets.entry(bucket.to_string()).or_default();
        for (fp, object) in found.into_iter().flatten() {
            e.seen.insert(fp, (now, object));
        }
        e.probe_valid_until = Some(now + ttl);
    }

    /// Must the claim be read (again) before a write that writes with
    /// `write`? When the cached claim calls for a change (a rotation, the
    /// first claim), yes.
    pub fn claim_due(
        &self,
        bucket: &str,
        held: impl Fn(&Fingerprint) -> bool,
        write: Option<Fingerprint>,
    ) -> bool {
        let now = Instant::now();
        let buckets = self.buckets.lock();
        let Some(e) = buckets.get(bucket) else {
            return true;
        };
        if e.claim_valid_until.is_none_or(|until| now >= until) {
            return true;
        }
        let cached = e.claim.map(|fp| (Some(fp), String::new()));
        !matches!(
            claim_step(cached.as_ref().map(|(f, v)| (*f, v.as_str())), held, write),
            ClaimStep::Keep | ClaimStep::Refuse(_)
        )
    }

    /// Record the claim of `bucket` as read or written. `own`: the claim
    /// names this proxy's write key (it holds longer).
    pub fn claim_seen(&self, bucket: &str, claim: Option<Fingerprint>, own: bool) {
        let ttl = if own { CLAIM_TTL } else { OPEN_CLAIM_TTL };
        let mut buckets = self.buckets.lock();
        let e = buckets.entry(bucket.to_string()).or_default();
        e.claim = claim;
        e.claim_valid_until = Some(Instant::now() + ttl);
    }

    /// The claim could not be read or written: keep the cached one, and try
    /// again after `OPEN_CLAIM_TTL`.
    pub fn claim_failed(&self, bucket: &str) {
        let mut buckets = self.buckets.lock();
        let e = buckets.entry(bucket.to_string()).or_default();
        e.claim_valid_until = Some(Instant::now() + OPEN_CLAIM_TTL);
    }

    /// The probe lock (see [`Self::needs_probe`]).
    pub async fn probe_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.probe.lock().await
    }

    /// The refusal of a write to `bucket`, if it holds an object sealed
    /// with a key that is not in `ring`. `write_key`: the id of the key
    /// that this proxy writes the bucket with, `None` for plaintext.
    pub fn refusal(&self, bucket: &str, ring: &Keyring, write_key: Option<&str>) -> Option<String> {
        let now = Instant::now();
        let mut buckets = self.buckets.lock();
        let e = buckets.get_mut(bucket)?;
        if let Some(claim) = e.claim.filter(|fp| ring.get(fp).is_none()) {
            return Some(refusal_message(
                bucket,
                &claim,
                "named by the key claim of the bucket",
                write_key,
            ));
        }
        e.seen
            .retain(|_, (at, _)| now.duration_since(*at) < SIGHTING_TTL);
        let foreign = first_foreign(&e.seen, |fp| ring.get(fp).is_some())?;
        Some(refusal_message(
            bucket,
            &foreign.0,
            &format!("for example '{}'", foreign.1),
            write_key,
        ))
    }
}

/// What a writer does with the key claim of a bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimStep {
    /// Nothing to change.
    Keep,
    /// Claim the bucket for this key.
    Create(Fingerprint),
    /// Replace the claim (at the version) with this key: a key rotation.
    Replace(Fingerprint, String),
    /// Remove the claim (at the version): the bucket stops encrypting.
    Remove(String),
    /// The claim names a key that this proxy does not have.
    Refuse(Fingerprint),
}

/// THE claim rule (pure). `claim`: the claim as read, its fingerprint
/// (`None` when it is not a fingerprint label) and its version. `held`:
/// is a key in this proxy's ring. `write`: the key that this proxy writes
/// the bucket with, `None` for plaintext.
///
/// A claim of a key that this proxy does not have refuses every write. A
/// claim of a key in the ring belongs to this proxy's history, so the
/// writer may move it to its write key (a rotation) or remove it (the
/// bucket stops encrypting). An unreadable claim is replaced by a writer
/// that encrypts and ignored by one that does not.
pub fn claim_step(
    claim: Option<(Option<Fingerprint>, &str)>,
    held: impl Fn(&Fingerprint) -> bool,
    write: Option<Fingerprint>,
) -> ClaimStep {
    match (claim, write) {
        (None, None) | (Some((None, _)), None) => ClaimStep::Keep,
        (None, Some(w)) => ClaimStep::Create(w),
        (Some((None, version)), Some(w)) => ClaimStep::Replace(w, version.to_string()),
        (Some((Some(c), _)), _) if !held(&c) => ClaimStep::Refuse(c),
        (Some((Some(c), _)), Some(w)) if c == w => ClaimStep::Keep,
        (Some((Some(_), version)), Some(w)) => ClaimStep::Replace(w, version.to_string()),
        (Some((Some(_), version)), None) => ClaimStep::Remove(version.to_string()),
    }
}

/// The text of a claim: `fp-` and the hex of the fingerprint.
pub fn claim_label(fp: &Fingerprint) -> String {
    format!("fp-{}", hex::encode(fp))
}

/// The fingerprint that a claim's text names, or `None`.
pub fn parse_claim_label(text: &str) -> Option<Fingerprint> {
    let bytes = hex::decode(text.trim().strip_prefix("fp-")?).ok()?;
    bytes.try_into().ok()
}

/// The first fingerprint (in a stable order) of `seen` that `held` does not
/// hold, with its example object. Pure.
fn first_foreign(
    seen: &HashMap<Fingerprint, (Instant, String)>,
    held: impl Fn(&Fingerprint) -> bool,
) -> Option<(Fingerprint, String)> {
    let mut foreign: Vec<_> = seen
        .iter()
        .filter(|(fp, _)| !held(fp))
        .map(|(fp, (_, object))| (*fp, object.clone()))
        .collect();
    foreign.sort();
    foreign.into_iter().next()
}

/// The words by which [`super::seal::names_a_key_not_held`] knows a refusal
/// that names a key this proxy does not have.
pub const KEY_NOT_HELD: &str = "with a key that this proxy does not have";

/// The refusal of a write (pure). Names the key by its public fingerprint
/// label, never by key material.
pub fn refusal_message(
    bucket: &str,
    fp: &Fingerprint,
    evidence: &str,
    write_key: Option<&str>,
) -> String {
    let how = match write_key {
        None => "without proxy encryption".to_string(),
        Some(id) => format!("under its key '{id}'"),
    };
    format!(
        "bucket '{bucket}' holds objects that were encrypted {KEY_NOT_HELD} (key fp-{}, \
         {evidence}). This proxy would write {how}, and the proxy that holds \
         that key could not read the result, so it refuses the write. Add that key to this \
         proxy's encryption config of the bucket, or send the writes through the proxy that \
         holds it. Reads and deletes still work",
        hex::encode(fp)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring_with(key: &[u8; 32]) -> (Keyring, Fingerprint) {
        let k = crate::storage::EncryptionKey(*key);
        let mut ring = Keyring::default();
        let fp = ring.insert("k", &k).fingerprint();
        (ring, fp)
    }

    #[test]
    fn a_bucket_without_evidence_is_written() {
        let s = KeySentinel::default();
        assert!(s.needs_probe("b"));
        assert_eq!(s.refusal("b", &Keyring::default(), None), None);
        s.probed("b", Some(vec![]));
        assert!(!s.needs_probe("b"));
        assert_eq!(s.refusal("b", &Keyring::default(), None), None);
    }

    /// The case of the report: a proxy without a key, and a bucket that
    /// another proxy encrypts. The refusal names the fingerprint, the
    /// object and that the write would be plaintext.
    #[test]
    fn a_keyless_proxy_refuses_a_bucket_with_a_sealed_object() {
        let (_, fp) = ring_with(&[7; 32]);
        let s = KeySentinel::default();
        s.sighted("b", fp, "integrity/report.html");
        let msg = s.refusal("b", &Keyring::default(), None).expect("refused");
        assert!(msg.contains(&hex::encode(fp)), "{msg}");
        assert!(msg.contains("integrity/report.html"), "{msg}");
        assert!(msg.contains("without proxy encryption"), "{msg}");
        assert!(msg.contains(KEY_NOT_HELD), "{msg}");
        assert_eq!(s.refusal("other", &Keyring::default(), None), None);
    }

    /// A key in the ring (the write key, or a read-only legacy key after a
    /// rotation or a move to native SSE) is not foreign.
    #[test]
    fn a_key_in_the_ring_is_not_foreign() {
        let (ring, fp) = ring_with(&[7; 32]);
        let s = KeySentinel::default();
        s.sighted("b", fp, "x");
        assert_eq!(s.refusal("b", &ring, Some("k")), None);
        assert_eq!(s.refusal("b", &ring, None), None);
        let (other, _) = ring_with(&[8; 32]);
        let msg = s.refusal("b", &other, Some("mine")).expect("refused");
        assert!(msg.contains("under its key 'mine'"), "{msg}");
    }

    #[test]
    fn a_sighting_expires() {
        let (_, fp) = ring_with(&[7; 32]);
        let s = KeySentinel::default();
        let old = Instant::now()
            .checked_sub(SIGHTING_TTL + Duration::from_secs(1))
            .expect("monotonic clock far enough from its origin");
        s.sighted_at("b", fp, "x", old);
        assert_eq!(s.refusal("b", &Keyring::default(), None), None);
    }

    #[test]
    fn a_failed_probe_holds_for_a_short_time() {
        let s = KeySentinel::default();
        s.probed("b", None);
        assert!(!s.needs_probe("b"));
        assert!(s.needs_probe_at("b", Instant::now() + FAILED_PROBE_TTL));
        s.probed("b", Some(vec![]));
        assert!(!s.needs_probe_at("b", Instant::now() + FAILED_PROBE_TTL));
        assert!(s.needs_probe_at("b", Instant::now() + PROBE_TTL));
    }

    /// The truth table of the claim rule.
    #[test]
    fn claim_step_truth_table() {
        let (a, b, c) = ([1; FP_LEN], [2; FP_LEN], [3; FP_LEN]);
        // The ring holds a and b; c is another proxy's key.
        let held = |fp: &Fingerprint| *fp == a || *fp == b;
        use ClaimStep::*;
        assert_eq!(claim_step(None, held, None), Keep);
        assert_eq!(claim_step(None, held, Some(a)), Create(a));
        assert_eq!(claim_step(Some((Some(a), "v")), held, Some(a)), Keep);
        // Rotation: the claimed key is still in the ring.
        assert_eq!(
            claim_step(Some((Some(a), "v")), held, Some(b)),
            Replace(b, "v".into())
        );
        // The bucket stops encrypting: the owner removes its claim.
        assert_eq!(
            claim_step(Some((Some(a), "v")), held, None),
            Remove("v".into())
        );
        // Another proxy's key: refused, writing plaintext or another key.
        assert_eq!(claim_step(Some((Some(c), "v")), held, None), Refuse(c));
        assert_eq!(claim_step(Some((Some(c), "v")), held, Some(a)), Refuse(c));
        // An unreadable claim.
        assert_eq!(claim_step(Some((None, "v")), held, None), Keep);
        assert_eq!(
            claim_step(Some((None, "v")), held, Some(a)),
            Replace(a, "v".into())
        );
    }

    #[test]
    fn a_claim_label_round_trips() {
        let fp = [0xab; FP_LEN];
        assert_eq!(claim_label(&fp), "fp-abababababababab");
        assert_eq!(parse_claim_label(&claim_label(&fp)), Some(fp));
        assert_eq!(parse_claim_label(" fp-abababababababab\n"), Some(fp));
        for bad in [
            "",
            "fp-",
            "fp-abab",
            "abababababababab",
            "fp-zzabababababababab",
        ] {
            assert_eq!(parse_claim_label(bad), None, "{bad}");
        }
    }

    /// A claim of another proxy's key refuses the write, whatever the
    /// sightings say; a claim of a key in the ring does not.
    #[test]
    fn a_foreign_claim_refuses() {
        let (ring, own) = ring_with(&[7; 32]);
        let s = KeySentinel::default();
        s.claim_seen("b", Some(own), true);
        assert_eq!(s.refusal("b", &ring, Some("k")), None);
        s.claim_seen("b", Some([9; FP_LEN]), false);
        let msg = s.refusal("b", &ring, Some("k")).expect("refused");
        assert!(msg.contains("key claim"), "{msg}");
        assert!(msg.contains(&hex::encode([9u8; FP_LEN])), "{msg}");
    }

    #[test]
    fn the_first_foreign_key_is_stable() {
        let now = Instant::now();
        let seen = HashMap::from([
            ([2; FP_LEN], (now, "b".to_string())),
            ([1; FP_LEN], (now, "a".to_string())),
            ([0; FP_LEN], (now, "held".to_string())),
        ]);
        let f = first_foreign(&seen, |fp| *fp == [0; FP_LEN]).unwrap();
        assert_eq!(f, ([1; FP_LEN], "a".to_string()));
    }
}
