// SPDX-License-Identifier: BUSL-1.1

//! Sealed metadata of format v2 (issue #120): what the wrapper stores for
//! an encrypted object, and what it presents to its callers. Pure.
//!
//! The stored (backend-facing) metadata of an encrypted object:
//!
//! - keeps in clear: the tool, the original name, the creation time, the
//!   note, the reference path, the content type, the client's user
//!   metadata, the delta size, and the size of a passthrough or reference
//!   (the stored size gives these away anyway);
//! - seals: the logical size, the MD5 and multipart ETag, the SHA-256 of
//!   the content and of a delta's reference, the creation time, and a
//!   digest of the clear headers above ([`client_headers`]);
//! - holds an identity token in `dg-file-sha256` (and `dg-ref-sha256` of a
//!   delta): unique per seal, never a hash of the content. Older releases
//!   require a non-empty value there; without one they would treat the
//!   object as unmanaged and serve its ciphertext;
//! - carries `dg-encrypted: aes-256-gcm-chunked-v1` ([`DISK_MARKER`]), the
//!   value of the previous format: a binary of an older release then fails
//!   on the `DGE2` body instead of serving it as plaintext.
//!
//! The presented metadata is the logical object again, with the markers
//! `dg-encrypted: v2` and `dg-encryption-key-id: <id of the key>`.

use super::v2::{self, Location, ObjectMeta, Sealed, SealedFacts, SiteKey};
use super::{ENCRYPTION_KEY_ID_KEY, ENCRYPTION_MARKER_KEY};
use crate::storage::traits::{StorageError, StoredObject};
use crate::types::{FileMetadata, StorageInfo};
use std::collections::HashMap;
use std::sync::Arc;

/// The user-metadata key of the sealed facts.
pub const SEALED_KEY: &str = crate::types::meta_keys::SEALED;

/// The `dg-encrypted` value that callers above the wrapper see on an
/// encrypted object.
pub const ENCRYPTION_MARKER_VALUE: &str = "v2";

/// The `dg-encrypted` value stored with a v2 object: the previous
/// format's value, so that an older release refuses the object (see the
/// module doc).
pub const DISK_MARKER: &str = "aes-256-gcm-chunked-v1";

/// Every user-metadata key that the wrapper owns. A write stores only its
/// own decision; a caller's copy of any of them is dropped.
pub const ENCRYPTION_MARKER_KEYS: [&str; 3] =
    [ENCRYPTION_MARKER_KEY, ENCRYPTION_KEY_ID_KEY, SEALED_KEY];

fn enc_err(msg: impl Into<String>) -> StorageError {
    StorageError::Encryption(msg.into())
}

/// The keys of one raw backend, by fingerprint, with their ids.
#[derive(Clone, Default)]
pub struct Keyring {
    by_fp: HashMap<[u8; v2::FP_LEN], RingEntry>,
}

#[derive(Clone)]
pub struct RingEntry {
    pub id: String,
    pub site: Arc<SiteKey>,
}

impl std::fmt::Debug for RingEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RingEntry")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Keyring {
    /// Add a key under its id. One key has one id (the engine refuses a
    /// config that gives it two), so a repeat is ignored.
    pub fn insert(&mut self, id: &str, key: &super::EncryptionKey) -> Arc<SiteKey> {
        let site = SiteKey::new(key);
        self.by_fp
            .entry(site.fingerprint())
            .or_insert_with(|| RingEntry {
                id: id.to_string(),
                site: Arc::new(site),
            })
            .site
            .clone()
    }

    pub fn get(&self, fp: &[u8; v2::FP_LEN]) -> Option<&RingEntry> {
        self.by_fp.get(fp)
    }

    pub fn is_empty(&self) -> bool {
        self.by_fp.is_empty()
    }
}

/// The location of an object for its seal: the prefix without a trailing
/// `/`, so one stored key has one encoding.
pub fn location<'a>(bucket: &'a str, prefix: &'a str, object: StoredObject<'a>) -> Location<'a> {
    Location {
        bucket,
        prefix: prefix.trim_end_matches('/'),
        object,
    }
}

/// The clear headers that a seal covers: the content type and every
/// user-metadata pair except the wrapper's own markers. A backend
/// writer that changes one of them makes the object fail closed.
pub fn client_headers(meta: &FileMetadata) -> [u8; 16] {
    // Not the original name: a listing names an entry by its key.
    let ct = meta.content_type.as_deref().unwrap_or("");
    v2::client_headers_digest(
        [(":content-type", ct)].into_iter().chain(
            meta.user_metadata
                .iter()
                .filter(|(k, _)| {
                    !ENCRYPTION_MARKER_KEYS
                        .iter()
                        .any(|m| k.eq_ignore_ascii_case(m))
                })
                .map(|(k, v)| (k.as_str(), v.as_str())),
        ),
    )
}

fn hex_array<const N: usize>(field: &str, value: &str) -> Result<[u8; N], StorageError> {
    let mut out = [0u8; N];
    hex::decode_to_slice(value, &mut out).map_err(|_| {
        enc_err(format!(
            "cannot seal metadata: {field} is not {N} bytes of hex"
        ))
    })?;
    Ok(out)
}

/// `"<md5 hex>-<parts>"` (quoted or not) → (digest, parts).
fn parse_multipart_etag(etag: &str) -> Result<([u8; 16], u16), StorageError> {
    let bare = etag.trim_matches('"');
    let (digest, parts) = bare
        .rsplit_once('-')
        .ok_or_else(|| enc_err("cannot seal metadata: malformed multipart ETag"))?;
    let parts: u16 = parts
        .parse()
        .map_err(|_| enc_err("cannot seal metadata: malformed multipart ETag part count"))?;
    Ok((hex_array("the multipart ETag", digest)?, parts))
}

/// The facts of a logical object, to seal.
pub fn facts_of(meta: &FileMetadata) -> Result<SealedFacts, StorageError> {
    let md5 = (!meta.md5.is_empty())
        .then(|| hex_array("dg-md5", &meta.md5))
        .transpose()?;
    let multipart = meta
        .multipart_etag
        .as_deref()
        .map(parse_multipart_etag)
        .transpose()?;
    let (ref_sha256, delta_size) = match &meta.storage_info {
        StorageInfo::Delta {
            ref_sha256,
            delta_size,
            ..
        } => (
            Some(hex_array("the reference SHA-256", ref_sha256)?),
            Some(*delta_size),
        ),
        _ => (None, None),
    };
    Ok(SealedFacts {
        file_size: meta.file_size,
        md5,
        multipart,
        file_sha256: hex_array("the content SHA-256", &meta.file_sha256)?,
        ref_sha256,
        delta_size,
        created_at_nanos: meta.created_at.timestamp_nanos_opt().unwrap_or(0),
        headers: client_headers(meta),
    })
}

/// The plaintext length of the stored body: a delta's size, else the
/// object's size.
pub fn body_len(facts: &SealedFacts) -> u64 {
    facts.delta_size.unwrap_or(facts.file_size)
}

/// The identity token stored in place of a content hash.
pub fn identity_token(sealed: &str) -> String {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    h.update(b"DGE2 id");
    h.update(sealed.as_bytes());
    format!("sealed:{}", &hex::encode(h.finalize())[..32])
}

/// The stored metadata of `meta` sealed under `keys` at `at`.
pub fn seal(
    meta: &FileMetadata,
    at: &Location<'_>,
    keys: &ObjectMeta,
) -> Result<FileMetadata, StorageError> {
    let mut stored = meta.clone();
    for k in ENCRYPTION_MARKER_KEYS {
        stored.user_metadata.remove(k);
    }
    let facts = facts_of(&stored)?;
    let sealed = keys.seal(at, &facts)?;
    let token = identity_token(&sealed);
    stored.md5 = String::new();
    stored.multipart_etag = None;
    stored.file_sha256 = token.clone();
    if let StorageInfo::Delta { ref_sha256, .. } = &mut stored.storage_info {
        *ref_sha256 = token;
        // A delta's logical size is not derivable from its stored size.
        stored.file_size = 0;
    }
    stored
        .user_metadata
        .insert(ENCRYPTION_MARKER_KEY.to_string(), DISK_MARKER.to_string());
    stored.user_metadata.insert(SEALED_KEY.to_string(), sealed);
    Ok(stored)
}

/// The parsed seal of a stored object, if it has one.
pub fn sealed_of(raw: &FileMetadata) -> Result<Option<Sealed>, StorageError> {
    raw.user_metadata
        .get(SEALED_KEY)
        .map(|v| Sealed::parse(v))
        .transpose()
}

/// What the read rule decides about an object without a seal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsealed {
    /// Plaintext that this bucket may serve.
    Serve,
    /// Refused: an unknown marker, or plaintext where the bucket refuses it.
    Refuse,
}

/// THE read rule for an object without a seal (pure). `plaintext_ok`:
/// the bucket writes plaintext, or encrypts with `plaintext_reads: serve`.
/// A zero-byte key that ends in `/` (a folder marker) always passes.
pub fn read_verdict(raw: &FileMetadata, key_is_folder: bool, plaintext_ok: bool) -> Unsealed {
    if raw.user_metadata.contains_key(ENCRYPTION_MARKER_KEY) {
        return Unsealed::Refuse;
    }
    if key_is_folder && raw.file_size == 0 {
        return Unsealed::Serve;
    }
    if plaintext_ok {
        Unsealed::Serve
    } else {
        Unsealed::Refuse
    }
}

/// The error of a refused unsealed object.
pub fn refusal(at: &Location<'_>, raw: &FileMetadata) -> StorageError {
    let what = match at.object {
        StoredObject::Reference => "reference.bin".to_string(),
        StoredObject::Delta(f) | StoredObject::Passthrough(f) => f.to_string(),
    };
    if raw.user_metadata.contains_key(ENCRYPTION_MARKER_KEY) {
        enc_err(format!(
            "object '{what}' in bucket '{}' carries an encryption marker but no valid sealed \
             metadata: it was written by another release, or its metadata was edited. The \
             proxy refuses it",
            at.bucket
        ))
    } else {
        enc_err(format!(
            "object '{what}' in bucket '{}' is not encrypted, but the bucket encrypts: the proxy \
             refuses it, because a writer with access to the backend could have planted it. Set \
             encryption.plaintext_reads: serve while a re-encrypt job converts the bucket's \
             history",
            at.bucket
        ))
    }
}

/// Open the seal of `raw` at `at` with a key of `ring`: the facts, and the
/// ring entry of the key.
pub fn open<'r>(
    raw: &FileMetadata,
    sealed: &Sealed,
    at: &Location<'_>,
    ring: &'r Keyring,
) -> Result<(SealedFacts, &'r RingEntry), StorageError> {
    let entry = ring.get(&sealed.fp).ok_or_else(|| {
        enc_err(format!(
            "object in bucket '{}' was encrypted with a key that this backend does not have \
             (fingerprint {}). Add that key as legacy_key, or restore it",
            at.bucket,
            hex::encode(sealed.fp)
        ))
    })?;
    let facts = entry.site.object(sealed.salt).open(at, sealed)?;
    if facts.headers != client_headers(raw) {
        return Err(enc_err(format!(
            "the clear metadata of an object in bucket '{}' was changed on the backend \
             (content type or user metadata): the proxy refuses it",
            at.bucket
        )));
    }
    Ok((facts, entry))
}

/// The logical metadata of a sealed object, as callers above the wrapper
/// see it.
pub fn present(raw: &FileMetadata, facts: &SealedFacts, key_id: &str) -> FileMetadata {
    let mut m = raw.clone();
    m.file_size = facts.file_size;
    m.md5 = facts.md5.map(hex::encode).unwrap_or_default();
    m.multipart_etag = facts
        .multipart
        .map(|(d, n)| format!("\"{}-{n}\"", hex::encode(d)));
    m.file_sha256 = hex::encode(facts.file_sha256);
    m.created_at = chrono::DateTime::<chrono::Utc>::from_timestamp_nanos(facts.created_at_nanos);
    if let StorageInfo::Delta {
        ref_sha256,
        delta_size,
        ..
    } = &mut m.storage_info
    {
        if let Some(r) = facts.ref_sha256 {
            *ref_sha256 = hex::encode(r);
        }
        if let Some(d) = facts.delta_size {
            *delta_size = d;
        }
    }
    m.user_metadata.remove(SEALED_KEY);
    m.user_metadata.insert(
        ENCRYPTION_MARKER_KEY.to_string(),
        ENCRYPTION_MARKER_VALUE.to_string(),
    );
    m.user_metadata
        .insert(ENCRYPTION_KEY_ID_KEY.to_string(), key_id.to_string());
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring() -> (Keyring, Arc<SiteKey>) {
        let mut ring = Keyring::default();
        let site = ring.insert("releases-2026", &super::super::EncryptionKey([7; 32]));
        (ring, site)
    }

    fn passthrough() -> FileMetadata {
        let mut m = FileMetadata::new_passthrough(
            "app.zip".into(),
            "ab".repeat(32),
            "cd".repeat(16),
            1234,
            Some("application/zip".into()),
        );
        m.user_metadata.insert("build".into(), "42".into());
        m
    }

    fn delta() -> FileMetadata {
        FileMetadata::new_delta(
            "app.zip".into(),
            "ab".repeat(32),
            "cd".repeat(16),
            1000,
            "reference.bin".into(),
            "ef".repeat(32),
            77,
            None,
        )
    }

    fn at(object: StoredObject<'_>) -> Location<'_> {
        location("releases", "v1/", object)
    }

    fn round_trip(meta: &FileMetadata, object: StoredObject<'_>) -> (FileMetadata, FileMetadata) {
        let (ring, site) = ring();
        let (_, keys) = site.fresh_object();
        let stored = seal(meta, &at(object), &keys).unwrap();
        let sealed = sealed_of(&stored).unwrap().unwrap();
        let (facts, entry) = open(&stored, &sealed, &at(object), &ring).unwrap();
        (stored.clone(), present(&stored, &facts, &entry.id))
    }

    #[test]
    fn a_sealed_passthrough_stores_no_hash_and_presents_the_object() {
        let meta = passthrough();
        let (stored, shown) = round_trip(&meta, StoredObject::Passthrough("app.zip"));
        assert!(stored.md5.is_empty());
        assert!(
            stored.file_sha256.starts_with("sealed:"),
            "{}",
            stored.file_sha256
        );
        assert_eq!(
            stored.file_size, 1234,
            "a passthrough keeps its size in clear"
        );
        assert_eq!(stored.user_metadata[ENCRYPTION_MARKER_KEY], DISK_MARKER);
        assert_eq!(shown.md5, meta.md5);
        assert_eq!(shown.file_sha256, meta.file_sha256);
        assert_eq!(shown.file_size, meta.file_size);
        assert_eq!(shown.created_at, meta.created_at);
        assert_eq!(shown.user_metadata["build"], "42");
        assert_eq!(shown.user_metadata[ENCRYPTION_MARKER_KEY], "v2");
        assert_eq!(shown.user_metadata[ENCRYPTION_KEY_ID_KEY], "releases-2026");
        assert!(!shown.user_metadata.contains_key(SEALED_KEY));
    }

    #[test]
    fn a_sealed_delta_hides_its_logical_size_and_reference_hash() {
        let meta = delta();
        let (stored, shown) = round_trip(&meta, StoredObject::Delta("app.zip.delta"));
        assert_eq!(stored.file_size, 0);
        assert_eq!(stored.delta_size(), Some(77), "the delta size is in clear");
        match &stored.storage_info {
            StorageInfo::Delta { ref_sha256, .. } => assert!(ref_sha256.starts_with("sealed:")),
            other => panic!("{other:?}"),
        }
        let mut want = meta.clone();
        want.user_metadata
            .insert(ENCRYPTION_MARKER_KEY.into(), "v2".into());
        want.user_metadata
            .insert(ENCRYPTION_KEY_ID_KEY.into(), "releases-2026".into());
        assert_eq!(
            serde_json::to_value(&shown).unwrap(),
            serde_json::to_value(&want).unwrap()
        );
    }

    #[test]
    fn a_multipart_etag_round_trips() {
        let mut meta = passthrough();
        meta.multipart_etag = Some(format!("\"{}-3\"", "12".repeat(16)));
        let (stored, shown) = round_trip(&meta, StoredObject::Passthrough("app.zip"));
        assert!(stored.multipart_etag.is_none());
        assert_eq!(shown.etag(), meta.etag());
    }

    #[test]
    fn edited_clear_headers_fail() {
        let (ring, site) = ring();
        let (_, keys) = site.fresh_object();
        let here = at(StoredObject::Passthrough("app.zip"));
        let stored = seal(&passthrough(), &here, &keys).unwrap();
        let sealed = sealed_of(&stored).unwrap().unwrap();
        for edit in [
            (|m: &mut FileMetadata| m.content_type = Some("text/html".into()))
                as fn(&mut FileMetadata),
            |m| {
                m.user_metadata.insert("build".into(), "43".into());
            },
            |m| {
                m.user_metadata.insert("approved".into(), "yes".into());
            },
        ] {
            let mut edited = stored.clone();
            edit(&mut edited);
            assert!(open(&edited, &sealed, &here, &ring).is_err());
        }
        assert!(open(&stored, &sealed, &here, &ring).is_ok());
    }

    #[test]
    fn a_seal_never_opens_at_another_location_or_with_an_unknown_key() {
        let (ring, site) = ring();
        let (_, keys) = site.fresh_object();
        let stored = seal(
            &passthrough(),
            &at(StoredObject::Passthrough("app.zip")),
            &keys,
        )
        .unwrap();
        let sealed = sealed_of(&stored).unwrap().unwrap();
        assert!(open(
            &stored,
            &sealed,
            &at(StoredObject::Passthrough("b.zip")),
            &ring
        )
        .is_err());
        let err = open(
            &stored,
            &sealed,
            &at(StoredObject::Passthrough("app.zip")),
            &Keyring::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("does not have"), "{err}");
    }

    #[test]
    fn the_prefix_has_one_encoding() {
        let (ring, site) = ring();
        let (_, keys) = site.fresh_object();
        let stored = seal(
            &passthrough(),
            &location("releases", "v1/", StoredObject::Passthrough("app.zip")),
            &keys,
        )
        .unwrap();
        let sealed = sealed_of(&stored).unwrap().unwrap();
        let bare = location("releases", "v1", StoredObject::Passthrough("app.zip"));
        assert!(open(&stored, &sealed, &bare, &ring).is_ok());
    }

    #[test]
    fn a_caller_cannot_plant_markers_or_a_seal() {
        let (_, site) = ring();
        let (_, keys) = site.fresh_object();
        let mut meta = passthrough();
        meta.user_metadata
            .insert(SEALED_KEY.into(), "2.forged".into());
        meta.user_metadata
            .insert(ENCRYPTION_KEY_ID_KEY.into(), "other".into());
        let stored = seal(&meta, &at(StoredObject::Passthrough("app.zip")), &keys).unwrap();
        assert_ne!(stored.user_metadata[SEALED_KEY], "2.forged");
        assert!(!stored.user_metadata.contains_key(ENCRYPTION_KEY_ID_KEY));
    }

    #[test]
    fn identity_tokens_differ_per_seal() {
        let (_, site) = ring();
        let (_, keys) = site.fresh_object();
        let here = at(StoredObject::Passthrough("app.zip"));
        let a = seal(&passthrough(), &here, &keys).unwrap();
        let b = seal(&passthrough(), &here, &keys).unwrap();
        assert_ne!(a.file_sha256, b.file_sha256);
    }

    #[test]
    fn read_verdict_truth_table() {
        let plain = passthrough();
        let mut marked = passthrough();
        marked
            .user_metadata
            .insert(ENCRYPTION_MARKER_KEY.into(), "whatever".into());
        let folder =
            FileMetadata::new_passthrough("d/".into(), String::new(), String::new(), 0, None);
        assert_eq!(read_verdict(&plain, false, true), Unsealed::Serve);
        assert_eq!(read_verdict(&plain, false, false), Unsealed::Refuse);
        assert_eq!(read_verdict(&marked, false, true), Unsealed::Refuse);
        assert_eq!(read_verdict(&folder, true, false), Unsealed::Serve);
        assert_eq!(
            read_verdict(&plain, true, false),
            Unsealed::Refuse,
            "a folder key with bytes"
        );
    }

    #[test]
    fn sealing_needs_real_hashes() {
        let (_, site) = ring();
        let (_, keys) = site.fresh_object();
        let mut meta = passthrough();
        meta.file_sha256 = String::new();
        assert!(seal(&meta, &at(StoredObject::Passthrough("app.zip")), &keys).is_err());
    }
}
