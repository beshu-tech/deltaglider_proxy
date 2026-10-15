// SPDX-License-Identifier: BUSL-1.1

//! Format v2 of the proxy's at-rest encryption (issue #120): per-object
//! keys, one body format, sealed metadata, listing tokens. Pure: no
//! backend I/O.
//!
//! # Keys
//!
//! Once per site key (`SiteKey::new`), kept for the life of the config:
//!
//! ```text
//! PRK   = HKDF-SHA256-Extract(salt = "DGE2 site", ikm = site key)
//! fp    = Expand(PRK, "DGE2 fp", 8)          key fingerprint, stored in every object
//! Klist = Expand(PRK, "DGE2 listing", 32)    listing tokens
//! ```
//!
//! Per object, from its 32-byte random salt:
//!
//! ```text
//! Kc = Expand(PRK, "DGE2 content" | 0x00 | salt)    frames
//! Km = Expand(PRK, "DGE2 meta"    | 0x00 | salt)    sealed metadata
//! C  = Expand(PRK, "DGE2 commit"  | 0x00 | salt)    key commitment
//! ```
//!
//! Only [`SiteKey::fresh_object`] gives a [`ContentSealer`]: a content key
//! encrypts exactly one body, so the frame index can be the nonce. A key
//! derived from a stored salt ([`SiteKey::opener`]) only decrypts.
//!
//! # Body
//!
//! ```text
//! header  = "DGE2" | fp (8) | salt (32) | C (32)                     76 bytes
//! frame i = AES-256-GCM(Kc, nonce = 0^4 | u64be(i),
//!                       plaintext_i, AAD = "DGE2" | u64be(i) | final)
//! ```
//!
//! Every frame but the last holds 64 KiB of plaintext; the last holds the
//! rest (1..=64 KiB, or 0 bytes only for an empty object). Frames have no
//! length field: the sealed plaintext size gives every boundary, so the
//! stored size is exactly [`stored_len`] of it.
//!
//! # Sealed metadata (`dg-sealed`)
//!
//! ```text
//! "2." b64url( fp (8) | salt (32) | C[..16] | nonce (12) | AES-256-GCM(Km, facts, AD) )
//! AD = "DGE2 meta" | 0x02 | variant | lp(real bucket) | lp(prefix) | lp(name) | salt
//! ```
//!
//! The AD binds the facts to the object's location: a body and its
//! metadata moved to another key or bucket fail. The backend's name is not
//! in the AD, so renaming a backend in the config changes nothing.
//!
//! # Listing token (`!!2` listing-facts markers)
//!
//! ```text
//! b64url( fp (8) | AES-256-GCM-SIV(Klist, nonce = 0^12, size | etag digest | parts, AD) )
//! AD = "DGE2 list" | lp(real bucket) | lp(stored key) | lp(stored etag) | u64be(stored size)
//! ```
//!
//! Deterministic: the same facts of the same stored object give the same
//! token (a marker is an object name, so a rewrite must be a no-op).

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use rand::RngCore;
use sha2::Sha256;
use zeroize::{Zeroize, Zeroizing};

use super::EncryptionKey;
use crate::storage::traits::{StorageError, StoredObject};

pub const MAGIC: [u8; 4] = *b"DGE2";
pub const FP_LEN: usize = 8;
pub const SALT_LEN: usize = 32;
pub const COMMIT_LEN: usize = 32;
/// The part of the commitment that the sealed value carries.
pub const SEALED_COMMIT_LEN: usize = 16;
pub const HEADER_LEN: usize = MAGIC.len() + FP_LEN + SALT_LEN + COMMIT_LEN;
pub const TAG_LEN: usize = 16;
pub const NONCE_LEN: usize = 12;

/// Plaintext bytes per frame. A range read decrypts at most one frame more
/// at each end than it serves.
pub const FRAME_PLAINTEXT: usize = 64 * 1024;
/// Stored bytes of every frame but the last.
pub const FRAME_WIRE_LEN: usize = FRAME_PLAINTEXT + TAG_LEN;
/// The most frames one object may have (64 PiB of plaintext).
pub const MAX_FRAMES: u64 = 1 << 40;

/// The prefix of a body in any proxy format: `DGE` and a version digit. An
/// unsealed object whose body starts so is refused, never served.
pub fn has_dge_magic(head: &[u8]) -> bool {
    head.len() >= 4 && &head[..3] == b"DGE" && head[3].is_ascii_digit()
}

fn enc_err(msg: impl Into<String>) -> StorageError {
    StorageError::Encryption(msg.into())
}

// ── Sizes ──

/// The number of frames of `plaintext` bytes, or `None` above [`MAX_FRAMES`].
pub fn frame_count(plaintext: u64) -> Option<u64> {
    let n = plaintext.div_ceil(FRAME_PLAINTEXT as u64).max(1);
    (n <= MAX_FRAMES).then_some(n)
}

/// The exact stored size of a body of `plaintext` bytes; `None` when the
/// object is too large for the format.
pub fn stored_len(plaintext: u64) -> Option<u64> {
    let frames = frame_count(plaintext)?;
    (HEADER_LEN as u64)
        .checked_add(plaintext)?
        .checked_add(frames.checked_mul(TAG_LEN as u64)?)
}

/// The plaintext size whose [`stored_len`] is `stored`, if any.
/// [`stored_len`] is strictly increasing, so the answer is unique.
pub fn plaintext_len_of(stored: u64) -> Option<u64> {
    let body = stored.checked_sub(HEADER_LEN as u64)?;
    // A body of f full-or-partial frames holds body - 16 f plaintext bytes.
    let per = (FRAME_WIRE_LEN) as u64;
    let frames = body.div_ceil(per).max(1);
    let plaintext = body.checked_sub(frames * TAG_LEN as u64)?;
    (stored_len(plaintext) == Some(stored)).then_some(plaintext)
}

/// The index of the last frame of `plaintext` bytes.
pub fn final_frame_index(plaintext: u64) -> Option<u64> {
    frame_count(plaintext).map(|n| n - 1)
}

/// The frame that holds plaintext offset `offset`, and the offset in it.
pub fn frame_for_offset(offset: u64) -> (u64, u64) {
    let sz = FRAME_PLAINTEXT as u64;
    (offset / sz, offset % sz)
}

/// The stored offset where frame `index` starts.
pub fn wire_offset_of_frame(index: u64) -> u64 {
    HEADER_LEN as u64 + index * FRAME_WIRE_LEN as u64
}

/// The stored size of frame `index` of a body of `plaintext` bytes.
fn frame_wire_len(index: u64, plaintext: u64) -> Option<usize> {
    let last = final_frame_index(plaintext)?;
    if index < last {
        Some(FRAME_WIRE_LEN)
    } else if index == last {
        Some((plaintext - last * FRAME_PLAINTEXT as u64) as usize + TAG_LEN)
    } else {
        None
    }
}

// ── Keys ──

fn expand(prk: &[u8; 32], info: &[&[u8]], out: &mut [u8]) {
    let hk = hkdf::Hkdf::<Sha256>::from_prk(prk).expect("a 32-byte PRK is valid for SHA-256");
    hk.expand_multi_info(info, out)
        .expect("output length is valid for HKDF-SHA256");
}

fn gcm(key: &[u8; 32]) -> Aes256Gcm {
    Aes256Gcm::new_from_slice(key).expect("32-byte key")
}

/// One site key, prepared for format v2. Holds the HKDF PRK (wiped on
/// drop), never the key bytes themselves.
pub struct SiteKey {
    prk: Zeroizing<[u8; 32]>,
    fp: [u8; FP_LEN],
    list: aes_gcm_siv::Aes256GcmSiv,
}

impl SiteKey {
    pub fn new(key: &EncryptionKey) -> Self {
        let (mut prk_ga, _) = hkdf::Hkdf::<Sha256>::extract(Some(b"DGE2 site"), &key.0);
        let mut prk = Zeroizing::new([0u8; 32]);
        prk.copy_from_slice(&prk_ga);
        prk_ga.as_mut_slice().zeroize();
        let mut fp = [0u8; FP_LEN];
        expand(&prk, &[b"DGE2 fp"], &mut fp);
        let mut klist = Zeroizing::new([0u8; 32]);
        expand(&prk, &[b"DGE2 listing"], klist.as_mut());
        let list = <aes_gcm_siv::Aes256GcmSiv as aes_gcm_siv::aead::KeyInit>::new_from_slice(
            klist.as_ref(),
        )
        .expect("32-byte key");
        Self { prk, fp, list }
    }

    /// The key fingerprint stored in every object this key encrypts.
    pub fn fingerprint(&self) -> [u8; FP_LEN] {
        self.fp
    }

    fn subkey(&self, label: &[u8], salt: &[u8; SALT_LEN]) -> Zeroizing<[u8; 32]> {
        let mut k = Zeroizing::new([0u8; 32]);
        expand(&self.prk, &[label, &[0u8], salt], k.as_mut());
        k
    }

    fn commit(&self, salt: &[u8; SALT_LEN]) -> [u8; COMMIT_LEN] {
        *self.subkey(b"DGE2 commit", salt)
    }

    /// The keys of a NEW object: a fresh salt, the only way to get a key
    /// that encrypts a body.
    pub fn fresh_object(&self) -> (ContentSealer, ObjectMeta) {
        let mut salt = [0u8; SALT_LEN];
        rand::rngs::OsRng.fill_bytes(&mut salt);
        let meta = self.object(salt);
        let mut header = [0u8; HEADER_LEN];
        header[..4].copy_from_slice(&MAGIC);
        header[4..4 + FP_LEN].copy_from_slice(&self.fp);
        header[4 + FP_LEN..4 + FP_LEN + SALT_LEN].copy_from_slice(&salt);
        header[4 + FP_LEN + SALT_LEN..].copy_from_slice(&self.commit(&salt));
        let sealer = ContentSealer {
            cipher: gcm(&self.subkey(b"DGE2 content", &salt)),
            header,
            index: 0,
            buf: Vec::with_capacity(FRAME_PLAINTEXT * 2),
            written: 0,
        };
        (sealer, meta)
    }

    /// The metadata keys of the stored object with `salt`.
    pub fn object(&self, salt: [u8; SALT_LEN]) -> ObjectMeta {
        let commit = self.commit(&salt);
        let mut c16 = [0u8; SEALED_COMMIT_LEN];
        c16.copy_from_slice(&commit[..SEALED_COMMIT_LEN]);
        ObjectMeta {
            cipher: gcm(&self.subkey(b"DGE2 meta", &salt)),
            salt,
            fp: self.fp,
            c16,
        }
    }

    /// The decrypt-only content key of the stored object with `salt`.
    pub fn opener(&self, salt: [u8; SALT_LEN]) -> ContentOpener {
        ContentOpener {
            cipher: gcm(&self.subkey(b"DGE2 content", &salt)),
            fp: self.fp,
            salt,
            commit: self.commit(&salt),
        }
    }

    fn list_ad(bucket: &str, stored_key: &str, stored_etag: &str, stored_size: u64) -> Vec<u8> {
        // An ETag reads quoted from one API and bare from another.
        let stored_etag = stored_etag.trim_matches('"');
        let mut ad = Vec::with_capacity(64 + stored_key.len());
        ad.extend_from_slice(b"DGE2 list");
        for f in [bucket, stored_key, stored_etag] {
            lp(&mut ad, f.as_bytes());
        }
        ad.extend_from_slice(&stored_size.to_be_bytes());
        ad
    }

    /// The listing token of a stored object's logical facts.
    pub fn listing_token(
        &self,
        at: &ListedAt<'_>,
        facts: &ListingFacts,
    ) -> Result<String, StorageError> {
        use aes_gcm_siv::aead::Aead as _;
        let mut pt = Vec::with_capacity(26);
        pt.extend_from_slice(&facts.size.to_be_bytes());
        pt.extend_from_slice(&facts.etag_digest);
        pt.extend_from_slice(&facts.parts.to_be_bytes());
        let ad = Self::list_ad(at.bucket, at.stored_key, at.stored_etag, at.stored_size);
        let ct = self
            .list
            .encrypt(
                aes_gcm_siv::Nonce::from_slice(&[0u8; NONCE_LEN]),
                aes_gcm_siv::aead::Payload { msg: &pt, aad: &ad },
            )
            .map_err(|e| enc_err(format!("listing token: {e}")))?;
        let mut blob = Vec::with_capacity(FP_LEN + ct.len());
        blob.extend_from_slice(&self.fp);
        blob.extend_from_slice(&ct);
        Ok(b64().encode(blob))
    }

    /// Open a listing token that [`token_fingerprint`] matched to this key.
    /// `None` when it does not open (forged, or another object's).
    pub fn open_listing_token(&self, at: &ListedAt<'_>, token: &str) -> Option<ListingFacts> {
        use aes_gcm_siv::aead::Aead as _;
        let blob = b64().decode(token).ok()?;
        if blob.len() != FP_LEN + 26 + TAG_LEN || blob[..FP_LEN] != self.fp {
            return None;
        }
        let ad = Self::list_ad(at.bucket, at.stored_key, at.stored_etag, at.stored_size);
        let pt = self
            .list
            .decrypt(
                aes_gcm_siv::Nonce::from_slice(&[0u8; NONCE_LEN]),
                aes_gcm_siv::aead::Payload {
                    msg: &blob[FP_LEN..],
                    aad: &ad,
                },
            )
            .ok()?;
        Some(ListingFacts {
            size: u64::from_be_bytes(pt[..8].try_into().ok()?),
            etag_digest: pt[8..24].try_into().ok()?,
            parts: u16::from_be_bytes(pt[24..26].try_into().ok()?),
        })
    }
}

/// The key fingerprint of a listing token, to pick the key that opens it.
pub fn token_fingerprint(token: &str) -> Option<[u8; FP_LEN]> {
    let blob = b64().decode(token).ok()?;
    blob.get(..FP_LEN)?.try_into().ok()
}

/// A stored object as the listing sees it: the AD of its listing token.
#[derive(Clone, Copy, Debug)]
pub struct ListedAt<'a> {
    pub bucket: &'a str,
    pub stored_key: &'a str,
    pub stored_etag: &'a str,
    pub stored_size: u64,
}

/// The logical facts a listing needs: size and ETag. The ETag is
/// `"<hex digest>"`, or `"<hex digest>-<parts>"` for a multipart upload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListingFacts {
    pub size: u64,
    pub etag_digest: [u8; 16],
    pub parts: u16,
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

/// Length-prefixed field: `u32be(len) | bytes`.
fn lp(out: &mut Vec<u8>, field: &[u8]) {
    out.extend_from_slice(&(field.len() as u32).to_be_bytes());
    out.extend_from_slice(field);
}

// ── Content ──

/// Encrypts ONE body. Buffers arbitrary input and emits a frame only once
/// it holds a full window AND more input follows, so the last frame (and
/// only it) is final whatever the input chunking.
pub struct ContentSealer {
    cipher: Aes256Gcm,
    header: [u8; HEADER_LEN],
    index: u64,
    buf: Vec<u8>,
    written: u64,
}

impl ContentSealer {
    /// The body header; it precedes the frames.
    pub fn header(&self) -> [u8; HEADER_LEN] {
        self.header
    }

    fn seal_frame(
        &mut self,
        is_final: bool,
        plaintext: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), StorageError> {
        if self.index >= MAX_FRAMES {
            return Err(enc_err("object too large for the encryption format"));
        }
        let ct = self
            .cipher
            .encrypt(
                Nonce::from_slice(&frame_nonce(self.index)),
                Payload {
                    msg: plaintext,
                    aad: &frame_aad(self.index, is_final),
                },
            )
            .map_err(|e| enc_err(format!("encrypt frame {}: {e}", self.index)))?;
        out.extend_from_slice(&ct);
        self.index += 1;
        Ok(())
    }

    /// Take more plaintext; append the frames it completes to `out`.
    pub fn push(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<(), StorageError> {
        self.written += data.len() as u64;
        self.buf.extend_from_slice(data);
        let mut start = 0;
        while self.buf.len() - start > FRAME_PLAINTEXT {
            let window = self.buf[start..start + FRAME_PLAINTEXT].to_vec();
            self.seal_frame(false, &window, out)?;
            start += FRAME_PLAINTEXT;
        }
        self.buf.drain(..start);
        Ok(())
    }

    /// Append the final frame to `out`; returns the plaintext size.
    pub fn finish(mut self, out: &mut Vec<u8>) -> Result<u64, StorageError> {
        let last = std::mem::take(&mut self.buf);
        self.seal_frame(true, &last, out)?;
        Ok(self.written)
    }

    /// The whole body of `plaintext`: header and frames.
    pub fn seal_all(mut self, plaintext: &[u8]) -> Result<Vec<u8>, StorageError> {
        let len = stored_len(plaintext.len() as u64)
            .ok_or_else(|| enc_err("object too large for the encryption format"))?;
        let mut out = Vec::with_capacity(len as usize);
        out.extend_from_slice(&self.header);
        self.push(plaintext, &mut out)?;
        self.finish(&mut out)?;
        Ok(out)
    }
}

fn frame_nonce(index: u64) -> [u8; NONCE_LEN] {
    let mut n = [0u8; NONCE_LEN];
    n[4..].copy_from_slice(&index.to_be_bytes());
    n
}

fn frame_aad(index: u64, is_final: bool) -> [u8; 13] {
    let mut aad = [0u8; 13];
    aad[..4].copy_from_slice(&MAGIC);
    aad[4..12].copy_from_slice(&index.to_be_bytes());
    aad[12] = u8::from(is_final);
    aad
}

/// Decrypts the body of one stored object.
pub struct ContentOpener {
    cipher: Aes256Gcm,
    fp: [u8; FP_LEN],
    salt: [u8; SALT_LEN],
    commit: [u8; COMMIT_LEN],
}

impl ContentOpener {
    /// Check a body header against the sealed metadata: the magic, the key,
    /// the salt (a body moved under another object's metadata fails here)
    /// and the full key commitment.
    pub fn check_header(&self, header: &[u8]) -> Result<(), StorageError> {
        if header.len() < HEADER_LEN || header[..4] != MAGIC {
            return Err(enc_err("the object body has no DGE2 header"));
        }
        if header[4..4 + FP_LEN] != self.fp {
            return Err(enc_err(
                "the object body was encrypted with another key than its metadata names",
            ));
        }
        if header[4 + FP_LEN..4 + FP_LEN + SALT_LEN] != self.salt {
            return Err(enc_err(
                "the object body does not belong to its metadata: the body was replaced \
                 or moved on the backend",
            ));
        }
        let ok: bool = subtle::ConstantTimeEq::ct_eq(
            &header[4 + FP_LEN + SALT_LEN..HEADER_LEN],
            &self.commit[..],
        )
        .into();
        if !ok {
            return Err(enc_err("key commitment mismatch in the object header"));
        }
        Ok(())
    }

    /// The plaintext of frame `index` (its stored bytes, tag included).
    pub fn open_frame(
        &self,
        index: u64,
        is_final: bool,
        wire: &[u8],
    ) -> Result<Vec<u8>, StorageError> {
        self.cipher
            .decrypt(
                Nonce::from_slice(&frame_nonce(index)),
                Payload {
                    msg: wire,
                    aad: &frame_aad(index, is_final),
                },
            )
            .map_err(|_| {
                enc_err(format!(
                    "frame {index} failed authentication (tampered, reordered or truncated)"
                ))
            })
    }

    /// The plaintext of a whole stored body whose sealed size is `body_len`.
    pub fn open_all(&self, wire: &[u8], body_len: u64) -> Result<Vec<u8>, StorageError> {
        check_stored_len(wire.len() as u64, body_len)?;
        self.check_header(&wire[..HEADER_LEN])?;
        let last = final_frame_index(body_len).expect("checked by check_stored_len");
        let mut out = Vec::with_capacity(body_len as usize);
        let mut pos = HEADER_LEN;
        for index in 0..=last {
            let len = frame_wire_len(index, body_len).expect("index <= last");
            out.extend_from_slice(&self.open_frame(index, index == last, &wire[pos..pos + len])?);
            pos += len;
        }
        debug_assert_eq!(pos, wire.len());
        Ok(out)
    }
}

/// A stored size that is not [`stored_len`] of the sealed plaintext size
/// means the body was cut, extended or replaced.
pub fn check_stored_len(stored: u64, body_len: u64) -> Result<(), StorageError> {
    match stored_len(body_len) {
        Some(want) if want == stored => Ok(()),
        Some(want) => Err(enc_err(format!(
            "stored size {stored} does not match the sealed size {body_len} (expected {want} \
             stored bytes): the object was tampered with"
        ))),
        None => Err(enc_err("sealed size too large for the encryption format")),
    }
}

/// The frame decoder: `inner` starts at frame `first` (after the header)
/// of a body of `body_len` plaintext bytes. Emits plaintext from `skip`
/// bytes into frame `first`, at most `take` bytes. A stream that ends early
/// or carries bytes after the final frame fails.
pub fn decrypt_frames<S>(
    inner: S,
    opener: ContentOpener,
    first: u64,
    body_len: u64,
    skip: u64,
    take: Option<u64>,
) -> futures::stream::BoxStream<'static, Result<bytes::Bytes, StorageError>>
where
    S: futures::Stream<Item = Result<bytes::Bytes, StorageError>> + Unpin + Send + 'static,
{
    struct St<S> {
        inner: S,
        opener: ContentOpener,
        buf: Vec<u8>,
        index: u64,
        last: u64,
        body_len: u64,
        done: bool,
        skip: u64,
        take: Option<u64>,
    }
    async fn next_frame<S>(st: &mut St<S>) -> Result<Vec<u8>, StorageError>
    where
        S: futures::Stream<Item = Result<bytes::Bytes, StorageError>> + Unpin,
    {
        use futures::StreamExt;
        let len = frame_wire_len(st.index, st.body_len)
            .ok_or_else(|| enc_err("frame index past the end of the object"))?;
        while st.buf.len() < len {
            match st.inner.next().await {
                Some(more) => st.buf.extend_from_slice(&more?),
                None => {
                    return Err(enc_err(format!(
                        "object body truncated in frame {} of {}",
                        st.index, st.last
                    )))
                }
            }
        }
        let is_final = st.index == st.last;
        let pt = st.opener.open_frame(st.index, is_final, &st.buf[..len])?;
        st.buf.drain(..len);
        if is_final {
            // Bytes after the final frame: the body was extended.
            if !st.buf.is_empty() {
                return Err(enc_err(
                    "bytes after the final frame: the body was extended",
                ));
            }
            while let Some(more) = st.inner.next().await {
                if !more?.is_empty() {
                    return Err(enc_err(
                        "bytes after the final frame: the body was extended",
                    ));
                }
            }
        }
        st.index += 1;
        Ok(pt)
    }
    let Some(last) = final_frame_index(body_len) else {
        return Box::pin(futures::stream::once(async {
            Err(enc_err("sealed size too large for the encryption format"))
        }));
    };
    if first > last {
        return Box::pin(futures::stream::once(async move {
            Err(enc_err(format!(
                "frame {first} is past the last frame {last}"
            )))
        }));
    }
    let st = St {
        inner,
        opener,
        buf: Vec::with_capacity(FRAME_WIRE_LEN + 64),
        index: first,
        last,
        body_len,
        done: false,
        skip,
        take,
    };
    Box::pin(futures::stream::unfold(st, |mut st| async move {
        loop {
            if st.done || matches!(st.take, Some(0)) {
                return None;
            }
            let pt = match next_frame(&mut st).await {
                Ok(pt) => pt,
                Err(e) => {
                    st.done = true;
                    return Some((Err(e), st));
                }
            };
            if st.index > st.last {
                st.done = true;
            }
            let start = st.skip.min(pt.len() as u64) as usize;
            st.skip -= start as u64;
            let mut rest = &pt[start..];
            if let Some(take) = st.take.as_mut() {
                let n = (*take).min(rest.len() as u64) as usize;
                rest = &rest[..n];
                *take -= n as u64;
            }
            if rest.is_empty() {
                continue;
            }
            return Some((Ok(bytes::Bytes::copy_from_slice(rest)), st));
        }
    }))
}

// ── Sealed metadata ──

/// The metadata keys of one stored object.
pub struct ObjectMeta {
    cipher: Aes256Gcm,
    salt: [u8; SALT_LEN],
    fp: [u8; FP_LEN],
    c16: [u8; SEALED_COMMIT_LEN],
}

/// The prefix of a v2 sealed value.
pub const SEALED_PREFIX: &str = "2.";

impl ObjectMeta {
    #[cfg(test)]
    pub fn salt(&self) -> [u8; SALT_LEN] {
        self.salt
    }

    fn ad(&self, at: &Location<'_>) -> Vec<u8> {
        let mut ad = Vec::with_capacity(96);
        ad.extend_from_slice(b"DGE2 meta");
        ad.push(0x02);
        at.encode_into(&mut ad);
        ad.extend_from_slice(&self.salt);
        ad
    }

    /// The sealed value of `facts` for the object at `at`. A fresh random
    /// nonce per seal: a metadata rewrite re-seals under the same key.
    pub fn seal(&self, at: &Location<'_>, facts: &SealedFacts) -> Result<String, StorageError> {
        let plain = Zeroizing::new(facts.encode());
        let mut nonce = [0u8; NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let ct = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &plain,
                    aad: &self.ad(at),
                },
            )
            .map_err(|e| enc_err(format!("seal: {e}")))?;
        let mut blob =
            Vec::with_capacity(FP_LEN + SALT_LEN + SEALED_COMMIT_LEN + NONCE_LEN + ct.len());
        blob.extend_from_slice(&self.fp);
        blob.extend_from_slice(&self.salt);
        blob.extend_from_slice(&self.c16);
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ct);
        Ok(format!("{SEALED_PREFIX}{}", b64().encode(blob)))
    }

    /// Open `sealed` for the object at `at`. Fails when it was sealed for
    /// another location, with another key, or was edited.
    pub fn open(&self, at: &Location<'_>, sealed: &Sealed) -> Result<SealedFacts, StorageError> {
        if sealed.fp != self.fp || sealed.salt != self.salt {
            return Err(enc_err("sealed metadata belongs to another key or object"));
        }
        let ok: bool = subtle::ConstantTimeEq::ct_eq(&sealed.c16[..], &self.c16[..]).into();
        if !ok {
            return Err(enc_err("key commitment mismatch in the sealed metadata"));
        }
        let plain = Zeroizing::new(
            self.cipher
                .decrypt(
                    Nonce::from_slice(&sealed.nonce),
                    Payload {
                        msg: &sealed.ct,
                        aad: &self.ad(at),
                    },
                )
                .map_err(|_| {
                    enc_err(format!(
                        "the sealed metadata of {} failed authentication: it was edited, or \
                         the object was moved or copied on the backend",
                        at.describe()
                    ))
                })?,
        );
        let facts = SealedFacts::decode(&plain)?;
        if matches!(at.object, StoredObject::Delta(_)) && facts.ref_sha256.is_none() {
            return Err(enc_err("sealed delta metadata has no reference SHA-256"));
        }
        Ok(facts)
    }
}

/// A parsed, still sealed `dg-sealed` value. The fingerprint and the salt
/// are readable, so a reader can pick the key and derive the object keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sealed {
    pub fp: [u8; FP_LEN],
    pub salt: [u8; SALT_LEN],
    c16: [u8; SEALED_COMMIT_LEN],
    nonce: [u8; NONCE_LEN],
    ct: Vec<u8>,
}

impl Sealed {
    pub fn parse(value: &str) -> Result<Self, StorageError> {
        let bad = || enc_err("malformed dg-sealed metadata");
        let blob = value
            .strip_prefix(SEALED_PREFIX)
            .ok_or_else(bad)
            .and_then(|b| b64().decode(b).map_err(|_| bad()))?;
        let head = FP_LEN + SALT_LEN + SEALED_COMMIT_LEN + NONCE_LEN;
        if blob.len() < head + TAG_LEN {
            return Err(bad());
        }
        let (fp, rest) = blob.split_at(FP_LEN);
        let (salt, rest) = rest.split_at(SALT_LEN);
        let (c16, rest) = rest.split_at(SEALED_COMMIT_LEN);
        let (nonce, ct) = rest.split_at(NONCE_LEN);
        Ok(Self {
            fp: fp.try_into().map_err(|_| bad())?,
            salt: salt.try_into().map_err(|_| bad())?,
            c16: c16.try_into().map_err(|_| bad())?,
            nonce: nonce.try_into().map_err(|_| bad())?,
            ct: ct.to_vec(),
        })
    }
}

/// Where an object is stored: the AD of its sealed metadata. The real
/// bucket and the stored key bind it, the backend's name does not.
#[derive(Clone, Copy, Debug)]
pub struct Location<'a> {
    pub bucket: &'a str,
    /// The deltaspace prefix, without a trailing `/`.
    pub prefix: &'a str,
    pub object: StoredObject<'a>,
}

impl Location<'_> {
    fn encode_into(&self, out: &mut Vec<u8>) {
        debug_assert!(!self.prefix.ends_with('/'), "one canonical split");
        let (tag, name) = match self.object {
            StoredObject::Reference => (b'r', ""),
            StoredObject::Delta(f) => (b'd', f),
            StoredObject::Passthrough(f) => (b'p', f),
        };
        debug_assert!(!name.contains('/'), "one canonical split");
        out.push(tag);
        for field in [self.bucket, self.prefix, name] {
            lp(out, field.as_bytes());
        }
    }

    fn describe(&self) -> String {
        let name = match self.object {
            StoredObject::Reference => "reference.bin",
            StoredObject::Delta(f) | StoredObject::Passthrough(f) => f,
        };
        if self.prefix.is_empty() {
            format!("{}/{name}", self.bucket)
        } else {
            format!("{}/{}/{name}", self.bucket, self.prefix)
        }
    }
}

/// The facts that the backend must neither read nor change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SealedFacts {
    /// Logical (plaintext) size of the object.
    pub file_size: u64,
    /// MD5 of the content; `None` for a delta from an older toolchain.
    pub md5: Option<[u8; 16]>,
    /// Multipart ETag: MD5 of the part MD5s, and the part count.
    pub multipart: Option<([u8; 16], u16)>,
    pub file_sha256: [u8; 32],
    /// Deltas: the reference SHA-256 and the plaintext delta size.
    pub ref_sha256: Option<[u8; 32]>,
    pub delta_size: Option<u64>,
    /// Creation time, Unix nanoseconds.
    pub created_at_nanos: i64,
    /// [`client_headers_digest`] of the stored client-visible headers.
    pub headers: [u8; 16],
}

const FACTS_VERSION: u8 = 1;
const F_MD5: u8 = 1;
const F_MULTIPART: u8 = 2;
const F_REF: u8 = 4;
const F_DELTA_SIZE: u8 = 8;

impl SealedFacts {
    fn encode(&self) -> Vec<u8> {
        let mut flags = 0u8;
        let mut out = Vec::with_capacity(160);
        out.push(FACTS_VERSION);
        out.push(0);
        out.extend_from_slice(&self.file_size.to_be_bytes());
        out.extend_from_slice(&self.file_sha256);
        out.extend_from_slice(&self.created_at_nanos.to_be_bytes());
        out.extend_from_slice(&self.headers);
        if let Some(md5) = &self.md5 {
            flags |= F_MD5;
            out.extend_from_slice(md5);
        }
        if let Some((digest, parts)) = &self.multipart {
            flags |= F_MULTIPART;
            out.extend_from_slice(digest);
            out.extend_from_slice(&parts.to_be_bytes());
        }
        if let Some(r) = &self.ref_sha256 {
            flags |= F_REF;
            out.extend_from_slice(r);
        }
        if let Some(d) = self.delta_size {
            flags |= F_DELTA_SIZE;
            out.extend_from_slice(&d.to_be_bytes());
        }
        out[1] = flags;
        out
    }

    fn decode(b: &[u8]) -> Result<Self, StorageError> {
        let bad = || enc_err("malformed sealed facts");
        let mut r = Reader(b);
        if r.take(1).ok_or_else(bad)? != [FACTS_VERSION] {
            return Err(enc_err("sealed facts of an unknown version"));
        }
        let flags = r.take(1).ok_or_else(bad)?[0];
        if flags & !(F_MD5 | F_MULTIPART | F_REF | F_DELTA_SIZE) != 0 {
            return Err(bad());
        }
        let mut facts = Self {
            file_size: r.u64().ok_or_else(bad)?,
            file_sha256: r.array().ok_or_else(bad)?,
            created_at_nanos: r.u64().ok_or_else(bad)? as i64,
            headers: r.array().ok_or_else(bad)?,
            ..Default::default()
        };
        if flags & F_MD5 != 0 {
            facts.md5 = Some(r.array().ok_or_else(bad)?);
        }
        if flags & F_MULTIPART != 0 {
            let digest = r.array().ok_or_else(bad)?;
            let parts = u16::from_be_bytes(r.array().ok_or_else(bad)?);
            facts.multipart = Some((digest, parts));
        }
        if flags & F_REF != 0 {
            facts.ref_sha256 = Some(r.array().ok_or_else(bad)?);
        }
        if flags & F_DELTA_SIZE != 0 {
            facts.delta_size = Some(r.u64().ok_or_else(bad)?);
        }
        if !r.0.is_empty() {
            return Err(bad());
        }
        Ok(facts)
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }
    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }
    fn u64(&mut self) -> Option<u64> {
        self.array().map(u64::from_be_bytes)
    }
}

/// The digest of the client-visible headers that a backend writer could
/// change: content type, original name, client user metadata. Keys are
/// lowercased and sorted; every field is length-prefixed.
pub fn client_headers_digest<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> [u8; 16] {
    use sha2::Digest as _;
    let mut sorted: Vec<(String, &str)> = pairs
        .into_iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v))
        .collect();
    sorted.sort();
    let mut h = Sha256::new();
    h.update(b"DGE2 headers");
    for (k, v) in sorted {
        let mut field = Vec::with_capacity(8 + k.len() + v.len());
        lp(&mut field, k.as_bytes());
        lp(&mut field, v.as_bytes());
        h.update(&field);
    }
    let d = h.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&d[..16]);
    out
}

/// Fuzz entry: the backend controls both inputs, so neither may panic.
pub(crate) fn fuzz_sealed_and_frames(data: &[u8]) {
    let site = SiteKey::new(&EncryptionKey([0x42; 32]));
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(sealed) = Sealed::parse(text) {
            let at = Location {
                bucket: "b",
                prefix: "",
                object: StoredObject::Passthrough("k"),
            };
            let _ = site.object(sealed.salt).open(&at, &sealed);
        }
        let _ = token_fingerprint(text);
    }
    let _ = SealedFacts::decode(data);
    let mut salt = [0u8; SALT_LEN];
    let n = data.len().min(SALT_LEN);
    salt[..n].copy_from_slice(&data[..n]);
    let body_len = plaintext_len_of(data.len() as u64).unwrap_or(0);
    let _ = site.opener(salt).open_all(data, body_len);
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn site(b: u8) -> SiteKey {
        SiteKey::new(&EncryptionKey([b; 32]))
    }

    fn here(name: &str) -> Location<'_> {
        Location {
            bucket: "releases",
            prefix: "v1",
            object: StoredObject::Passthrough(name),
        }
    }

    fn facts() -> SealedFacts {
        SealedFacts {
            file_size: 5,
            md5: Some([7; 16]),
            file_sha256: [9; 32],
            created_at_nanos: 1_700_000_000_000_000_000,
            headers: client_headers_digest([("content-type", "application/zip")]),
            ..Default::default()
        }
    }

    fn seal_body(site: &SiteKey, plaintext: &[u8]) -> (Vec<u8>, ObjectMeta) {
        let (sealer, meta) = site.fresh_object();
        (sealer.seal_all(plaintext).unwrap(), meta)
    }

    #[test]
    fn stored_len_is_exact_at_every_boundary() {
        let s = site(1);
        let f = FRAME_PLAINTEXT;
        for n in [0, 1, f - 1, f, f + 1, 2 * f, 2 * f + 7] {
            let (body, meta) = seal_body(&s, &vec![7u8; n]);
            assert_eq!(Some(body.len() as u64), stored_len(n as u64), "n={n}");
            assert_eq!(plaintext_len_of(body.len() as u64), Some(n as u64), "n={n}");
            let plain = s.opener(meta.salt()).open_all(&body, n as u64).unwrap();
            assert_eq!(plain, vec![7u8; n]);
        }
    }

    #[test]
    fn the_sealer_frames_any_chunking_the_same_way() {
        let s = site(1);
        let n = 3 * FRAME_PLAINTEXT;
        let plain: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
        for chunk in [1usize, 1000, FRAME_PLAINTEXT, FRAME_PLAINTEXT + 1, n] {
            let (mut sealer, meta) = s.fresh_object();
            let mut out = sealer.header().to_vec();
            sealer.push(&[], &mut out).unwrap();
            for c in plain.chunks(chunk) {
                sealer.push(c, &mut out).unwrap();
                sealer.push(&[], &mut out).unwrap();
            }
            assert_eq!(sealer.finish(&mut out).unwrap(), n as u64);
            assert_eq!(
                Some(out.len() as u64),
                stored_len(n as u64),
                "chunk={chunk}"
            );
            assert_eq!(
                s.opener(meta.salt()).open_all(&out, n as u64).unwrap(),
                plain
            );
        }
    }

    #[test]
    fn plaintext_len_of_rejects_impossible_sizes() {
        for stored in 0..HEADER_LEN as u64 + TAG_LEN as u64 {
            assert_eq!(plaintext_len_of(stored), None, "{stored}");
        }
        let full = stored_len(FRAME_PLAINTEXT as u64).unwrap();
        // One more byte would be a second frame of 1 byte: + 17 bytes.
        for stored in full + 1..full + 1 + TAG_LEN as u64 {
            assert_eq!(plaintext_len_of(stored), None, "{stored}");
        }
        assert_eq!(
            plaintext_len_of(full + 1 + TAG_LEN as u64),
            Some(FRAME_PLAINTEXT as u64 + 1)
        );
    }

    #[test]
    fn each_object_gets_its_own_keys() {
        let s = site(1);
        let (a, _) = seal_body(&s, b"same");
        let (b, _) = seal_body(&s, b"same");
        assert_eq!(a[4..12], b[4..12], "one fingerprint per key");
        assert_ne!(a[12..44], b[12..44], "one salt per object");
        assert_ne!(a[HEADER_LEN..], b[HEADER_LEN..]);
        assert_ne!(site(1).fingerprint(), site(2).fingerprint());
    }

    #[test]
    fn a_wrong_key_or_a_foreign_body_fails_at_the_header() {
        let s = site(1);
        let (body, meta) = seal_body(&s, b"hello");
        let err = site(2)
            .opener(meta.salt())
            .open_all(&body, 5)
            .unwrap_err()
            .to_string();
        assert!(err.contains("another key"), "{err}");
        let (other, _) = seal_body(&s, b"world");
        let err = s
            .opener(meta.salt())
            .open_all(&other, 5)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not belong"), "{err}");
        let mut forged = body.clone();
        forged[HEADER_LEN - 1] ^= 1;
        let err = s
            .opener(meta.salt())
            .open_all(&forged, 5)
            .unwrap_err()
            .to_string();
        assert!(err.contains("commitment"), "{err}");
    }

    #[test]
    fn tampering_with_the_body_fails() {
        let s = site(1);
        let n = 2 * FRAME_PLAINTEXT + 5;
        let (body, meta) = seal_body(&s, &vec![1u8; n]);
        let open = |b: &[u8]| s.opener(meta.salt()).open_all(b, n as u64);
        let mut flipped = body.clone();
        flipped[HEADER_LEN + 10] ^= 1;
        assert!(open(&flipped).is_err());
        assert!(
            open(&body[..wire_offset_of_frame(2) as usize]).is_err(),
            "cut"
        );
        let mut longer = body.clone();
        longer.push(0);
        assert!(open(&longer).is_err(), "extended");
        let (f0, f1) = (
            wire_offset_of_frame(0) as usize,
            wire_offset_of_frame(1) as usize,
        );
        let mut swapped = body.clone();
        swapped[f0..f1].copy_from_slice(&body[f1..f1 + FRAME_WIRE_LEN]);
        swapped[f1..f1 + FRAME_WIRE_LEN].copy_from_slice(&body[f0..f1]);
        assert!(open(&swapped).is_err(), "reordered");
        // The same body read as a shorter object: frame 1 is not final.
        let cut_len = FRAME_PLAINTEXT as u64;
        let first = &body[..stored_len(cut_len).unwrap() as usize];
        assert!(
            s.opener(meta.salt()).open_all(first, cut_len).is_err(),
            "final flag"
        );
    }

    #[test]
    fn sealed_facts_bind_the_location_the_key_and_the_content() {
        let s = site(1);
        let (_, meta) = s.fresh_object();
        let v = meta.seal(&here("app.zip"), &facts()).unwrap();
        let sealed = Sealed::parse(&v).unwrap();
        let again = s.object(sealed.salt);
        assert_eq!(again.open(&here("app.zip"), &sealed).unwrap(), facts());
        assert!(
            again.open(&here("other.zip"), &sealed).is_err(),
            "another key"
        );
        let other_bucket = Location {
            bucket: "downloads",
            ..here("app.zip")
        };
        assert!(
            again.open(&other_bucket, &sealed).is_err(),
            "another bucket"
        );
        let other_prefix = Location {
            prefix: "v2",
            ..here("app.zip")
        };
        assert!(
            again.open(&other_prefix, &sealed).is_err(),
            "another folder"
        );
        let as_delta = Location {
            object: StoredObject::Delta("app.zip"),
            ..here("app.zip")
        };
        assert!(again.open(&as_delta, &sealed).is_err(), "another variant");
        assert!(site(2)
            .object(sealed.salt)
            .open(&here("app.zip"), &sealed)
            .is_err());
        let mut blob = b64()
            .decode(v.strip_prefix(SEALED_PREFIX).unwrap())
            .unwrap();
        *blob.last_mut().unwrap() ^= 1;
        let edited = Sealed::parse(&format!("{SEALED_PREFIX}{}", b64().encode(blob))).unwrap();
        assert!(again.open(&here("app.zip"), &edited).is_err(), "edited");
        // Two seals of the same facts differ (random nonce).
        assert_ne!(v, meta.seal(&here("app.zip"), &facts()).unwrap());
    }

    #[test]
    fn a_sealed_delta_needs_its_reference_hash() {
        let s = site(1);
        let (_, meta) = s.fresh_object();
        let at = Location {
            object: StoredObject::Delta("app.zip"),
            ..here("app.zip")
        };
        let v = meta.seal(&at, &facts()).unwrap();
        assert!(meta.open(&at, &Sealed::parse(&v).unwrap()).is_err());
        let with_ref = SealedFacts {
            ref_sha256: Some([3; 32]),
            delta_size: Some(2),
            ..facts()
        };
        let v = meta.seal(&at, &with_ref).unwrap();
        assert_eq!(
            meta.open(&at, &Sealed::parse(&v).unwrap()).unwrap(),
            with_ref
        );
    }

    #[test]
    fn a_sealed_value_fits_the_metadata_budget() {
        let s = site(1);
        let (_, meta) = s.fresh_object();
        let full = SealedFacts {
            file_size: u64::MAX,
            md5: Some([0xff; 16]),
            multipart: Some(([0xff; 16], 10_000)),
            file_sha256: [0xff; 32],
            ref_sha256: Some([0xff; 32]),
            delta_size: Some(u64::MAX),
            created_at_nanos: i64::MAX,
            headers: [0xff; 16],
        };
        let v = meta.seal(&here("k"), &full).unwrap();
        assert!(v.len() <= 330, "{} chars", v.len());
        assert_eq!(
            meta.open(&here("k"), &Sealed::parse(&v).unwrap()).unwrap(),
            full
        );
    }

    #[test]
    fn malformed_sealed_values_are_errors_not_panics() {
        for v in [
            "",
            "2.",
            "1.AAAA",
            "2.!!!!",
            "2.AAAA",
            &format!("2.{}", "A".repeat(40)),
        ] {
            assert!(Sealed::parse(v).is_err(), "{v}");
        }
        assert!(SealedFacts::decode(&[]).is_err());
        assert!(SealedFacts::decode(&[FACTS_VERSION, 0x80]).is_err());
        let mut long = facts().encode();
        long.push(0);
        assert!(SealedFacts::decode(&long).is_err());
    }

    #[test]
    fn the_client_headers_digest_ignores_order_and_key_case() {
        let a = client_headers_digest([("Content-Type", "text/plain"), ("x", "1")]);
        let b = client_headers_digest([("x", "1"), ("content-type", "text/plain")]);
        assert_eq!(a, b);
        assert_ne!(
            a,
            client_headers_digest([("content-type", "text/html"), ("x", "1")])
        );
        // Length prefixes keep field boundaries.
        assert_ne!(
            client_headers_digest([("ab", "c")]),
            client_headers_digest([("a", "bc")])
        );
    }

    #[test]
    fn listing_tokens_are_deterministic_and_bound_to_the_stored_object() {
        let s = site(1);
        let at = ListedAt {
            bucket: "releases",
            stored_key: "v1/app.zip",
            stored_etag: "\"abc\"",
            stored_size: 1234,
        };
        let lf = ListingFacts {
            size: 1000,
            etag_digest: [5; 16],
            parts: 0,
        };
        let t = s.listing_token(&at, &lf).unwrap();
        assert_eq!(t, s.listing_token(&at, &lf).unwrap(), "deterministic");
        assert_eq!(token_fingerprint(&t), Some(s.fingerprint()));
        assert_eq!(s.open_listing_token(&at, &t), Some(lf));
        for moved in [
            ListedAt {
                bucket: "downloads",
                ..at
            },
            ListedAt {
                stored_key: "v1/other.zip",
                ..at
            },
            ListedAt {
                stored_etag: "\"abd\"",
                ..at
            },
            ListedAt {
                stored_size: 1235,
                ..at
            },
        ] {
            assert_eq!(s.open_listing_token(&moved, &t), None, "{moved:?}");
        }
        assert_eq!(site(2).open_listing_token(&at, &t), None);
        assert_eq!(s.open_listing_token(&at, "garbage"), None);
    }

    #[test]
    fn the_dge_magic_covers_every_version() {
        assert!(has_dge_magic(b"DGE1...."));
        assert!(has_dge_magic(b"DGE2"));
        assert!(has_dge_magic(b"DGE9x"));
        assert!(!has_dge_magic(b"DGEx"));
        assert!(!has_dge_magic(b"DGE"));
        assert!(!has_dge_magic(b"PK\x03\x04"));
    }

    async fn collect(
        s: futures::stream::BoxStream<'static, Result<bytes::Bytes, StorageError>>,
    ) -> Result<Vec<u8>, StorageError> {
        let mut out = Vec::new();
        futures::pin_mut!(s);
        while let Some(b) = s.next().await {
            out.extend_from_slice(&b?);
        }
        Ok(out)
    }

    fn chunked(
        b: &[u8],
        size: usize,
    ) -> futures::stream::BoxStream<'static, Result<bytes::Bytes, StorageError>> {
        let mut parts: Vec<Result<bytes::Bytes, StorageError>> = b
            .chunks(size.max(1))
            .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
            .collect();
        parts.push(Ok(bytes::Bytes::new()));
        Box::pin(futures::stream::iter(parts))
    }

    #[tokio::test]
    async fn the_stream_decoder_serves_every_range() {
        let s = site(3);
        let n = 3 * FRAME_PLAINTEXT + 100;
        let plain: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
        let (body, meta) = seal_body(&s, &plain);
        let f = FRAME_PLAINTEXT;
        for (a, b) in [
            (0, n - 1),
            (5, 10),
            (f - 1, f),
            (n - 3, n - 1),
            (2 * f, 3 * f - 1),
        ] {
            let (first, off) = frame_for_offset(a as u64);
            let wire = &body[wire_offset_of_frame(first) as usize..];
            let out = collect(decrypt_frames(
                chunked(wire, 7000),
                s.opener(meta.salt()),
                first,
                n as u64,
                off,
                Some((b - a + 1) as u64),
            ))
            .await
            .unwrap();
            assert_eq!(out, plain[a..=b], "{a}..={b}");
        }
    }

    #[tokio::test]
    async fn the_stream_decoder_refuses_truncation_and_extension() {
        let s = site(3);
        let n = 2 * FRAME_PLAINTEXT;
        let (body, meta) = seal_body(&s, &vec![9u8; n]);
        let frames = body[HEADER_LEN..].to_vec();
        let dec = |b: Vec<u8>| {
            decrypt_frames(
                chunked(&b, 1000),
                s.opener(meta.salt()),
                0,
                n as u64,
                0,
                None,
            )
        };
        assert_eq!(collect(dec(frames.clone())).await.unwrap().len(), n);
        assert!(collect(dec(frames[..FRAME_WIRE_LEN].to_vec()))
            .await
            .is_err());
        let mut longer = frames.clone();
        longer.push(0);
        assert!(collect(dec(longer)).await.is_err());
        let past = decrypt_frames(
            chunked(&frames, 1000),
            s.opener(meta.salt()),
            5,
            n as u64,
            0,
            None,
        );
        assert!(collect(past).await.is_err());
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]
        #[test]
        fn plaintext_len_of_inverts_stored_len(n in 0u64..(1 << 40)) {
            let stored = stored_len(n).unwrap();
            proptest::prop_assert_eq!(plaintext_len_of(stored), Some(n));
            proptest::prop_assert!(stored_len(n + 1).unwrap() > stored);
        }

        #[test]
        fn sealed_facts_round_trip(
            file_size: u64, md5: Option<[u8; 16]>, multipart: Option<([u8; 16], u16)>,
            file_sha256: [u8; 32], ref_sha256: Option<[u8; 32]>, delta_size: Option<u64>,
            created_at_nanos: i64, headers: [u8; 16],
        ) {
            let f = SealedFacts {
                file_size, md5, multipart, file_sha256, ref_sha256, delta_size,
                created_at_nanos, headers,
            };
            proptest::prop_assert_eq!(SealedFacts::decode(&f.encode()).unwrap(), f);
        }
    }
}
