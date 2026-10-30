// SPDX-License-Identifier: BUSL-1.1

//! Transparent encryption at rest for any StorageBackend (issue #120).
//!
//! `EncryptingBackend<B>` wraps one raw backend. A write to a bucket that
//! encrypts stores a format-v2 body (`v2.rs`: per-object keys, 64 KiB
//! authenticated frames, key commitment) and sealed metadata (`seal.rs`:
//! the facts bound to the object's location, no hashes in clear). Every
//! read of a body or of metadata goes through THE read rule
//! (`present_raw`): a sealed object opens with the key its fingerprint
//! names; an object without a seal is served only where the bucket may
//! serve plaintext (it writes plaintext, or `plaintext_reads: serve`).
//! Callers above the wrapper see the logical object, with the markers
//! `dg-encrypted: v2` and `dg-encryption-key-id: <id>`; the raw stored
//! metadata never leaves the wrapper.
//!
//! # Layout
//!
//! `v2.rs`: the format (pure). `seal.rs`: sealed metadata, the keyring by
//! fingerprint and the read rule (pure). `format.rs`: the key type and the
//! marker names. `witness.rs`: the node-local gate of the encryption
//! witness. This file: the wrapper.

#[cfg(test)]
mod bucket_tests;
mod format;
pub(crate) mod seal;
mod sentinel;
#[cfg(test)]
mod sentinel_tests;
#[cfg(test)]
mod tests;
pub(crate) mod v2;
mod witness;

pub use seal::{ENCRYPTION_MARKER_KEYS, PRESENTED_MARKER_VALUE};

pub use format::*;
pub use sentinel::KeySentinel;
pub use witness::{WitnessGate, WitnessGateState, WITNESS_BACKEND_DEFAULT};

/// The id of a key that the config does not name: `fp-` and the hex of
/// its v2 fingerprint (issue #120). It depends on the key alone, so the
/// same key has the same id on every backend and a rename changes nothing.
pub fn fingerprint_label(key: &EncryptionKey) -> String {
    sentinel::label(&v2::SiteKey::new(key).fingerprint())
}

use super::io_to_storage_error;
use super::list_size_cache::{ListedSize, ResolveListed};
use super::traits::{
    BulkListing, ByteStream, DelegatedListResult, LiteScanResult, MultipartUpload, ObjectRead,
    RangeRead, ReferenceSource, StorageBackend, StorageError, StoredObject, UploadedPart,
};
use crate::deltaglider::spool::SpoolBudget;
use crate::types::FileMetadata;
use arc_swap::ArcSwap;
use bytes::Bytes;
use std::sync::Arc;

/// Drop the at-rest encryption markers (and the seal) from an object's
/// user metadata, so a SUBSEQUENT write decides them fresh. The single home
/// of the marker set ([`ENCRYPTION_MARKER_KEYS`]): used by the re-encrypt
/// job, the delta-passthrough ship and the copy paths above this module.
pub(crate) fn strip_encryption_markers(
    user_metadata: &mut std::collections::HashMap<String, String>,
) {
    user_metadata.retain(|k, _| !seal::is_marker_key(k));
}

/// The wrapper OWNS its markers: a write stores only its own decision. Any
/// copy the caller carries (a client's `x-amz-meta-dg-sealed`, a sync tool
/// replaying GET metadata into a PUT) is dropped first.
fn without_markers(metadata: &FileMetadata) -> FileMetadata {
    let mut meta = metadata.clone();
    strip_encryption_markers(&mut meta.user_metadata);
    meta.listing_token = Default::default();
    meta
}

/// A spool refusal is back-pressure: `Throttled` reaches the client as a
/// retryable 503 SlowDown.
fn spool_error(e: std::io::Error) -> StorageError {
    if e.kind() == crate::deltaglider::spool::CONTENDED {
        StorageError::Throttled(e.to_string())
    } else {
        io_to_storage_error(e)
    }
}

/// Controls what the wrapper does on writes. Reads always decrypt
/// tagged objects regardless of this flag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WriteMode {
    /// Default: encrypt writes when `key` is Some, pass through
    /// when None.
    #[default]
    Encrypt,
    /// Writes always skip encryption — even when a `key` is present.
    /// Used by the decrypt-only shim during a proxy-AES → native-SSE
    /// mode transition: the wrapper keeps the legacy proxy key so
    /// old objects still decrypt on read, but new writes go through
    /// unencrypted (the inner `S3Backend` is already configured with
    /// native SSE and encrypts server-side on its own).
    PassThrough,
    /// The backend's mode encrypts (`aes256-gcm-proxy`) but no key is
    /// configured. Every object write fails with
    /// [`StorageError::Encryption`]: a backend that the operator set to
    /// encrypt never stores plaintext (issue #116). Reads keep working
    /// (plaintext objects, and the legacy key when one is set).
    Refuse,
}

/// The error of a write under [`WriteMode::Refuse`].
pub(crate) const NO_KEY_WRITE_REFUSAL: &str = "this backend is in aes256-gcm-proxy mode but no \
     key is configured, so the proxy refuses the write instead of storing plaintext. Set \
     encryption.key (or the DGP_*_ENCRYPTION_KEY env var) for the backend, or change its mode";

/// Hot-reloadable encryption configuration of one raw backend.
///
/// `key` is the site key that writes of this backend use. Every key here
/// (`key`, `legacy_key`, `keyring`, the bucket overrides' keys) goes into
/// one ring indexed by its fingerprint: a read opens an object with the key
/// that the object's seal names by fingerprint, never by id and never by
/// trying keys in turn. Key ids are labels in the ring only: they are not
/// stored with an object, and callers see the id of the key that opened it
/// as `dg-encryption-key-id`.
///
/// `legacy_key` holds the PREVIOUS key after a mode transition (for
/// example proxy AES to native SSE), so the history stays readable
/// without a rewrite up front.
#[derive(Default)]
pub struct EncryptionConfig {
    pub key: Option<EncryptionKey>,
    /// The label of `key`. Without an explicit id in the YAML, the engine
    /// uses [`fingerprint_label`] of the key.
    pub key_id: Option<String>,
    /// Write-path policy. `Encrypt` (default) follows the key:
    /// encrypt when Some, passthrough when None. `PassThrough`
    /// forces passthrough regardless of key presence — used by the
    /// decrypt-only shim.
    pub write_mode: WriteMode,
    /// The PREVIOUS key after a mode transition: in the ring for reads,
    /// never used to write.
    pub legacy_key: Option<EncryptionKey>,
    /// Id paired with `legacy_key`. Same shape rules as `key_id`.
    pub legacy_key_id: Option<String>,
    /// Per-bucket write policy (issue #116), indexed by the REAL bucket
    /// name that this wrapper receives (the router has already mapped a
    /// virtual name or an alias to it). A bucket that is not here writes
    /// with the backend's own `key` / `write_mode` above.
    pub buckets: std::collections::HashMap<String, BucketKeys>,
    /// Every other key on this raw backend (the bucket overrides' keys and
    /// legacy keys), indexed by key id. They are in the ring, so a bucket
    /// reads its history under the backend's key, a rotated key, or a
    /// removed override.
    pub keyring: std::collections::HashMap<String, EncryptionKey>,
    /// `plaintext_reads: serve` of the backend's own encryption: a read of
    /// a bucket without an override may serve an unencrypted object.
    pub serve_plaintext: bool,
    /// Every key above, by fingerprint (built on first use).
    pub(crate) ring: std::sync::OnceLock<seal::Keyring>,
}

/// One raw backend's policy slot and the config an engine publishes into it.
pub type PolicySlot = (
    String,
    Arc<ArcSwap<EncryptionConfig>>,
    Arc<EncryptionConfig>,
);

/// The encryption-policy slots of the raw backends, shared by an engine
/// and the engine that replaces it (issue #116 review D). An engine
/// rebuild (a config apply) builds new wrappers; without a shared slot, a
/// copy that still runs on the old engine (a replication or lifecycle
/// run, a multipart upload) kept writing a bucket in the form it had
/// before the apply. The new engine publishes its configs into the shared
/// slots when it is installed, so every engine of the lineage writes with
/// the current per-bucket policy.
#[derive(Default)]
pub struct EncryptionPolicies {
    slots: parking_lot::Mutex<std::collections::HashMap<String, Arc<ArcSwap<EncryptionConfig>>>>,
    /// The witness gate that every wrapper of the lineage consults.
    witness: Arc<WitnessGate>,
    /// The foreign-key sentinel of each slot (same keys as `slots`).
    sentinels: parking_lot::Mutex<std::collections::HashMap<String, Arc<KeySentinel>>>,
}

impl EncryptionPolicies {
    /// The witness gate of every engine that shares these slots.
    pub fn witness_gate(&self) -> Arc<WitnessGate> {
        self.witness.clone()
    }

    /// The foreign-key sentinel of slot `key`: an engine rebuild keeps the
    /// evidence of the raw backend that it does not change.
    pub fn sentinel(&self, key: &str) -> Arc<KeySentinel> {
        self.sentinels
            .lock()
            .entry(key.to_string())
            .or_default()
            .clone()
    }

    /// The slot of `key` and remember `cfg` for [`Self::publish`]. A new
    /// slot starts with `cfg` (no engine reads it yet); an existing one
    /// keeps the value that the running engine uses until the publish.
    pub fn slot(
        &self,
        key: &str,
        cfg: EncryptionConfig,
        published: &mut Vec<PolicySlot>,
    ) -> Arc<ArcSwap<EncryptionConfig>> {
        let cfg = Arc::new(cfg);
        let slot = self
            .slots
            .lock()
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(ArcSwap::new(cfg.clone())))
            .clone();
        published.push((key.to_string(), slot.clone(), cfg));
        slot
    }

    /// Store each config into its slot, and forget the slots that are not
    /// in `published` (an engine that still holds one keeps its `Arc`).
    pub fn publish(&self, published: &[PolicySlot]) {
        for (_, slot, cfg) in published {
            slot.store(cfg.clone());
        }
        self.slots
            .lock()
            .retain(|k, _| published.iter().any(|(p, _, _)| p == k));
        self.sentinels
            .lock()
            .retain(|k, _| published.iter().any(|(p, _, _)| p == k));
    }
}

/// The write policy of one bucket with its own `encryption` override.
#[derive(Clone, Default)]
pub struct BucketKeys {
    /// The key new writes use (`None` with `WriteMode::Encrypt` = plaintext).
    pub key: Option<EncryptionKey>,
    pub key_id: Option<String>,
    pub write_mode: WriteMode,
    /// The bucket's decrypt-only key id, if any (its key is in the keyring).
    pub legacy_key_id: Option<String>,
    /// `plaintext_reads: serve` of the bucket's override.
    pub serve_plaintext: bool,
    /// The native SSE headers of the bucket's writes (an S3 backend reads
    /// them per request through [`EncryptionConfig::native_for`]).
    /// `None` for a proxy-AES or plaintext override.
    pub native: crate::storage::NativeEncryptionConfig,
}

impl EncryptionConfig {
    /// Every key of this raw backend by fingerprint, with its id: the
    /// primary, the legacy key, and the keyring (bucket keys, retired keys).
    pub(crate) fn ring(&self) -> &seal::Keyring {
        self.ring.get_or_init(|| {
            let mut ring = seal::Keyring::default();
            if let (Some(k), Some(id)) = (&self.key, &self.key_id) {
                ring.insert(id, k);
            }
            if let (Some(k), Some(id)) = (&self.legacy_key, &self.legacy_key_id) {
                ring.insert(id, k);
            }
            for (id, k) in &self.keyring {
                ring.insert(id, k);
            }
            for b in self.buckets.values() {
                if let (Some(k), Some(id)) = (&b.key, &b.key_id) {
                    ring.insert(id, k);
                }
            }
            ring
        })
    }

    /// The prepared key of `key` (from the ring; a key that is not there,
    /// which the engine never builds, is prepared on the spot).
    fn site_of(&self, key: &EncryptionKey) -> Arc<v2::SiteKey> {
        let site = v2::SiteKey::new(key);
        match self.ring().get(&site.fingerprint()) {
            Some(entry) => entry.site.clone(),
            None => Arc::new(site),
        }
    }

    /// The native SSE mode of a bucket with its own override, or `None`
    /// when the bucket follows the backend.
    pub fn native_for(&self, bucket: &str) -> Option<crate::storage::NativeEncryptionConfig> {
        self.buckets.get(bucket).map(|b| b.native.clone())
    }
}

/// A whole stream in one buffer.
async fn collect_stream(stream: ByteStream) -> Result<Vec<u8>, StorageError> {
    use futures::TryStreamExt;
    let parts: Vec<Bytes> = stream.try_collect().await?;
    let mut buf = Vec::with_capacity(parts.iter().map(|b| b.len()).sum());
    for p in parts {
        buf.extend_from_slice(&p);
    }
    Ok(buf)
}

/// Transparent encryption wrapper around any `StorageBackend`.
pub struct EncryptingBackend<B: StorageBackend> {
    inner: B,
    config: Arc<ArcSwap<EncryptionConfig>>,
    /// The raw backend's name and the witness gate (multi-instance).
    witness: Option<(String, Arc<WitnessGate>)>,
    /// The foreign-key sentinel (`sentinel.rs`); `None` = no check.
    sentinel: Option<Arc<KeySentinel>>,
}

impl<B: StorageBackend> EncryptingBackend<B> {
    pub fn new(inner: B, config: Arc<ArcSwap<EncryptionConfig>>) -> Self {
        Self {
            inner,
            config,
            witness: None,
            sentinel: None,
        }
    }

    /// Refuse writes to a bucket that holds objects sealed with a key that
    /// this backend does not have (`sentinel.rs`).
    pub fn with_sentinel(mut self, sentinel: Arc<KeySentinel>) -> Self {
        self.sentinel = Some(sentinel);
        self
    }

    /// THE witness check (issue #116 P3): a node whose encryption of this
    /// bucket disagrees with the other instances (or cannot confirm that it
    /// agrees) never writes it, also not a metadata rewrite or a folder
    /// marker (an S3 self-copy carries this node's SSE headers). Every
    /// mutating method calls it first (source guard
    /// `every_write_method_checks_the_witness`).
    fn check_witness(&self, bucket: &str) -> Result<(), StorageError> {
        match self
            .witness
            .as_ref()
            .and_then(|(name, gate)| gate.refusal(name, bucket))
        {
            Some(msg) => Err(StorageError::WritesRefused(msg)),
            None => Ok(()),
        }
    }

    /// THE write check: the witness, then the foreign-key sentinel. Every
    /// mutating method calls it first (source guard
    /// `every_write_method_checks_the_witness`).
    async fn check_write(&self, bucket: &str) -> Result<(), StorageError> {
        self.check_write_unless(bucket, false).await
    }

    /// [`Self::check_write`]; `delete`: the write only removes an object,
    /// which the sentinel allows (the proxy must stay able to remove what
    /// it wrote by mistake).
    async fn check_write_unless(&self, bucket: &str, delete: bool) -> Result<(), StorageError> {
        self.check_witness(bucket)?;
        let Some(sentinel) = self.sentinel.as_ref().filter(|_| !delete) else {
            return Ok(());
        };
        let cfg = self.config.load_full();
        // A bucket in `WriteMode::Refuse` stores nothing that a key could
        // read or not: its content writes fail on their own, and a folder
        // marker or a metadata rewrite keeps its old behaviour.
        let Ok(site) = self.write_key(bucket) else {
            return Ok(());
        };
        let write = site.as_ref().map(|s| s.fingerprint());
        let held = |fp: &sentinel::Fingerprint| cfg.ring().get(fp).is_some();
        let now = std::time::Instant::now;
        let mut refusal = sentinel.refusal(bucket, now(), held);
        if refusal.is_none()
            && (sentinel.probe_due(bucket, now()) || sentinel.claim_due(bucket, now(), held, write))
        {
            let lock = sentinel.lock(bucket);
            let _one = lock.lock().await;
            if sentinel.probe_due(bucket, now()) {
                let found = match self.probe_keys(bucket, cfg.ring()).await {
                    Ok(found) => Some(found),
                    Err(StorageError::BucketNotFound(_) | StorageError::NotFound(_)) => {
                        Some(Vec::new())
                    }
                    Err(e) => {
                        tracing::debug!("encryption: key probe of bucket '{bucket}': {e}");
                        None
                    }
                };
                let failed = found.is_none();
                if sentinel.probed(bucket, found, now()) {
                    tracing::warn!(
                        "encryption: the key probe of bucket '{bucket}' failed; writes go ahead \
                         on the other evidence, and the probe is tried again in {}s",
                        sentinel::FAILED_TTL.as_secs()
                    );
                } else if !failed {
                    tracing::debug!("encryption: probed bucket '{bucket}'");
                }
            }
            // The probe and the sightings first: a writer never claims a
            // bucket that holds another key.
            refusal = sentinel.refusal(bucket, now(), held);
            if refusal.is_none() && sentinel.claim_due(bucket, now(), held, write) {
                if let Err(e) = self.settle_claim(sentinel, bucket, held, write).await {
                    if sentinel.claim_failed(bucket, now()) {
                        tracing::warn!(
                            "encryption: the key claim of bucket '{bucket}' could not be read or \
                             written, writes go ahead on the other evidence and the claim is \
                             tried again in {}s: {e}",
                            sentinel::FAILED_TTL.as_secs()
                        );
                    }
                }
                refusal = sentinel.refusal(bucket, now(), held);
            }
        }
        let Some(r) = refusal else {
            return Ok(());
        };
        let write_key = site.as_ref().map(|site| {
            cfg.ring()
                .get(&site.fingerprint())
                .map_or_else(|| sentinel::label(&site.fingerprint()), |e| e.id.clone())
        });
        Err(StorageError::KeyNotHeld {
            key: sentinel::label(&r.fingerprint),
            write: true,
            detail: sentinel::refusal_detail(bucket, &r, write_key.as_deref()),
        })
    }

    /// Read the key claim of `bucket` and apply [`sentinel::claim_action`]:
    /// create, replace or remove it with a conditional write, and read it
    /// again when another writer changed it first.
    async fn settle_claim(
        &self,
        sentinel: &KeySentinel,
        bucket: &str,
        held: impl Fn(&sentinel::Fingerprint) -> bool,
        write: Option<sentinel::Fingerprint>,
    ) -> Result<(), StorageError> {
        use super::{ClaimOutcome, ClaimWrite};
        use sentinel::ClaimAction;
        let write_label = write.as_ref().map(sentinel::label);
        for _ in 0..3 {
            let claim = self.inner.read_key_claim(bucket).await?;
            let parsed = claim
                .as_ref()
                .map(|c| sentinel::parse_label(&c.fingerprint));
            let version = claim.as_ref().map_or("", |c| c.version.as_str());
            let now = std::time::Instant::now;
            let (op, after) = match (sentinel::claim_action(parsed, &held, write), &write_label) {
                (ClaimAction::Keep | ClaimAction::Refuse(_), _) => {
                    let fp = parsed.flatten();
                    sentinel.claim_seen(bucket, fp, fp.is_some() && fp == write, now());
                    return Ok(());
                }
                (ClaimAction::Create, Some(label)) => (ClaimWrite::Create(label), write),
                (ClaimAction::Replace, Some(label)) => (
                    ClaimWrite::Replace {
                        fingerprint: label,
                        version,
                    },
                    write,
                ),
                (ClaimAction::Remove, _) => (ClaimWrite::Remove { version }, None),
                (ClaimAction::Create | ClaimAction::Replace, None) => {
                    unreachable!("claim_action creates or replaces only for a write key")
                }
            };
            match self.inner.write_key_claim(bucket, op).await? {
                ClaimOutcome::Written => {
                    tracing::info!(
                        "encryption: key claim of bucket '{bucket}' is now {}",
                        after.as_ref().map_or("none".to_string(), sentinel::label)
                    );
                    sentinel.claim_seen(bucket, after, true, now());
                    return Ok(());
                }
                ClaimOutcome::Lost => continue,
            }
        }
        Err(StorageError::Throttled(format!(
            "the key claim of bucket '{bucket}' changed three times while this proxy wrote it"
        )))
    }

    /// The fingerprints of the keys that sealed a sample of `bucket`, with
    /// an object each: one bounded flat listing page from the root (whose
    /// raw metadata already carries the seal on a filesystem), the listing
    /// tokens of that page, then a HEAD of a few baselines and of objects
    /// spread over the page. Stops at the first key that `ring` lacks. A
    /// backend that cannot list a bounded page is not sampled.
    async fn probe_keys(
        &self,
        bucket: &str,
        ring: &seal::Keyring,
    ) -> Result<Vec<(sentinel::Fingerprint, String)>, StorageError> {
        let Some(page) = self
            .inner
            .list_objects_delegated(bucket, "", None, sentinel::PROBE_LIST, None)
            .await?
        else {
            return Ok(Vec::new());
        };
        let mut found = Vec::new();
        let foreign = |found: &[(sentinel::Fingerprint, String)]| {
            found.iter().any(|(fp, _)| ring.get(fp).is_none())
        };
        for (key, meta) in &page.objects {
            if let Ok(Some(sealed)) = seal::sealed_of(meta) {
                found.push((sealed.fp, key.clone()));
            }
        }
        let tokens = self.inner.listing_tokens(bucket, &page.objects).await;
        for ((key, _), token) in page.objects.iter().zip(tokens) {
            if let Some(fp) = token.and_then(|t| v2::token_fingerprint(&t.token)) {
                found.push((fp, key.clone()));
            }
        }
        if foreign(&found) || !found.is_empty() && page.objects.len() == found.len() {
            return Ok(found);
        }
        // Baselines first: one reference stands for its whole deltaspace.
        let mut heads: Vec<(String, StoredObject<'_>, String)> = page
            .reference_prefixes
            .iter()
            .flatten()
            .take(sentinel::PROBE_HEADS / 2)
            .map(|prefix| {
                let key = object_name(prefix, StoredObject::Reference);
                (prefix.clone(), StoredObject::Reference, key)
            })
            .collect();
        let files: Vec<_> = page
            .objects
            .iter()
            .filter(|(key, _)| !key.ends_with('/'))
            .collect();
        let room = sentinel::PROBE_HEADS - heads.len();
        let step = files.len().div_ceil(room.max(1)).max(1);
        for (key, meta) in files.iter().step_by(step).take(room) {
            let (prefix, name) = key.rsplit_once('/').unwrap_or(("", key.as_str()));
            let object = if meta.is_delta() {
                StoredObject::Delta(name)
            } else {
                StoredObject::Passthrough(name)
            };
            heads.push((prefix.to_string(), object, key.to_string()));
        }
        for (prefix, object, key) in heads {
            let raw = match object {
                StoredObject::Reference => self.inner.get_reference_metadata(bucket, &prefix).await,
                StoredObject::Delta(f) => self.inner.get_delta_metadata(bucket, &prefix, f).await,
                StoredObject::Passthrough(f) => {
                    self.inner
                        .get_passthrough_metadata(bucket, &prefix, f)
                        .await
                }
            };
            // An object deleted since the listing is no evidence either way.
            let Ok(raw) = raw else { continue };
            if let Ok(Some(sealed)) = seal::sealed_of(&raw) {
                found.push((sealed.fp, key));
                if foreign(&found) {
                    break;
                }
            }
        }
        Ok(found)
    }

    /// Refuse the writes that the witness gate names for raw backend `name`.
    pub fn with_witness(mut self, name: &str, gate: Arc<WitnessGate>) -> Self {
        self.witness = Some((name.to_string(), gate));
        self
    }
}

/// Where a stored object lives, for its seal.
fn object_of<'a>(variant: super::traits::ObjectVariant, filename: &'a str) -> StoredObject<'a> {
    match variant {
        super::traits::ObjectVariant::Delta => StoredObject::Delta(filename),
        super::traits::ObjectVariant::Passthrough => StoredObject::Passthrough(filename),
    }
}

/// `meta` named for the stored object: a delta or passthrough stores its
/// own filename as its original name, so a scan (which returns metadata
/// without keys) finds the location of its seal.
fn named(meta: &FileMetadata, object: StoredObject<'_>) -> FileMetadata {
    let mut m = meta.clone();
    if let StoredObject::Delta(f) | StoredObject::Passthrough(f) = object {
        m.original_name = f.to_string();
    }
    m
}

/// The stored object of a listed key: its folder, and the object in it.
fn listed_object<'a>(key: &'a str, meta: &FileMetadata) -> (&'a str, StoredObject<'a>) {
    let (prefix, name) = key.rsplit_once('/').unwrap_or(("", key));
    (prefix, stored_object_of(meta, name))
}

/// The stored object that `meta` describes, named `name` in its folder.
fn stored_object_of<'a>(meta: &FileMetadata, name: &'a str) -> StoredObject<'a> {
    if meta.is_reference() {
        StoredObject::Reference
    } else if meta.is_delta() {
        StoredObject::Delta(name)
    } else {
        StoredObject::Passthrough(name)
    }
}

/// The key of a stored object, for a message.
fn object_name(prefix: &str, object: StoredObject<'_>) -> String {
    let name = match object {
        StoredObject::Reference => "reference.bin",
        StoredObject::Delta(f) | StoredObject::Passthrough(f) => f,
    };
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

/// A folder marker: never sealed, always readable.
fn is_folder(object: StoredObject<'_>) -> bool {
    matches!(object, StoredObject::Passthrough(f) if f.is_empty() || f.ends_with('/'))
}

/// The listing facts of sealed `facts`, or `None` without an ETag.
fn listing_facts_of(facts: &v2::SealedFacts) -> Option<v2::ListingFacts> {
    let (etag_digest, parts) = match (facts.multipart, facts.md5) {
        (Some((d, n)), _) => (d, n),
        (None, Some(md5)) => (md5, 0),
        (None, None) => return None,
    };
    Some(v2::ListingFacts {
        size: facts.file_size,
        etag_digest,
        parts,
    })
}

/// The listing-token maker of a sealed object (see `ListingTokenMaker`).
fn token_maker(
    site: Arc<v2::SiteKey>,
    bucket: &str,
    facts: &v2::SealedFacts,
) -> crate::types::ListingTokenMaker {
    let Some(lf) = listing_facts_of(facts) else {
        return Default::default();
    };
    let bucket = bucket.to_string();
    crate::types::ListingTokenMaker::new(move |key, etag, size| {
        site.listing_token(
            &v2::ListedAt {
                bucket: &bucket,
                stored_key: key,
                stored_etag: super::list_size_cache::bare_etag(etag),
                stored_size: size,
            },
            &lf,
        )
        .ok()
    })
}

/// A stored object, read: its presented metadata, and the keys of its
/// body when it is sealed.
struct Opened {
    meta: FileMetadata,
    sealed: Option<SealedBody>,
}

struct SealedBody {
    site: Arc<v2::SiteKey>,
    salt: [u8; v2::SALT_LEN],
    len: u64,
}

impl SealedBody {
    fn opener(&self) -> v2::ContentOpener {
        self.site.opener(self.salt)
    }
}

/// The caller's pin `source` against the presented metadata of the source
/// now: another version is the same `PreconditionFailed` the inner backends
/// return, never a copy of a version the caller did not read.
fn check_copy_pin(
    source: &FileMetadata,
    now: &FileMetadata,
    src: &super::traits::ObjectAt<'_>,
) -> Result<(), StorageError> {
    if super::traits::same_stored_generation(source, now) {
        return Ok(());
    }
    Err(StorageError::PreconditionFailed(format!(
        "{}/{}/{} changed before the copy",
        src.bucket, src.prefix, src.filename
    )))
}

/// Refuse a plaintext stream that begins with a `DGE<digit>` magic: a
/// proxy-encrypted body whose metadata was stripped. Joins short leading
/// chunks; never polls an ended stream.
fn sniff_dge_magic(inner: ByteStream) -> ByteStream {
    enum State {
        Initial(ByteStream),
        Passthrough(ByteStream),
        Done,
    }
    Box::pin(futures::stream::unfold(
        State::Initial(inner),
        |st| async move {
            use futures::StreamExt;
            match st {
                State::Initial(mut inner) => {
                    let mut head = bytes::BytesMut::new();
                    let mut ended = false;
                    while head.len() < v2::MAGIC.len() {
                        match inner.next().await {
                            Some(Ok(chunk)) => head.extend_from_slice(&chunk),
                            Some(Err(e)) => return Some((Err(e), State::Done)),
                            None => {
                                ended = true;
                                break;
                            }
                        }
                    }
                    if head.is_empty() {
                        return None;
                    }
                    if v2::has_dge_magic(&head) {
                        return Some((Err(stripped_marker_error()), State::Done));
                    }
                    let next = if ended {
                        State::Done
                    } else {
                        State::Passthrough(inner)
                    };
                    Some((Ok(head.freeze()), next))
                }
                State::Passthrough(mut inner) => inner
                    .next()
                    .await
                    .map(|item| (item, State::Passthrough(inner))),
                State::Done => None,
            }
        },
    ))
}

pub(super) fn stripped_marker_error() -> StorageError {
    StorageError::Encryption(
        "the object body is proxy-encrypted but its metadata names no encryption: the \
         metadata was stripped (for example by a backup or a copy without metadata). The \
         proxy refuses to serve ciphertext as plaintext"
            .into(),
    )
}

/// The plaintext of a whole sealed body as a stream: the header is read and
/// checked first, then the frames are decrypted one by one.
async fn open_body_stream(
    mut stream: ByteStream,
    body: &SealedBody,
) -> Result<ByteStream, StorageError> {
    use futures::StreamExt;
    let mut head: Vec<u8> = Vec::with_capacity(v2::HEADER_LEN);
    while head.len() < v2::HEADER_LEN {
        match stream.next().await {
            Some(chunk) => head.extend_from_slice(&chunk?),
            None => {
                return Err(StorageError::Encryption(
                    "the object body ends inside its header: it was truncated".into(),
                ))
            }
        }
    }
    let opener = body.opener();
    opener.check_header(&head[..v2::HEADER_LEN])?;
    let rest = Bytes::from(head.split_off(v2::HEADER_LEN));
    let frames: ByteStream = Box::pin(
        futures::stream::once(async move { Ok(rest) })
            .filter(|b: &Result<Bytes, StorageError>| {
                futures::future::ready(b.as_ref().map_or(true, |b| !b.is_empty()))
            })
            .chain(stream),
    );
    Ok(v2::decrypt_frames(frames, opener, 0, body.len, 0, None))
}

impl<B: StorageBackend> EncryptingBackend<B> {
    /// THE write-side decision for one REAL bucket: the bucket's own
    /// override when it has one (issue #116), else the backend's. `Err`
    /// under [`WriteMode::Refuse`]: the write must fail, never fall back
    /// to plaintext. `None`: the write stores plaintext.
    fn write_key(&self, bucket: &str) -> Result<Option<Arc<v2::SiteKey>>, StorageError> {
        self.check_witness(bucket)?;
        let cfg = self.config.load();
        let (mode, key) = match cfg.buckets.get(bucket) {
            Some(b) => (b.write_mode, &b.key),
            None => (cfg.write_mode, &cfg.key),
        };
        match mode {
            WriteMode::PassThrough => Ok(None),
            WriteMode::Refuse if cfg.buckets.contains_key(bucket) => {
                Err(StorageError::Encryption(format!(
                    "bucket '{bucket}' is in aes256-gcm-proxy mode but no key is configured, \
                     so the proxy refuses the write instead of storing plaintext. Set the \
                     bucket's encryption.key, or change its mode"
                )))
            }
            WriteMode::Refuse => Err(StorageError::Encryption(NO_KEY_WRITE_REFUSAL.to_string())),
            WriteMode::Encrypt => Ok(key.as_ref().map(|k| cfg.site_of(k))),
        }
    }

    /// True when a write to `bucket` stores plaintext under the config
    /// (no key, or native SSE). Not the witness: a node that the witness
    /// stops still reads the bucket by its config.
    fn writes_plaintext(&self, bucket: &str) -> bool {
        let cfg = self.config.load();
        let (mode, key) = match cfg.buckets.get(bucket) {
            Some(b) => (b.write_mode, &b.key),
            None => (cfg.write_mode, &cfg.key),
        };
        match mode {
            WriteMode::PassThrough => true,
            WriteMode::Refuse => false,
            WriteMode::Encrypt => key.is_none(),
        }
    }

    /// May a read of `bucket` serve an object that is not encrypted? Yes
    /// when the bucket writes plaintext, or encrypts with
    /// `plaintext_reads: serve`.
    fn plaintext_ok(&self, bucket: &str) -> bool {
        if self.writes_plaintext(bucket) {
            return true;
        }
        let cfg = self.config.load();
        cfg.buckets
            .get(bucket)
            .map_or(cfg.serve_plaintext, |b| b.serve_plaintext)
    }

    /// True when a bucket of this backend may hold sealed objects: a key
    /// exists anywhere on it (a key of another bucket can open an object
    /// that was moved here; its seal then fails at the location). Per
    /// backend, not per bucket.
    fn has_any_key(&self) -> bool {
        !self.config.load().ring().is_empty()
    }

    /// THE read rule: the presented metadata of the stored object `raw` at
    /// (`bucket`, `prefix`, `object`). A sealed object opens with the key
    /// its fingerprint names, at its location; an object without a seal is
    /// served only where the bucket may serve plaintext.
    fn present_raw(
        &self,
        bucket: &str,
        prefix: &str,
        object: StoredObject<'_>,
        raw: &FileMetadata,
    ) -> Result<Opened, StorageError> {
        let at = seal::location(bucket, prefix, object);
        match seal::sealed_of(raw)? {
            Some(sealed) => {
                let cfg = self.config.load();
                if let Some(sentinel) = &self.sentinel {
                    if cfg.ring().get(&sealed.fp).is_none() {
                        let at = object_name(prefix, object);
                        sentinel.sighted(bucket, sealed.fp, &at, std::time::Instant::now());
                    }
                }
                let (facts, entry) = seal::open(raw, &sealed, &at, cfg.ring())?;
                Ok(Opened {
                    meta: seal::present(raw, &facts, &entry.id),
                    sealed: Some(SealedBody {
                        site: entry.site.clone(),
                        salt: sealed.salt,
                        len: seal::body_len(&facts),
                    }),
                })
            }
            None => match seal::read_verdict(raw, is_folder(object), self.plaintext_ok(bucket)) {
                seal::Unsealed::Serve => Ok(Opened {
                    meta: raw.clone(),
                    sealed: None,
                }),
                seal::Unsealed::Refuse => {
                    tracing::warn!("encryption: refused {bucket}/{prefix} {object:?}");
                    Err(seal::refusal(&at, raw))
                }
            },
        }
    }

    /// The presented metadata of a listed or scanned entry. An entry
    /// without a seal is listed as it is (a GET applies the read rule). An
    /// entry whose seal does not open is listed as stored, without its seal
    /// ([`FileMetadata::is_unopened_seal`]), and logged: one such object
    /// must not fail the listing of its whole folder, nor the re-encrypt
    /// and migrate jobs that page through it. A GET or HEAD of it fails.
    fn present_entry(
        &self,
        bucket: &str,
        prefix: &str,
        object: StoredObject<'_>,
        mut raw: FileMetadata,
    ) -> FileMetadata {
        if !raw.user_metadata.contains_key(seal::SEALED_KEY) {
            return raw;
        }
        match self.present_raw(bucket, prefix, object, &raw) {
            Ok(opened) => opened.meta,
            Err(e) => {
                tracing::warn!("listing {bucket}/{prefix}: an entry does not open: {e}");
                raw.user_metadata.remove(seal::SEALED_KEY);
                raw
            }
        }
    }

    fn present_listed(
        &self,
        bucket: &str,
        objects: Vec<(String, FileMetadata)>,
    ) -> Vec<(String, FileMetadata)> {
        objects
            .into_iter()
            .map(|(key, raw)| {
                let meta = {
                    let (prefix, object) = listed_object(&key, &raw);
                    self.present_entry(bucket, prefix, object, raw)
                };
                (key, meta)
            })
            .collect()
    }

    fn present_scanned(
        &self,
        bucket: &str,
        prefix: &str,
        metas: Vec<FileMetadata>,
    ) -> Vec<FileMetadata> {
        metas
            .into_iter()
            .map(|raw| {
                let name = raw.original_name.clone();
                let object = stored_object_of(&raw, &name);
                self.present_entry(bucket, prefix, object, raw)
            })
            .collect()
    }

    /// The stored metadata of a write of `meta` (body of `body_len` bytes)
    /// sealed with fresh object keys of `site`, and the content sealer.
    fn seal_new(
        &self,
        site: &Arc<v2::SiteKey>,
        bucket: &str,
        prefix: &str,
        object: StoredObject<'_>,
        meta: &FileMetadata,
        body_len: u64,
    ) -> Result<(v2::ContentSealer, FileMetadata), StorageError> {
        let (sealer, keys) = site.fresh_object();
        let mut stored = seal::seal(
            &named(meta, object),
            &seal::location(bucket, prefix, object),
            &keys,
        )?;
        let facts = seal::facts_of(&named(meta, object))?;
        if seal::body_len(&facts) != body_len {
            return Err(StorageError::Encryption(format!(
                "cannot seal {bucket}/{prefix} {object:?}: its metadata names a body of {} \
                 bytes, the write has {body_len}",
                seal::body_len(&facts)
            )));
        }
        stored.listing_token = token_maker(site.clone(), bucket, &facts);
        Ok((sealer, stored))
    }

    /// The stored body and metadata of a buffered write.
    fn seal_bytes(
        &self,
        bucket: &str,
        prefix: &str,
        object: StoredObject<'_>,
        data: &Bytes,
        meta: &FileMetadata,
    ) -> Result<(Bytes, FileMetadata), StorageError> {
        match self.write_key(bucket)? {
            Some(site) => {
                let (sealer, stored) =
                    self.seal_new(&site, bucket, prefix, object, meta, data.len() as u64)?;
                Ok((Bytes::from(sealer.seal_all(data)?), stored))
            }
            None => Ok((data.clone(), without_markers(meta))),
        }
    }

    /// The stored metadata of a metadata-only rewrite (the body stays):
    /// re-sealed under the stored object's own keys, at its location. A
    /// stored object without a seal keeps no markers.
    fn reseal(
        &self,
        bucket: &str,
        prefix: &str,
        object: StoredObject<'_>,
        meta: &FileMetadata,
        raw: &FileMetadata,
    ) -> Result<FileMetadata, StorageError> {
        let Some(sealed) = seal::sealed_of(raw)? else {
            if raw.user_metadata.contains_key(ENCRYPTION_MARKER_KEY) {
                return Err(seal::refusal(&seal::location(bucket, prefix, object), raw));
            }
            return Ok(without_markers(meta));
        };
        let cfg = self.config.load();
        let at = seal::location(bucket, prefix, object);
        let (old, entry) = seal::open(raw, &sealed, &at, cfg.ring())?;
        let keys = entry.site.object(sealed.salt);
        let mut stored = seal::seal(&named(meta, object), &at, &keys)?;
        let facts = seal::facts_of(&named(meta, object))?;
        if seal::body_len(&facts) != seal::body_len(&old) {
            return Err(StorageError::Encryption(format!(
                "a metadata rewrite of {bucket}/{prefix} {object:?} names another body size \
                 than the stored body"
            )));
        }
        stored.listing_token = token_maker(entry.site.clone(), bucket, &facts);
        Ok(stored)
    }

    /// Encrypt the files `parts` (in order) into the spool file `ct` on a
    /// blocking thread: header and frames, 256 KiB reads.
    async fn encrypt_files(
        parts: Vec<std::path::PathBuf>,
        ct: std::path::PathBuf,
        sealer: v2::ContentSealer,
    ) -> Result<u64, StorageError> {
        tokio::task::spawn_blocking(move || -> Result<u64, StorageError> {
            use std::io::{Read, Write};
            let mut sealer = sealer;
            let mut out = std::fs::File::create(&ct).map_err(io_to_storage_error)?;
            out.write_all(&sealer.header())
                .map_err(io_to_storage_error)?;
            let mut buf = vec![0u8; 256 * 1024];
            let mut frames = Vec::with_capacity(buf.len() + v2::FRAME_WIRE_LEN);
            for p in &parts {
                let mut input = std::fs::File::open(p).map_err(io_to_storage_error)?;
                loop {
                    let n = input.read(&mut buf).map_err(io_to_storage_error)?;
                    if n == 0 {
                        break;
                    }
                    sealer.push(&buf[..n], &mut frames)?;
                    out.write_all(&frames).map_err(io_to_storage_error)?;
                    frames.clear();
                }
            }
            let len = sealer.finish(&mut frames)?;
            out.write_all(&frames).map_err(io_to_storage_error)?;
            out.flush().map_err(io_to_storage_error)?;
            Ok(len)
        })
        .await
        .map_err(|e| StorageError::Other(format!("encrypt-to-file task join: {e}")))?
    }

    /// Encrypt the files `parts` into a spool file and store it as the
    /// passthrough object `filename`.
    #[allow(clippy::too_many_arguments)]
    async fn put_encrypted_files(
        &self,
        site: Arc<v2::SiteKey>,
        bucket: &str,
        prefix: &str,
        filename: &str,
        parts: &[std::path::PathBuf],
        metadata: &FileMetadata,
        spool: SpoolBudget<'_>,
    ) -> Result<(), StorageError> {
        let mut plain_len = 0u64;
        for p in parts {
            plain_len += tokio::fs::metadata(p)
                .await
                .map_err(io_to_storage_error)?
                .len();
        }
        let wire_len = v2::stored_len(plain_len).ok_or_else(|| {
            StorageError::Encryption("object too large for the encryption format".into())
        })?;
        let object = StoredObject::Passthrough(filename);
        let (sealer, stored) = self.seal_new(&site, bucket, prefix, object, metadata, plain_len)?;
        let ct = spool.file(wire_len).await.map_err(spool_error)?;
        let written = Self::encrypt_files(parts.to_vec(), ct.path().to_path_buf(), sealer).await?;
        if written != plain_len {
            return Err(StorageError::Other(format!(
                "a source file changed while it was encrypted ({plain_len} → {written} bytes)"
            )));
        }
        self.inner
            .put_passthrough_file(
                bucket,
                prefix,
                filename,
                ct.path(),
                &stored,
                spool.holding(&ct),
            )
            .await
    }

    /// A follow-up read of `start..=end` of the stored (raw) object whose
    /// first read gave the metadata `pin`. When the response shows another
    /// version (a write replaced the object between the two requests), the
    /// read fails with a retryable 503 SlowDown, so the bytes of one
    /// version never go out with the metadata of another. Every follow-up
    /// read of this wrapper goes through here.
    async fn pinned_range(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        start: u64,
        end: u64,
        pin: &FileMetadata,
    ) -> Result<RangeRead, StorageError> {
        let read = self
            .inner
            .open_passthrough_range(bucket, prefix, filename, start, end)
            .await?;
        if !super::traits::same_stored_version(pin, &read.metadata) {
            return Err(super::traits::object_changed_while_read(
                bucket,
                &format!("{prefix}/{filename}"),
            ));
        }
        Ok(read)
    }

    /// A range of a plaintext object that the rule lets this bucket serve,
    /// from a second read at plaintext offsets pinned to `raw`. A range that
    /// starts after byte 0 does not hold the magic, so the first bytes of
    /// the object are read too (in parallel): a body whose metadata was
    /// stripped is refused at any offset, not only at offset 0.
    async fn plaintext_range(
        &self,
        (bucket, prefix, filename): (&str, &str, &str),
        (start, end): (u64, u64),
        raw: &FileMetadata,
    ) -> Result<RangeRead, StorageError> {
        if start == 0 {
            let read = self
                .pinned_range(bucket, prefix, filename, start, end, raw)
                .await?;
            return Ok(RangeRead {
                stream: sniff_dge_magic(read.stream),
                metadata: raw.clone(),
                ..read
            });
        }
        let magic_end = (v2::MAGIC.len() - 1) as u64;
        let (head, read) = tokio::join!(
            self.pinned_range(bucket, prefix, filename, 0, magic_end, raw),
            self.pinned_range(bucket, prefix, filename, start, end, raw),
        );
        let head = collect_stream(head?.stream).await?;
        if v2::has_dge_magic(&head) {
            return Err(stripped_marker_error());
        }
        Ok(RangeRead {
            metadata: raw.clone(),
            ..read?
        })
    }

    /// A range of a passthrough object on a backend with a key: one ranged
    /// GET of exactly the frames that hold it (frame offsets do not depend
    /// on the object), whose response also carries the metadata. A
    /// plaintext object needs a second read at plaintext offsets.
    async fn keyed_range(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        start: u64,
        end: u64,
    ) -> Result<RangeRead, StorageError> {
        let (first, skip) = v2::frame_for_offset(start);
        let (last, _) = v2::frame_for_offset(end);
        let wire_start = v2::wire_offset_of_frame(first);
        let wire_end = last
            .checked_mul(v2::FRAME_WIRE_LEN as u64)
            .and_then(|o| o.checked_add((v2::HEADER_LEN + v2::FRAME_WIRE_LEN - 1) as u64))
            .unwrap_or(u64::MAX - 1)
            .min(u64::MAX - 1);
        let object = StoredObject::Passthrough(filename);
        let read = match self
            .inner
            .open_passthrough_range(bucket, prefix, filename, wire_start, wire_end)
            .await
        {
            Ok(read) => read,
            // The frame offset is past the stored end: a range past the end
            // of a sealed object, or a range near the end of a plaintext one.
            Err(StorageError::InvalidRange(_)) => {
                let raw = self
                    .inner
                    .get_passthrough_metadata(bucket, prefix, filename)
                    .await?;
                let opened = self.present_raw(bucket, prefix, object, &raw)?;
                if let Some(body) = opened.sealed {
                    return Err(StorageError::InvalidRange(format!(
                        "bytes {start}-{end} of an object of {} bytes",
                        body.len
                    )));
                }
                return self
                    .plaintext_range((bucket, prefix, filename), (start, end), &raw)
                    .await;
            }
            Err(e) => return Err(e),
        };
        let opened = self.present_raw(bucket, prefix, object, &read.metadata)?;
        let Some(body) = opened.sealed else {
            drop(read.stream);
            return self
                .plaintext_range((bucket, prefix, filename), (start, end), &read.metadata)
                .await;
        };
        v2::check_stored_len(read.stored_total, body.len)?;
        let (start, end) = super::clamp_range(start, end, body.len)?;
        let take = end - start + 1;
        Ok(RangeRead {
            stream: v2::decrypt_frames(
                read.stream,
                body.opener(),
                first,
                body.len,
                skip,
                Some(take),
            ),
            len: take,
            metadata: opened.meta,
            stored_total: read.stored_total,
        })
    }
}

impl<B: StorageBackend + Send + Sync> StorageBackend for EncryptingBackend<B> {
    // ── Writes: seal ──

    async fn put_reference(
        &self,
        bucket: &str,
        prefix: &str,
        data: bytes::Bytes,
        metadata: &FileMetadata,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.check_write(bucket).await?;
        let (body, stored) =
            self.seal_bytes(bucket, prefix, StoredObject::Reference, &data, metadata)?;
        self.inner
            .put_reference(bucket, prefix, body, &stored, proof)
            .await
    }

    /// A storage-level copy moves the stored bytes as they are, so it is
    /// valid only when the destination would store them the same way:
    /// plaintext to plaintext, or sealed under the destination's write key.
    /// A sealed copy is re-sealed for its destination (the seal binds the
    /// location) under the source's own object keys, after the source is
    /// checked against the caller's pin. Otherwise `Unsupported`: the
    /// caller copies through the proxy.
    async fn copy_within(
        &self,
        variant: super::traits::ObjectVariant,
        src: super::traits::ObjectAt<'_>,
        dst: super::traits::ObjectAt<'_>,
        source: &FileMetadata,
        metadata: &FileMetadata,
    ) -> Result<super::traits::CopyOutcome, StorageError> {
        self.check_write(dst.bucket).await?;
        use super::traits::{CopyOutcome, ObjectVariant};
        let dst_site = self.write_key(dst.bucket)?;
        let raw = match variant {
            ObjectVariant::Delta => {
                self.inner
                    .get_delta_metadata(src.bucket, src.prefix, src.filename)
                    .await?
            }
            ObjectVariant::Passthrough => {
                self.inner
                    .get_passthrough_metadata(src.bucket, src.prefix, src.filename)
                    .await?
            }
        };
        let src_object = object_of(variant, src.filename);
        let dst_object = object_of(variant, dst.filename);
        let stored = match (seal::sealed_of(&raw)?, dst_site) {
            (None, None) => {
                if raw.user_metadata.contains_key(ENCRYPTION_MARKER_KEY) {
                    return Ok(CopyOutcome::Unsupported);
                }
                // The read rule and the caller's pin hold here too: a copy
                // must not move an object the source bucket refuses, nor a
                // version the caller never read.
                let opened = self.present_raw(src.bucket, src.prefix, src_object, &raw)?;
                check_copy_pin(source, &opened.meta, &src)?;
                without_markers(metadata)
            }
            (Some(sealed), Some(site)) if sealed.fp == site.fingerprint() => {
                let opened = self.present_raw(src.bucket, src.prefix, src_object, &raw)?;
                check_copy_pin(source, &opened.meta, &src)?;
                let at = seal::location(dst.bucket, dst.prefix, dst_object);
                let keys = site.object(sealed.salt);
                let mut stored = seal::seal(&named(metadata, dst_object), &at, &keys)?;
                let facts = seal::facts_of(&named(metadata, dst_object))?;
                if seal::body_len(&facts) != opened.sealed.as_ref().map_or(0, |b| b.len) {
                    return Ok(CopyOutcome::Unsupported);
                }
                stored.listing_token = token_maker(site, dst.bucket, &facts);
                stored
            }
            _ => return Ok(CopyOutcome::Unsupported),
        };
        self.inner
            .copy_within(variant, src, dst, &raw, &stored)
            .await
    }

    async fn put_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: bytes::Bytes,
        metadata: &FileMetadata,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.check_write(bucket).await?;
        let (body, stored) = self.seal_bytes(
            bucket,
            prefix,
            StoredObject::Delta(filename),
            &data,
            metadata,
        )?;
        self.inner
            .put_delta(bucket, prefix, filename, body, &stored, proof)
            .await
    }

    async fn put_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: bytes::Bytes,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.check_write(bucket).await?;
        let (body, stored) = self.seal_bytes(
            bucket,
            prefix,
            StoredObject::Passthrough(filename),
            &data,
            metadata,
        )?;
        self.inner
            .put_passthrough(bucket, prefix, filename, body, &stored)
            .await
    }

    /// BOUNDED-MEMORY file store: encrypt the source file into a spool file
    /// (256 KiB reads, never the whole object), then hand that file to the
    /// inner backend. The ciphertext file holds spool budget.
    async fn put_passthrough_file(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        source_path: &std::path::Path,
        metadata: &FileMetadata,
        spool: SpoolBudget<'_>,
    ) -> Result<(), StorageError> {
        self.check_write(bucket).await?;
        let Some(site) = self.write_key(bucket)? else {
            return self
                .inner
                .put_passthrough_file(
                    bucket,
                    prefix,
                    filename,
                    source_path,
                    &without_markers(metadata),
                    spool,
                )
                .await;
        };
        self.put_encrypted_files(
            site,
            bucket,
            prefix,
            filename,
            &[source_path.to_path_buf()],
            metadata,
            spool,
        )
        .await
    }

    /// BOUNDED-MEMORY relayed-parts store: the ordered part files are
    /// encrypted straight into one spool file (no joined plaintext copy).
    async fn put_passthrough_parts(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        part_paths: &[std::path::PathBuf],
        metadata: &FileMetadata,
        spool: SpoolBudget<'_>,
    ) -> Result<(), StorageError> {
        self.check_write(bucket).await?;
        let Some(site) = self.write_key(bucket)? else {
            return self
                .inner
                .put_passthrough_parts(
                    bucket,
                    prefix,
                    filename,
                    part_paths,
                    &without_markers(metadata),
                    spool,
                )
                .await;
        };
        self.put_encrypted_files(site, bucket, prefix, filename, part_paths, metadata, spool)
            .await
    }

    async fn file_put_spool_bytes(&self, bucket: &str, bytes: u64, parts: bool) -> u64 {
        if self.writes_plaintext(bucket) {
            return self.inner.file_put_spool_bytes(bucket, bytes, parts).await;
        }
        // The ciphertext file.
        let wire = v2::stored_len(bytes).unwrap_or(u64::MAX);
        wire.saturating_add(self.inner.file_put_spool_bytes(bucket, wire, false).await)
    }

    /// The chunks are encrypted as they come (one 64 KiB window and one
    /// frame of working memory) and handed to the inner chunked PUT.
    async fn put_passthrough_chunked(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        chunks: &[Bytes],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.check_write(bucket).await?;
        let Some(site) = self.write_key(bucket)? else {
            return self
                .inner
                .put_passthrough_chunked(
                    bucket,
                    prefix,
                    filename,
                    chunks,
                    &without_markers(metadata),
                )
                .await;
        };
        let plain_len: u64 = chunks.iter().map(|c| c.len() as u64).sum();
        let object = StoredObject::Passthrough(filename);
        let (mut sealer, stored) =
            self.seal_new(&site, bucket, prefix, object, metadata, plain_len)?;
        let mut out: Vec<Bytes> = Vec::with_capacity(chunks.len() + 2);
        out.push(Bytes::copy_from_slice(&sealer.header()));
        for chunk in chunks {
            let mut frames = Vec::with_capacity(chunk.len() + v2::FRAME_WIRE_LEN);
            sealer.push(chunk, &mut frames)?;
            if !frames.is_empty() {
                out.push(Bytes::from(frames));
            }
        }
        let mut last = Vec::with_capacity(v2::FRAME_WIRE_LEN);
        sealer.finish(&mut last)?;
        out.push(Bytes::from(last));
        self.inner
            .put_passthrough_chunked(bucket, prefix, filename, &out, &stored)
            .await
    }

    // ── Reads: open ──

    async fn get_reference(&self, bucket: &str, prefix: &str) -> Result<Vec<u8>, StorageError> {
        let read = self
            .open_object(bucket, prefix, StoredObject::Reference)
            .await?;
        collect_stream(read.stream).await
    }

    /// A plaintext reference passes through (the filesystem keeps its
    /// hardlink). A sealed one is checked against its stored size, then
    /// decrypted frame by frame as the caller writes it; its size is the
    /// sealed plaintext size.
    async fn open_reference_source(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<ReferenceSource, StorageError> {
        let mut source = self.inner.open_reference_source(bucket, prefix).await?;
        let opened = self.present_raw(bucket, prefix, StoredObject::Reference, &source.metadata)?;
        let Some(body) = opened.sealed else {
            source.metadata = opened.meta;
            return Ok(source);
        };
        // The inner source's size is the stored size: checked before the
        // body is read, so a reference with bytes appended on the backend
        // is refused without reading it into memory.
        v2::check_stored_len(source.size, body.len)?;
        // Decrypted frame by frame into the caller's file: a reference of
        // any size costs one frame of memory. Each frame is authenticated
        // before it is written, and a cut or extended body fails at its
        // end (final-frame flag), so the caller's write fails.
        let stream = open_body_stream(source.into_stream(), &body).await?;
        Ok(ReferenceSource::stream(stream, body.len, opened.meta))
    }

    async fn get_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
        let ObjectRead { stream, .. } = self
            .open_object(bucket, prefix, StoredObject::Delta(filename))
            .await?;
        collect_stream(stream).await
    }

    async fn get_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
        let ObjectRead { stream, .. } = self
            .open_object(bucket, prefix, StoredObject::Passthrough(filename))
            .await?;
        collect_stream(stream).await
    }

    /// The inner read carries the stored metadata and the stored size, so
    /// the read rule needs no HEAD of its own, and a sealed object of the
    /// wrong stored size fails before the first byte. References and deltas
    /// (bounded by the object size limit) are decrypted whole; a passthrough
    /// streams frame by frame. Returns the PRESENTED metadata.
    async fn open_object(
        &self,
        bucket: &str,
        prefix: &str,
        object: StoredObject<'_>,
    ) -> Result<ObjectRead, StorageError> {
        let read = self.inner.open_object(bucket, prefix, object).await?;
        let opened = self.present_raw(bucket, prefix, object, &read.metadata)?;
        if let Some(body) = &opened.sealed {
            v2::check_stored_len(read.stored_total, body.len)?;
        }
        let stream: ByteStream = match (&opened.sealed, object) {
            (Some(body), StoredObject::Reference | StoredObject::Delta(_)) => {
                let wire = collect_stream(read.stream).await?;
                let plain = body.opener().open_all(&wire, body.len)?;
                Box::pin(futures::stream::once(async { Ok(Bytes::from(plain)) }))
            }
            (Some(body), StoredObject::Passthrough(_)) => {
                open_body_stream(read.stream, body).await?
            }
            (None, _) => sniff_dge_magic(read.stream),
        };
        Ok(ObjectRead {
            stream,
            metadata: opened.meta,
            stored_total: read.stored_total,
        })
    }

    // === Multipart upload ===
    //
    // Forwarded to the inner backend ONLY when this wrapper does not
    // encrypt the bucket. When it does, the transfer layer never calls
    // these (gated off by `multipart_storage_label`); the explicit error is
    // defence-in-depth.

    async fn create_multipart_upload(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        metadata: &FileMetadata,
    ) -> Result<MultipartUpload, StorageError> {
        self.check_write(bucket).await?;
        // A refused write names its cause, not "multipart unsupported".
        self.write_key(bucket)?;
        if !self.writes_plaintext(bucket) {
            return Err(StorageError::Other(
                "multipart upload unsupported on a proxy-AES-encrypting bucket".to_string(),
            ));
        }
        self.inner
            .create_multipart_upload(bucket, prefix, filename, &without_markers(metadata))
            .await
    }

    async fn upload_part(
        &self,
        upload: &MultipartUpload,
        prefix: &str,
        filename: &str,
        part_number: i32,
        data: Bytes,
    ) -> Result<UploadedPart, StorageError> {
        self.check_write(&upload.bucket).await?;
        self.inner
            .upload_part(upload, prefix, filename, part_number, data)
            .await
    }

    async fn complete_multipart_upload(
        &self,
        upload: &MultipartUpload,
        prefix: &str,
        filename: &str,
        parts: &[UploadedPart],
        assembled: &[Bytes],
        metadata: &FileMetadata,
    ) -> Result<String, StorageError> {
        self.check_write(&upload.bucket).await?;
        // The bucket's write policy NOW (a config apply can change it
        // between create and complete). A bucket that encrypts never stores
        // the parts as plaintext: buffered parts are encrypted like any
        // chunked write; native parts already sit on the backend in
        // plaintext, so the upload is refused.
        if self.write_key(&upload.bucket)?.is_some() {
            if upload.native {
                return Err(StorageError::Encryption(format!(
                    "bucket '{}' encrypts at rest, but this multipart upload stored its \
                     parts natively (unencrypted): the encryption of the bucket changed \
                     during the upload. Start the upload again",
                    upload.bucket
                )));
            }
            self.put_passthrough_chunked(&upload.bucket, prefix, filename, assembled, metadata)
                .await?;
            return Ok(metadata.etag());
        }
        self.inner
            .complete_multipart_upload(
                upload,
                prefix,
                filename,
                parts,
                assembled,
                &without_markers(metadata),
            )
            .await
    }

    async fn abort_multipart_upload(
        &self,
        upload: &MultipartUpload,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        self.inner
            .abort_multipart_upload(upload, prefix, filename)
            .await
    }

    fn multipart_storage_label(&self, bucket: &str) -> &'static str {
        if !self.writes_plaintext(bucket) {
            return "aes256-gcm-proxy";
        }
        self.inner.multipart_storage_label(bucket)
    }

    fn supports_native_multipart(&self, bucket: &str) -> bool {
        // Encryption rewrites the bytes, so the streaming multipart path
        // cannot be used, whatever the inner backend.
        if !self.writes_plaintext(bucket) {
            return false;
        }
        self.inner.supports_native_multipart(bucket)
    }

    /// The wrapper presents listed entries already (sealed ones opened), so
    /// the inner backend's answer holds: the filesystem lists the logical
    /// facts, an S3 lite list does not.
    fn lite_list_carries_logical_facts(&self, bucket: &str) -> bool {
        self.inner.lite_list_carries_logical_facts(bucket)
    }

    /// Without any key, objects here are plaintext: the inner backend's one
    /// read gives the range and the metadata, and the read rule applies.
    /// With a key, `keyed_range` reads the covering frames.
    async fn open_passthrough_range(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        start: u64,
        end: u64,
    ) -> Result<RangeRead, StorageError> {
        if self.has_any_key() {
            return self.keyed_range(bucket, prefix, filename, start, end).await;
        }
        let read = self
            .inner
            .open_passthrough_range(bucket, prefix, filename, start, end)
            .await?;
        let opened = self.present_raw(
            bucket,
            prefix,
            StoredObject::Passthrough(filename),
            &read.metadata,
        )?;
        Ok(RangeRead {
            stream: if start == 0 {
                sniff_dge_magic(read.stream)
            } else {
                read.stream
            },
            metadata: opened.meta,
            ..read
        })
    }

    async fn listing_tokens(
        &self,
        b: &str,
        objects: &[(String, FileMetadata)],
    ) -> Vec<Option<crate::storage::ListedToken>> {
        self.inner.listing_tokens(b, objects).await
    }

    // ── Metadata: present / re-seal ──

    async fn get_reference_metadata(&self, b: &str, p: &str) -> Result<FileMetadata, StorageError> {
        let raw = self.inner.get_reference_metadata(b, p).await?;
        Ok(self.present_raw(b, p, StoredObject::Reference, &raw)?.meta)
    }
    async fn read_key_claim(&self, b: &str) -> Result<Option<super::KeyClaim>, StorageError> {
        self.inner.read_key_claim(b).await
    }
    async fn write_key_claim(
        &self,
        b: &str,
        op: super::ClaimWrite<'_>,
    ) -> Result<super::ClaimOutcome, StorageError> {
        self.inner.write_key_claim(b, op).await
    }
    async fn get_delta_metadata(
        &self,
        b: &str,
        p: &str,
        f: &str,
    ) -> Result<FileMetadata, StorageError> {
        let raw = self.inner.get_delta_metadata(b, p, f).await?;
        Ok(self.present_raw(b, p, StoredObject::Delta(f), &raw)?.meta)
    }
    async fn get_passthrough_metadata(
        &self,
        b: &str,
        p: &str,
        f: &str,
    ) -> Result<FileMetadata, StorageError> {
        let raw = self.inner.get_passthrough_metadata(b, p, f).await?;
        Ok(self
            .present_raw(b, p, StoredObject::Passthrough(f), &raw)?
            .meta)
    }
    /// The presented metadata; the basis keeps the stored (raw) metadata,
    /// which the re-seal of the rewrite needs (its salt).
    async fn get_passthrough_metadata_for_rewrite(
        &self,
        b: &str,
        p: &str,
        f: &str,
    ) -> Result<(FileMetadata, super::RewriteBasis), StorageError> {
        let (raw, mut basis) = self
            .inner
            .get_passthrough_metadata_for_rewrite(b, p, f)
            .await?;
        let meta = self
            .present_raw(b, p, StoredObject::Passthrough(f), &raw)?
            .meta;
        if basis.stored.is_none() {
            basis.stored = Some(raw);
        }
        Ok((meta, basis))
    }
    async fn put_passthrough_metadata(
        &self,
        b: &str,
        p: &str,
        f: &str,
        m: &FileMetadata,
        basis: &super::RewriteBasis,
    ) -> Result<(), StorageError> {
        self.check_write(b).await?;
        // The body is not rewritten: the stored object's seal decides. A
        // caller's earlier read gives the stored metadata without a HEAD.
        let raw = match &basis.stored {
            Some(raw) => raw.clone(),
            None => self.inner.get_passthrough_metadata(b, p, f).await?,
        };
        let stored = self.reseal(b, p, StoredObject::Passthrough(f), m, &raw)?;
        self.inner
            .put_passthrough_metadata(b, p, f, &stored, basis)
            .await
    }
    async fn put_reference_metadata(
        &self,
        b: &str,
        p: &str,
        m: &FileMetadata,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.check_write(b).await?;
        let raw = self.inner.get_reference_metadata(b, p).await?;
        let stored = self.reseal(b, p, StoredObject::Reference, m, &raw)?;
        self.inner
            .put_reference_metadata(b, p, &stored, proof)
            .await
    }
    async fn reference_fence_with_metadata(
        &self,
        b: &str,
        p: &str,
    ) -> Result<(super::traits::RefFence, Option<FileMetadata>), StorageError> {
        let (fence, raw) = self.inner.reference_fence_with_metadata(b, p).await?;
        let meta = match raw {
            Some(raw) => Some(self.present_raw(b, p, StoredObject::Reference, &raw)?.meta),
            None => None,
        };
        Ok((fence, meta))
    }
    /// The same transformation as the unfenced writes (seal a body, re-seal
    /// a metadata-only write), then the inner fenced write: the fence is on
    /// the stored object, which is what the inner backend observes.
    async fn write_reference_fenced(
        &self,
        b: &str,
        p: &str,
        op: super::traits::RefWrite<'_>,
        fence: &super::traits::RefFence,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<super::traits::RefFence, StorageError> {
        self.check_write_unless(b, matches!(op, super::traits::RefWrite::Delete))
            .await?;
        use super::traits::RefWrite;
        match op {
            RefWrite::Put { data, metadata } => {
                let (body, stored) =
                    self.seal_bytes(b, p, StoredObject::Reference, data, metadata)?;
                self.inner
                    .write_reference_fenced(
                        b,
                        p,
                        RefWrite::Put {
                            data: &body,
                            metadata: &stored,
                        },
                        fence,
                        proof,
                    )
                    .await
            }
            RefWrite::PutFile { path, metadata } => {
                let data = bytes::Bytes::from(tokio::fs::read(path).await?);
                let (body, stored) =
                    self.seal_bytes(b, p, StoredObject::Reference, &data, metadata)?;
                self.inner
                    .write_reference_fenced(
                        b,
                        p,
                        RefWrite::Put {
                            data: &body,
                            metadata: &stored,
                        },
                        fence,
                        proof,
                    )
                    .await
            }
            RefWrite::Metadata { metadata } => {
                let raw = self.inner.get_reference_metadata(b, p).await?;
                let stored = self.reseal(b, p, StoredObject::Reference, metadata, &raw)?;
                self.inner
                    .write_reference_fenced(
                        b,
                        p,
                        RefWrite::Metadata { metadata: &stored },
                        fence,
                        proof,
                    )
                    .await
            }
            RefWrite::Delete => {
                self.inner
                    .write_reference_fenced(b, p, RefWrite::Delete, fence, proof)
                    .await
            }
        }
    }

    // ── Listings: present ──

    async fn scan_deltaspace(&self, b: &str, p: &str) -> Result<Vec<FileMetadata>, StorageError> {
        let raw = self.inner.scan_deltaspace(b, p).await?;
        Ok(self.present_scanned(b, p, raw))
    }
    async fn scan_deltaspace_lite(&self, b: &str, p: &str) -> Result<LiteScanResult, StorageError> {
        let mut lite = self.inner.scan_deltaspace_lite(b, p).await?;
        lite.metadata = self.present_scanned(b, p, std::mem::take(&mut lite.metadata));
        Ok(lite)
    }
    async fn holds_only_reference(&self, b: &str, p: &str) -> Result<bool, StorageError> {
        self.inner.holds_only_reference(b, p).await
    }
    async fn bulk_list_objects(
        &self,
        b: &str,
        p: &str,
    ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
        let raw = self.inner.bulk_list_objects(b, p).await?;
        Ok(self.present_listed(b, raw))
    }
    async fn enrich_list_metadata(
        &self,
        b: &str,
        o: Vec<(String, FileMetadata)>,
    ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
        let raw = self.inner.enrich_list_metadata(b, o).await?;
        Ok(self.present_listed(b, raw))
    }
    async fn bulk_list_objects_with_baselines(
        &self,
        b: &str,
        p: &str,
        start_after: Option<&str>,
        max_listed: Option<usize>,
    ) -> Result<BulkListing, StorageError> {
        let mut listing = self
            .inner
            .bulk_list_objects_with_baselines(b, p, start_after, max_listed)
            .await?;
        listing.objects = self.present_listed(b, std::mem::take(&mut listing.objects));
        Ok(listing)
    }
    async fn list_objects_delegated(
        &self,
        b: &str,
        p: &str,
        d: Option<&str>,
        m: u32,
        t: Option<&str>,
    ) -> Result<Option<DelegatedListResult>, StorageError> {
        let Some(mut page) = self.inner.list_objects_delegated(b, p, d, m, t).await? else {
            return Ok(None);
        };
        page.objects = self.present_listed(b, std::mem::take(&mut page.objects));
        Ok(Some(page))
    }
    /// With a key, the inner listing may hold ciphertext: its own facts are
    /// not trusted (a backend writer can forge `!!1` markers), a listed
    /// size is the stored size, and the logical facts come from listing
    /// tokens that open under a key of this backend.
    async fn resolve_listed_sizes(
        &self,
        b: &str,
        objects: &mut [(String, FileMetadata)],
        how: ResolveListed,
    ) -> Vec<ListedSize> {
        if !self.has_any_key() {
            return self.inner.resolve_listed_sizes(b, objects, how).await;
        }
        let mut sizes = self
            .inner
            .resolve_listed_sizes(
                b,
                objects,
                ResolveListed {
                    trust_v1_facts: false,
                    ..how
                },
            )
            .await;
        if self.inner.lite_list_carries_logical_facts(b) {
            return sizes;
        }
        downgrade_ciphertext_listings(objects, &mut sizes);
        let tokens = self.inner.listing_tokens(b, objects).await;
        let cfg = self.config.load();
        for (((_, meta), size), token) in objects.iter_mut().zip(sizes.iter_mut()).zip(tokens) {
            let Some(t) = token else { continue };
            if *size != ListedSize::StoredOnly {
                continue;
            }
            if let Some(lf) = open_token(cfg.ring(), b, &t) {
                if apply_listing_facts(meta, &t, &lf) {
                    *size = ListedSize::Cached;
                }
            } else if let (Some(sentinel), Some(fp)) =
                (&self.sentinel, v2::token_fingerprint(&t.token))
            {
                if cfg.ring().get(&fp).is_none() {
                    sentinel.sighted(b, fp, &t.stored_key, std::time::Instant::now());
                }
            }
        }
        sizes
    }

    // ── Pass-through ──

    async fn create_bucket(&self, b: &str) -> Result<(), StorageError> {
        self.inner.create_bucket(b).await
    }
    async fn ensure_declared_bucket(&self, b: &str) -> Result<(), StorageError> {
        self.inner.ensure_declared_bucket(b).await
    }
    async fn delete_bucket(&self, b: &str) -> Result<(), StorageError> {
        self.inner.delete_bucket(b).await?;
        // A new bucket of that name starts without the old one's evidence.
        if let Some(sentinel) = &self.sentinel {
            sentinel.forget(b);
        }
        Ok(())
    }
    async fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
        self.inner.list_buckets().await
    }
    async fn list_buckets_with_dates(
        &self,
    ) -> Result<Vec<(String, chrono::DateTime<chrono::Utc>)>, StorageError> {
        self.inner.list_buckets_with_dates().await
    }
    async fn head_bucket(&self, b: &str) -> Result<bool, StorageError> {
        self.inner.head_bucket(b).await
    }
    async fn has_reference(&self, b: &str, p: &str) -> Result<bool, StorageError> {
        self.inner.has_reference(b, p).await
    }
    async fn delete_reference(
        &self,
        b: &str,
        p: &str,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.inner.delete_reference(b, p, proof).await
    }
    async fn flush_pending(&self) -> Result<(), StorageError> {
        self.inner.flush_pending().await
    }
    async fn reference_fence(
        &self,
        b: &str,
        p: &str,
    ) -> Result<super::traits::RefFence, StorageError> {
        self.inner.reference_fence(b, p).await
    }
    async fn delete_delta(&self, b: &str, p: &str, f: &str) -> Result<(), StorageError> {
        self.inner.delete_delta(b, p, f).await
    }
    async fn delete_passthrough(&self, b: &str, p: &str, f: &str) -> Result<(), StorageError> {
        self.inner.delete_passthrough(b, p, f).await
    }
    // The version is the stored object's: an overwrite changes it too.
    async fn variant_version(
        &self,
        b: &str,
        p: &str,
        f: &str,
        v: crate::storage::ObjectVariant,
    ) -> Result<Option<String>, StorageError> {
        self.inner.variant_version(b, p, f, v).await
    }
    async fn delete_variant_if(
        &self,
        b: &str,
        p: &str,
        f: &str,
        v: crate::storage::ObjectVariant,
        version: &str,
    ) -> Result<bool, StorageError> {
        self.inner.delete_variant_if(b, p, f, v, version).await
    }
    async fn list_deltaspaces(&self, b: &str) -> Result<Vec<String>, StorageError> {
        self.inner.list_deltaspaces(b).await
    }
    async fn list_reference_prefixes(&self, b: &str, s: &str) -> Result<Vec<String>, StorageError> {
        self.inner.list_reference_prefixes(b, s).await
    }
    async fn total_size(&self, b: Option<&str>) -> Result<u64, StorageError> {
        self.inner.total_size(b).await
    }
    /// The witness, then the sentinel's cached verdict (no I/O): the
    /// background writers defer instead of failing object after object.
    fn write_refusal(&self, bucket: &str) -> Option<String> {
        if let Err(StorageError::WritesRefused(m)) = self.check_witness(bucket) {
            return Some(m);
        }
        let sentinel = self.sentinel.as_ref()?;
        let cfg = self.config.load();
        let r = sentinel.refusal(bucket, std::time::Instant::now(), |fp| {
            cfg.ring().get(fp).is_some()
        })?;
        Some(sentinel::refusal_detail(bucket, &r, None))
    }
    async fn put_directory_marker(&self, b: &str, k: &str) -> Result<(), StorageError> {
        self.check_write(b).await?;
        self.inner.put_directory_marker(b, k).await
    }
}

/// The logical facts of a listing token, when it opens under a key of
/// `ring` for exactly the stored object it was kept for. Pure.
fn open_token(
    ring: &seal::Keyring,
    bucket: &str,
    t: &crate::storage::ListedToken,
) -> Option<v2::ListingFacts> {
    let entry = ring.get(&v2::token_fingerprint(&t.token)?)?;
    entry.site.open_listing_token(
        &v2::ListedAt {
            bucket,
            stored_key: &t.stored_key,
            stored_etag: super::list_size_cache::bare_etag(&t.stored_etag),
            stored_size: t.stored_size,
        },
        &t.token,
    )
}

/// Apply the facts of an opened listing token to a listed entry. A
/// passthrough's stored size must be exactly the stored size of its
/// logical size; a delta's plaintext size follows from its stored size.
fn apply_listing_facts(
    meta: &mut FileMetadata,
    t: &crate::storage::ListedToken,
    lf: &v2::ListingFacts,
) -> bool {
    let body = v2::plaintext_len_of(t.stored_size);
    if let crate::types::StorageInfo::Delta { delta_size, .. } = &mut meta.storage_info {
        if let Some(b) = body {
            *delta_size = b;
        }
    } else if body != Some(lf.size) {
        return false;
    }
    let digest = hex::encode(lf.etag_digest);
    let etag = match lf.parts {
        0 => digest,
        parts => format!("{digest}-{parts}"),
    };
    super::list_size_cache::apply(
        meta,
        &super::list_size_cache::LogicalFacts::of_listing_token(lf.size, etag),
    );
    true
}

/// On a backend whose listing reports stored bytes, a passthrough entry
/// not resolved yet carries its stored (ciphertext) size: mark it
/// `StoredOnly`. Directory markers stay `Listed`. Pure; unit-tested.
fn downgrade_ciphertext_listings(objects: &[(String, FileMetadata)], sizes: &mut [ListedSize]) {
    for ((key, _), size) in objects.iter().zip(sizes.iter_mut()) {
        if *size == ListedSize::Listed && !key.ends_with('/') {
            *size = ListedSize::StoredOnly;
        }
    }
}
