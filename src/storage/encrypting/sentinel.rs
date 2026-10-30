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
//! Evidence comes from two places:
//!
//! 1. **Sightings**, at no cost: every sealed object that this wrapper reads
//!    (GET, HEAD, a listing with metadata, the reference fence before a
//!    delta write) records the fingerprint of its key.
//! 2. **A probe**, before the first write to a bucket and then every
//!    [`PROBE_TTL`]: one listing page from the root, the listing tokens of
//!    that page (S3: one more LIST of the facts namespace) and a HEAD of a
//!    few baselines and objects spread over the page. It stops at the first
//!    key that this proxy does not have.
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
        e.seen
            .retain(|_, (at, _)| now.duration_since(*at) < SIGHTING_TTL);
        let foreign = first_foreign(&e.seen, |fp| ring.get(fp).is_some())?;
        Some(refusal_message(bucket, &foreign.0, &foreign.1, write_key))
    }
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
    object: &str,
    write_key: Option<&str>,
) -> String {
    let how = match write_key {
        None => "without proxy encryption".to_string(),
        Some(id) => format!("under its key '{id}'"),
    };
    format!(
        "bucket '{bucket}' holds objects that were encrypted {KEY_NOT_HELD} (key fp-{}, \
         for example '{object}'). This proxy would write {how}, and the proxy that holds \
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
