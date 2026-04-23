//! Transparent encryption-at-rest wrapper for any StorageBackend.
//!
//! `EncryptingBackend<B>` wraps a storage backend and encrypts all object data
//! with AES-256-GCM before writing, decrypting on read. Metadata is NOT encrypted.
//!
//! # Two wire formats
//!
//! **`aes-256-gcm-v1`** (single-shot, original) — used for `put_reference`,
//! `put_delta`, `put_passthrough`. These bodies are bounded by
//! `max_object_size` (default 100 MiB) so whole-blob encryption is fine.
//!
//! ```text
//! [12-byte IV] [ciphertext + 16-byte GCM tag]
//! ```
//! Overhead: 28 bytes per object.
//!
//! **`aes-256-gcm-chunked-v1`** (chunked, streaming reads) — used ONLY for
//! `put_passthrough_chunked`. The format exists so the read path can
//! decrypt chunk-by-chunk with bounded peak memory and do O(1) range reads
//! on large objects. The WRITE path is not fully streaming in this
//! release — the wrapper buffers all encrypted frames before handing them
//! to the inner backend's chunked PUT. Peak write memory ≈ ciphertext size.
//! The engine's `max_object_size` ceiling (default 100 MiB) keeps this
//! bounded; if operators raise it, they should budget RAM accordingly.
//!
//! ```text
//! [4-byte magic "DGE1"] [12-byte base_iv]
//! | [4-byte u32 LE frame_len] [ciphertext + 16-byte GCM tag]    # chunk 0
//! | [4-byte u32 LE frame_len] [ciphertext + 16-byte GCM tag]    # chunk 1
//! | ...
//! | [4-byte u32 LE frame_len] [ciphertext + 16-byte GCM tag]    # chunk N (final)
//! ```
//!
//! Each chunk's nonce = `base_iv XOR (chunk_index as big-endian u96)` — unique
//! for 2^32 chunks (256 TiB at 64 KiB each). The AAD for chunk `i` is 16 bytes:
//! `"DGE1" || chunk_index_le_u32 || final_flag_u8 || 0x00 0x00 0x00`, binding
//! the index (foils reordering) and the final flag (foils truncation — the
//! former last-chunk's `final_flag=0` AAD wouldn't match after a truncation).
//!
//! Every non-final chunk is exactly `4 + 64 * 1024 + 16 = 65556` wire bytes,
//! which lets range reads compute chunk offsets in O(1) without scanning
//! the frame-length prefixes.
//!
//! # Detection
//!
//! Objects with `dg-encrypted: aes-256-gcm-v1` → single-shot decrypt.
//! Objects with `dg-encrypted: aes-256-gcm-chunked-v1` → chunked decrypt.
//! Objects without the marker → returned as-is (backward compatible).

use super::traits::{DelegatedListResult, StorageBackend, StorageError};
use crate::types::FileMetadata;
use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use rand::RngCore;
use std::sync::Arc;

pub const ENCRYPTION_MARKER_KEY: &str = "dg-encrypted";
pub const ENCRYPTION_MARKER_VALUE: &str = "aes-256-gcm-v1";
pub const CHUNK_MARKER_VALUE: &str = "aes-256-gcm-chunked-v1";
/// Metadata field stamping the per-object key_id of the key that
/// encrypted it. Lets reads detect "this object was encrypted with a
/// key I don't currently have configured" and emit a SPECIFIC error
/// (cites both ids) instead of the opaque AEAD auth failure.
///
/// Legacy objects without this field fall through unchanged — the
/// mismatch check only fires when BOTH sides have a key_id, so the
/// upgrade path for pre-key-id objects is a no-op.
pub const ENCRYPTION_KEY_ID_KEY: &str = "dg-encryption-key-id";
const IV_LEN: usize = 12;
const GCM_TAG_LEN: usize = 16;

// ── Chunked format constants ──
//
// Plaintext chunk size of 64 KiB was picked for four reasons:
//   1. Overhead = 4 B length prefix + 16 B GCM tag = 20 B per chunk ≈ 0.03%.
//   2. Range-read trim cost is at most one extra chunk at each end (≤128 KiB).
//   3. Worker memory per in-flight chunk: ~130 KiB — trivial.
//   4. Nonce space: 2^32 chunks × 64 KiB = 256 TiB per object.
const CHUNK_MAGIC: [u8; 4] = *b"DGE1";
pub const CHUNK_PLAINTEXT_SIZE: usize = 64 * 1024;
const CHUNK_FRAME_LEN_FIELD: usize = 4;
const CHUNK_HEADER_LEN: usize = 4 /*magic*/ + 12 /*base_iv*/;
/// Wire size of every non-final chunk (length-prefix + ciphertext + tag).
pub const CHUNK_FRAME_WIRE_LEN: usize = CHUNK_FRAME_LEN_FIELD + CHUNK_PLAINTEXT_SIZE + GCM_TAG_LEN;
/// Cap on the length-prefix to foil DOS-via-crafted-length allocations.
/// A legitimate chunk can never exceed 64 KiB + tag + a tiny buffer.
/// Consumed by the streaming decoder in Step 2 of this implementation
/// (plumbed via the rewritten `get_passthrough_stream`).
#[allow(dead_code)]
pub(crate) const CHUNK_MAX_WIRE_CIPHERTEXT: usize = CHUNK_PLAINTEXT_SIZE + GCM_TAG_LEN + 1024;

/// AES-256 encryption key (32 bytes). Zeroized on drop.
#[derive(Clone)]
pub struct EncryptionKey(pub(crate) [u8; 32]);

impl EncryptionKey {
    pub fn from_hex(hex_str: &str) -> Result<Self, String> {
        let bytes = hex::decode(hex_str).map_err(|e| format!("invalid hex key: {}", e))?;
        if bytes.len() != 32 {
            return Err(format!(
                "key must be 32 bytes (64 hex chars), got {}",
                bytes.len()
            ));
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        Ok(Self(key))
    }
}

impl Drop for EncryptionKey {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
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
}

/// Hot-reloadable encryption configuration.
///
/// `key` is the AES-256 master key used to encrypt/decrypt object
/// bodies on this backend. `key_id` is a stable, non-secret
/// identifier stamped on each written object as the
/// `dg-encryption-key-id` metadata field so read paths can detect
/// cross-backend key mismatch (see [`ENCRYPTION_KEY_ID_KEY`]).
///
/// `legacy_key` + `legacy_key_id` hold the PREVIOUS key after a mode
/// transition. When a read's `dg-encryption-key-id` matches the
/// legacy id, the wrapper decrypts with `legacy_key` instead of
/// `key`. This is the decrypt-only-shim affordance during
/// proxy-AES → native-SSE migrations: operators keep reading old
/// objects without having to rewrite them all up front.
#[derive(Default)]
pub struct EncryptionConfig {
    pub key: Option<EncryptionKey>,
    /// Stable id paired with `key`. Required when `key` is Some so
    /// reads can detect mismatch; the engine resolver derives it
    /// automatically from `SHA-256(backend_name || key)` when the
    /// YAML doesn't pin one explicitly.
    pub key_id: Option<String>,
    /// Write-path policy. `Encrypt` (default) follows the key:
    /// encrypt when Some, passthrough when None. `PassThrough`
    /// forces passthrough regardless of key presence — used by the
    /// decrypt-only shim.
    pub write_mode: WriteMode,
    /// Decrypt-only-shim: the PREVIOUS key after a mode transition.
    /// Consulted on reads when the object's stamped id doesn't match
    /// `key_id` but DOES match `legacy_key_id`.
    pub legacy_key: Option<EncryptionKey>,
    /// Id paired with `legacy_key`. Same shape rules as `key_id`.
    pub legacy_key_id: Option<String>,
}

/// Encrypt plaintext → `[12-byte IV] [ciphertext + 16-byte GCM tag]`.
pub fn encrypt(key: &EncryptionKey, plaintext: &[u8]) -> Result<Vec<u8>, StorageError> {
    let cipher = Aes256Gcm::new_from_slice(&key.0)
        .map_err(|e| StorageError::Encryption(format!("cipher init: {}", e)))?;
    let mut iv = [0u8; IV_LEN];
    rand::rngs::OsRng.fill_bytes(&mut iv);
    let nonce = Nonce::from_slice(&iv);
    let ct = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| StorageError::Encryption(format!("encrypt: {}", e)))?;
    let mut blob = Vec::with_capacity(IV_LEN + ct.len());
    blob.extend_from_slice(&iv);
    blob.extend_from_slice(&ct);
    Ok(blob)
}

/// Decrypt `[12-byte IV] [ciphertext + tag]` → plaintext.
pub fn decrypt(key: &EncryptionKey, blob: &[u8]) -> Result<Vec<u8>, StorageError> {
    if blob.len() < IV_LEN + 16 {
        return Err(StorageError::Encryption(format!(
            "blob too short: {} bytes",
            blob.len()
        )));
    }
    let cipher = Aes256Gcm::new_from_slice(&key.0)
        .map_err(|e| StorageError::Encryption(format!("cipher init: {}", e)))?;
    let nonce = Nonce::from_slice(&blob[..IV_LEN]);
    cipher.decrypt(nonce, &blob[IV_LEN..]).map_err(|_| {
        StorageError::Encryption("decryption failed (wrong key or tampered data)".into())
    })
}

pub fn is_encrypted(metadata: &FileMetadata) -> bool {
    metadata
        .user_metadata
        .get(ENCRYPTION_MARKER_KEY)
        .map(|v| v == ENCRYPTION_MARKER_VALUE || v == CHUNK_MARKER_VALUE)
        .unwrap_or(false)
}

/// True iff the object was written with the chunked (streaming) format.
pub fn is_chunked_encrypted(metadata: &FileMetadata) -> bool {
    metadata
        .user_metadata
        .get(ENCRYPTION_MARKER_KEY)
        .map(|v| v == CHUNK_MARKER_VALUE)
        .unwrap_or(false)
}

/// Stamp the single-shot-format marker on the write path. When
/// `key_id` is `Some`, also stamps `dg-encryption-key-id` so reads
/// can cross-check against the wrapper's configured key_id. Legacy
/// objects written without a key_id stay readable (the read check
/// is a two-sided conditional — both sides need an id to fire).
pub fn mark_encrypted(metadata: &mut FileMetadata, key_id: Option<&str>) {
    metadata.user_metadata.insert(
        ENCRYPTION_MARKER_KEY.to_string(),
        ENCRYPTION_MARKER_VALUE.to_string(),
    );
    if let Some(kid) = key_id {
        metadata
            .user_metadata
            .insert(ENCRYPTION_KEY_ID_KEY.to_string(), kid.to_string());
    }
}

/// Stamp the chunked-format marker on the write path. Same key_id
/// semantics as [`mark_encrypted`].
pub fn mark_chunked_encrypted(metadata: &mut FileMetadata, key_id: Option<&str>) {
    metadata.user_metadata.insert(
        ENCRYPTION_MARKER_KEY.to_string(),
        CHUNK_MARKER_VALUE.to_string(),
    );
    if let Some(kid) = key_id {
        metadata
            .user_metadata
            .insert(ENCRYPTION_KEY_ID_KEY.to_string(), kid.to_string());
    }
}

/// Read the object's stamped key_id, if any. Returns None for legacy
/// objects that pre-date the Step 3 stamp.
pub fn stamped_key_id(metadata: &FileMetadata) -> Option<&str> {
    metadata
        .user_metadata
        .get(ENCRYPTION_KEY_ID_KEY)
        .map(|s| s.as_str())
}

// ─────────────────────────────────────────────────────────────────────
// Chunked-format primitives
// ─────────────────────────────────────────────────────────────────────

/// Derive the per-chunk nonce: `base_iv XOR (chunk_index as big-endian u96)`.
///
/// We XOR rather than append/concatenate because `base_iv` is already 12 bytes
/// (the exact nonce size) and we need a deterministic, collision-free mapping
/// from `(base_iv, index)` to a 12-byte nonce. XOR gives 2^32 distinct nonces
/// per object, well past any passthrough we'd see.
fn chunk_nonce(base_iv: &[u8; IV_LEN], chunk_index: u32) -> [u8; IV_LEN] {
    let mut nonce = *base_iv;
    // Place the big-endian u32 at the LAST four bytes (positions 8..12),
    // leaving the high-order 8 bytes intact so two adjacent chunk_indices
    // produce nonces that differ in exactly the bits we chose.
    let idx_be = chunk_index.to_be_bytes();
    nonce[8] ^= idx_be[0];
    nonce[9] ^= idx_be[1];
    nonce[10] ^= idx_be[2];
    nonce[11] ^= idx_be[3];
    nonce
}

/// Build the AAD blob for a chunk: 16 bytes of
/// `"DGE1" || chunk_index_le_u32 || final_flag_u8 || 0x00 0x00 0x00`.
///
/// The AAD is authenticated (not encrypted). Binding the index prevents
/// reordering of chunks on disk; binding the final flag prevents truncation
/// (the new "last" chunk's AAD would mismatch what was signed at write time).
fn chunk_aad(chunk_index: u32, is_final: bool) -> [u8; 16] {
    let mut aad = [0u8; 16];
    aad[..4].copy_from_slice(&CHUNK_MAGIC);
    aad[4..8].copy_from_slice(&chunk_index.to_le_bytes());
    aad[8] = if is_final { 1 } else { 0 };
    // aad[9..16] = 0 (reserved for future use; must stay zero).
    aad
}

/// Encrypt a single plaintext chunk into a wire-format frame:
/// `[4 B length prefix (u32 LE)] [ciphertext + 16 B GCM tag]`.
///
/// The caller is responsible for chunking the plaintext into ≤64 KiB windows
/// and tracking the correct `chunk_index` / `is_final` across the stream.
pub fn encrypt_chunk(
    key: &EncryptionKey,
    base_iv: &[u8; IV_LEN],
    chunk_index: u32,
    is_final: bool,
    plaintext: &[u8],
) -> Result<Vec<u8>, StorageError> {
    if plaintext.len() > CHUNK_PLAINTEXT_SIZE {
        return Err(StorageError::Encryption(format!(
            "chunk plaintext too large: {} bytes (max {})",
            plaintext.len(),
            CHUNK_PLAINTEXT_SIZE
        )));
    }
    let cipher = Aes256Gcm::new_from_slice(&key.0)
        .map_err(|e| StorageError::Encryption(format!("cipher init: {}", e)))?;
    let nonce = chunk_nonce(base_iv, chunk_index);
    let aad = chunk_aad(chunk_index, is_final);
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|e| StorageError::Encryption(format!("encrypt chunk {}: {}", chunk_index, e)))?;
    let ct_len: u32 = ct.len().try_into().map_err(|_| {
        StorageError::Encryption("chunk ciphertext length overflows u32".to_string())
    })?;
    let mut frame = Vec::with_capacity(CHUNK_FRAME_LEN_FIELD + ct.len());
    frame.extend_from_slice(&ct_len.to_le_bytes());
    frame.extend_from_slice(&ct);
    Ok(frame)
}

/// Decrypt a chunk's ciphertext back to plaintext. Unlike `encrypt_chunk`,
/// this takes the raw ciphertext (without the length prefix) — the framing
/// is handled by `ChunkedDecryptStream`.
pub fn decrypt_chunk(
    key: &EncryptionKey,
    base_iv: &[u8; IV_LEN],
    chunk_index: u32,
    is_final: bool,
    ciphertext: &[u8],
) -> Result<Vec<u8>, StorageError> {
    if ciphertext.len() < GCM_TAG_LEN {
        return Err(StorageError::Encryption(format!(
            "chunk {} ciphertext too short: {} bytes",
            chunk_index,
            ciphertext.len()
        )));
    }
    let cipher = Aes256Gcm::new_from_slice(&key.0)
        .map_err(|e| StorageError::Encryption(format!("cipher init: {}", e)))?;
    let nonce = chunk_nonce(base_iv, chunk_index);
    let aad = chunk_aad(chunk_index, is_final);
    cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| {
            StorageError::Encryption(format!(
                "chunk {} decryption failed (wrong key, tampered, or reordered)",
                chunk_index
            ))
        })
}

/// O(1) helper for range reads: given a plaintext byte offset, return
/// `(chunk_index, offset_within_chunk)`. Works because every non-final
/// chunk is exactly `CHUNK_PLAINTEXT_SIZE` plaintext bytes.
pub fn chunk_index_for_plaintext_offset(pt_offset: u64) -> (u32, u32) {
    let chunk_sz = CHUNK_PLAINTEXT_SIZE as u64;
    let idx = (pt_offset / chunk_sz) as u32;
    let off = (pt_offset % chunk_sz) as u32;
    (idx, off)
}

/// O(1) helper for range reads: given a plaintext byte offset, return the
/// corresponding ciphertext byte offset in the on-disk wire stream. Assumes
/// we want to read starting at the CHUNK boundary that contains the target
/// offset (not mid-chunk — GCM can't decrypt a partial chunk).
pub fn wire_offset_of_chunk(chunk_index: u32) -> u64 {
    CHUNK_HEADER_LEN as u64 + (chunk_index as u64) * (CHUNK_FRAME_WIRE_LEN as u64)
}

// ─────────────────────────────────────────────────────────────────────
// Streaming decoder
// ─────────────────────────────────────────────────────────────────────

/// State machine for the chunked wire-format decoder.
///
/// Carried through `futures::stream::unfold` so we don't need a
/// manual `pin_project` dependency. See `chunked_decrypt_stream`
/// below for the public builder.
struct DecryptState<S>
where
    S: futures::Stream<Item = Result<Bytes, StorageError>> + Unpin,
{
    inner: S,
    key: EncryptionKey,
    // Rolling buffer of ciphertext bytes not yet consumed.
    buf: Vec<u8>,
    header_done: bool,
    base_iv: [u8; IV_LEN],
    // Zero-indexed count of frames we've already emitted.
    chunk_index: u32,
    // Hint: if the caller knows the total number of plaintext bytes
    // (from FileMetadata.file_size), we can derive which frame is
    // final. Required for correctness — the AAD binds is_final, so
    // the decoder MUST know it matches what the encoder stamped.
    expected_final_index: u32,
    // Set once we've successfully decrypted the is_final=true frame.
    emitted_final: bool,
    // Plaintext bytes to skip at the very start (range trim at head).
    skip_bytes: u64,
    // Plaintext bytes still to emit; None = emit until end.
    take_bytes: Option<u64>,
}

/// Produce a plaintext stream from an encrypted chunked-format
/// ciphertext stream — full-file path. The stream MUST begin with the
/// `[magic][base_iv]` header; use [`chunked_decrypt_stream_from_chunk`]
/// for the range-read path where the header was fetched separately.
///
/// `expected_final_index` MUST be the zero-based index of the final
/// chunk (derived from `ceil(plaintext_size / CHUNK_PLAINTEXT_SIZE) - 1`;
/// a zero-byte object has `expected_final_index = 0`). Required because
/// the AEAD AAD binds the final flag — the decoder needs to know which
/// frame to mark final on reconstruction, or GCM auth will reject.
///
/// `skip_bytes` and `take_bytes` trim the head/tail of the plaintext
/// for range reads.
fn chunked_decrypt_stream<S>(
    inner: S,
    key: EncryptionKey,
    expected_final_index: u32,
    skip_bytes: u64,
    take_bytes: Option<u64>,
) -> BoxStream<'static, Result<Bytes, StorageError>>
where
    S: futures::Stream<Item = Result<Bytes, StorageError>> + Unpin + Send + 'static,
{
    let state = DecryptState {
        inner,
        key,
        buf: Vec::with_capacity(CHUNK_FRAME_WIRE_LEN + 64),
        header_done: false,
        base_iv: [0u8; IV_LEN],
        chunk_index: 0,
        expected_final_index,
        emitted_final: false,
        skip_bytes,
        take_bytes,
    };

    Box::pin(futures::stream::unfold(state, |mut st| async move {
        use futures::StreamExt;
        loop {
            // Early termination by caller bound.
            if matches!(st.take_bytes, Some(0)) {
                return None;
            }

            // Phase 1: header ([magic][base_iv]).
            if !st.header_done {
                while st.buf.len() < CHUNK_HEADER_LEN {
                    match st.inner.next().await {
                        Some(Ok(more)) => st.buf.extend_from_slice(&more),
                        Some(Err(e)) => return Some((Err(e), st)),
                        None => {
                            return Some((
                                Err(StorageError::Encryption(
                                    "stream ended before encryption header".into(),
                                )),
                                st,
                            ));
                        }
                    }
                }
                if st.buf[..4] != CHUNK_MAGIC {
                    return Some((
                        Err(StorageError::Encryption(format!(
                            "bad chunked-encryption magic: {:02x?}",
                            &st.buf[..4]
                        ))),
                        st,
                    ));
                }
                st.base_iv.copy_from_slice(&st.buf[4..CHUNK_HEADER_LEN]);
                st.buf.drain(..CHUNK_HEADER_LEN);
                st.header_done = true;
            }

            // If we've already emitted the final chunk, we're done —
            // any trailing bytes from the inner stream are a framing
            // violation. We don't error (keeps the caller's stream
            // clean — the plaintext was already delivered intact) but
            // we do log at debug level so oddities on disk surface in
            // traces. A shipped file with trailing garbage likely
            // indicates a broken backup/restore path; operators who
            // care can grep the logs.
            if st.emitted_final {
                if !st.buf.is_empty() {
                    tracing::debug!(
                        "chunked-encryption decoder: {} trailing bytes after final frame — \
                         discarding (possible backup/restore mishandling)",
                        st.buf.len()
                    );
                }
                return None;
            }

            // Phase 2: frame [4 B len] [ct+tag].
            while st.buf.len() < CHUNK_FRAME_LEN_FIELD {
                match st.inner.next().await {
                    Some(Ok(more)) => st.buf.extend_from_slice(&more),
                    Some(Err(e)) => return Some((Err(e), st)),
                    None => {
                        // Upstream ended with empty buffer. That's a
                        // truncation: we haven't yet emitted the final
                        // frame.
                        return Some((
                            Err(StorageError::Encryption(format!(
                                "stream truncated before chunk {} (expected final index {})",
                                st.chunk_index, st.expected_final_index
                            ))),
                            st,
                        ));
                    }
                }
            }

            let declared =
                u32::from_le_bytes(st.buf[..CHUNK_FRAME_LEN_FIELD].try_into().unwrap()) as usize;
            if declared > CHUNK_MAX_WIRE_CIPHERTEXT {
                return Some((
                    Err(StorageError::Encryption(format!(
                        "frame length {} exceeds ceiling {} — rejecting (possible DOS)",
                        declared, CHUNK_MAX_WIRE_CIPHERTEXT,
                    ))),
                    st,
                ));
            }
            let frame_wire_len = CHUNK_FRAME_LEN_FIELD + declared;
            while st.buf.len() < frame_wire_len {
                match st.inner.next().await {
                    Some(Ok(more)) => st.buf.extend_from_slice(&more),
                    Some(Err(e)) => return Some((Err(e), st)),
                    None => {
                        return Some((
                            Err(StorageError::Encryption(
                                "stream truncated mid-frame-body".into(),
                            )),
                            st,
                        ));
                    }
                }
            }

            let is_final = st.chunk_index == st.expected_final_index;
            let ct = &st.buf[CHUNK_FRAME_LEN_FIELD..frame_wire_len];
            let pt = match decrypt_chunk(&st.key, &st.base_iv, st.chunk_index, is_final, ct) {
                Ok(p) => p,
                Err(e) => return Some((Err(e), st)),
            };
            st.buf.drain(..frame_wire_len);
            st.chunk_index = match st.chunk_index.checked_add(1) {
                Some(v) => v,
                None => {
                    return Some((
                        Err(StorageError::Encryption(
                            "chunk index overflow during decode".into(),
                        )),
                        st,
                    ));
                }
            };
            if is_final {
                st.emitted_final = true;
            }

            // Apply skip_bytes from the head of this frame's plaintext.
            let mut start = 0usize;
            if st.skip_bytes > 0 {
                let skip = std::cmp::min(st.skip_bytes as usize, pt.len());
                start += skip;
                st.skip_bytes -= skip as u64;
            }
            let remainder = &pt[start..];

            // Apply take_bytes ceiling.
            let to_emit: Bytes = if let Some(take) = st.take_bytes {
                let take_now = std::cmp::min(take as usize, remainder.len());
                let slice = Bytes::copy_from_slice(&remainder[..take_now]);
                st.take_bytes = Some(take - take_now as u64);
                slice
            } else {
                Bytes::copy_from_slice(remainder)
            };

            if to_emit.is_empty() {
                // Don't emit an empty Bytes — loop to next frame.
                continue;
            }
            return Some((Ok(to_emit), st));
        }
    }))
}

/// Range-read decoder builder. Differs from
/// [`chunked_decrypt_stream`] in that the caller has already fetched
/// the 16-byte header (magic + base_iv) via a separate small range
/// request and hands the parsed `base_iv` + starting `chunk_index` in
/// directly. The `inner` stream must start at the beginning of
/// `starting_chunk_index`'s frame (i.e. at `wire_offset_of_chunk`),
/// not at wire offset 0.
///
/// This is what makes "read last 100 bytes of a 10 GiB file" cost O(1)
/// network traffic instead of O(N): we fetch exactly the target chunks
/// plus the separate tiny header fetch, and the decoder starts with
/// its chunk_index aligned to the range.
fn chunked_decrypt_stream_from_chunk<S>(
    inner: S,
    key: EncryptionKey,
    base_iv: [u8; IV_LEN],
    starting_chunk_index: u32,
    expected_final_index: u32,
    skip_bytes: u64,
    take_bytes: Option<u64>,
) -> BoxStream<'static, Result<Bytes, StorageError>>
where
    S: futures::Stream<Item = Result<Bytes, StorageError>> + Unpin + Send + 'static,
{
    let state = DecryptState {
        inner,
        key,
        buf: Vec::with_capacity(CHUNK_FRAME_WIRE_LEN + 64),
        // Caller already consumed the header — skip phase 1 entirely.
        header_done: true,
        base_iv,
        chunk_index: starting_chunk_index,
        expected_final_index,
        emitted_final: false,
        skip_bytes,
        take_bytes,
    };

    Box::pin(futures::stream::unfold(state, |mut st| async move {
        use futures::StreamExt;
        loop {
            if matches!(st.take_bytes, Some(0)) {
                return None;
            }
            if st.emitted_final {
                return None;
            }

            while st.buf.len() < CHUNK_FRAME_LEN_FIELD {
                match st.inner.next().await {
                    Some(Ok(more)) => st.buf.extend_from_slice(&more),
                    Some(Err(e)) => return Some((Err(e), st)),
                    None => {
                        return Some((
                            Err(StorageError::Encryption(format!(
                                "stream truncated before chunk {} (expected final index {})",
                                st.chunk_index, st.expected_final_index
                            ))),
                            st,
                        ));
                    }
                }
            }

            let declared =
                u32::from_le_bytes(st.buf[..CHUNK_FRAME_LEN_FIELD].try_into().unwrap()) as usize;
            if declared > CHUNK_MAX_WIRE_CIPHERTEXT {
                return Some((
                    Err(StorageError::Encryption(format!(
                        "frame length {} exceeds ceiling {} — rejecting (possible DOS)",
                        declared, CHUNK_MAX_WIRE_CIPHERTEXT,
                    ))),
                    st,
                ));
            }
            let frame_wire_len = CHUNK_FRAME_LEN_FIELD + declared;
            while st.buf.len() < frame_wire_len {
                match st.inner.next().await {
                    Some(Ok(more)) => st.buf.extend_from_slice(&more),
                    Some(Err(e)) => return Some((Err(e), st)),
                    None => {
                        return Some((
                            Err(StorageError::Encryption(
                                "stream truncated mid-frame-body".into(),
                            )),
                            st,
                        ));
                    }
                }
            }

            let is_final = st.chunk_index == st.expected_final_index;
            let ct = &st.buf[CHUNK_FRAME_LEN_FIELD..frame_wire_len];
            let pt = match decrypt_chunk(&st.key, &st.base_iv, st.chunk_index, is_final, ct) {
                Ok(p) => p,
                Err(e) => return Some((Err(e), st)),
            };
            st.buf.drain(..frame_wire_len);
            st.chunk_index = match st.chunk_index.checked_add(1) {
                Some(v) => v,
                None => {
                    return Some((
                        Err(StorageError::Encryption(
                            "chunk index overflow during decode".into(),
                        )),
                        st,
                    ));
                }
            };
            if is_final {
                st.emitted_final = true;
            }

            let mut start = 0usize;
            if st.skip_bytes > 0 {
                let skip = std::cmp::min(st.skip_bytes as usize, pt.len());
                start += skip;
                st.skip_bytes -= skip as u64;
            }
            let remainder = &pt[start..];

            let to_emit: Bytes = if let Some(take) = st.take_bytes {
                let take_now = std::cmp::min(take as usize, remainder.len());
                let slice = Bytes::copy_from_slice(&remainder[..take_now]);
                st.take_bytes = Some(take - take_now as u64);
                slice
            } else {
                Bytes::copy_from_slice(remainder)
            };

            if to_emit.is_empty() {
                continue;
            }
            return Some((Ok(to_emit), st));
        }
    }))
}

/// Compute the index of the final chunk given a plaintext byte
/// count. Zero-byte objects still have one chunk (index 0 with empty
/// plaintext) — the write path guarantees this.
fn final_chunk_index_for_plaintext_size(plaintext_size: u64) -> u32 {
    if plaintext_size == 0 {
        return 0;
    }
    let sz = CHUNK_PLAINTEXT_SIZE as u64;
    let last = (plaintext_size - 1) / sz;
    last as u32
}

/// Wrap an unencrypted-passthrough stream so the first 4 bytes are
/// inspected for the chunked-encryption `DGE1` magic. If present, we
/// emit a hard error instead of serving ciphertext. Guards against the
/// operational failure mode where a backup/restore round-trip strips
/// xattrs — the body on disk is still ciphertext, the metadata no
/// longer carries the encryption marker, and without this check the
/// wrapper would happily serve ciphertext as plaintext.
///
/// Cost: reads the first emitted `Bytes` of the stream (whatever size
/// that is — usually ≥4 KiB), inspects up to 4 bytes, then re-emits
/// the original Bytes unchanged. Zero extra network/disk round-trips.
fn sniff_dge1_magic<S>(inner: S) -> BoxStream<'static, Result<Bytes, StorageError>>
where
    S: futures::Stream<Item = Result<Bytes, StorageError>> + Unpin + Send + 'static,
{
    enum State<S> {
        Initial(S),
        Passthrough(S),
        Done,
    }
    Box::pin(futures::stream::unfold(
        State::Initial(inner),
        |st| async move {
            use futures::StreamExt;
            match st {
                State::Initial(mut inner) => match inner.next().await {
                    Some(Ok(first)) => {
                        if first.len() >= 4 && first[..4] == CHUNK_MAGIC {
                            let err = Err(StorageError::Encryption(
                                "object body begins with chunked-encryption magic but \
                                 metadata has no dg-encrypted marker — xattrs may have \
                                 been stripped during backup/restore. Refusing to serve \
                                 ciphertext as plaintext."
                                    .into(),
                            ));
                            return Some((err, State::Done));
                        }
                        Some((Ok(first), State::Passthrough(inner)))
                    }
                    Some(Err(e)) => Some((Err(e), State::Done)),
                    None => None,
                },
                State::Passthrough(mut inner) => inner
                    .next()
                    .await
                    .map(|item| (item, State::Passthrough(inner))),
                State::Done => None,
            }
        },
    ))
}

/// Fetch the 16-byte `[magic][base_iv]` header via a short range request
/// and return the parsed `base_iv`. Errors on bad magic or truncation
/// — the caller should propagate those unchanged.
///
/// Small enough that the overhead of an extra backend call is
/// negligible vs. the gain of bounded-cost range reads on large
/// objects. Used only by the range-read path; the full-file stream
/// decoder parses the header from its own byte stream in phase 1.
async fn fetch_chunked_header<B: StorageBackend + ?Sized>(
    inner: &B,
    bucket: &str,
    prefix: &str,
    filename: &str,
) -> Result<[u8; IV_LEN], StorageError> {
    use futures::StreamExt;
    let (mut stream, _) = inner
        .get_passthrough_stream_range(bucket, prefix, filename, 0, CHUNK_HEADER_LEN as u64 - 1)
        .await?;
    let mut buf: Vec<u8> = Vec::with_capacity(CHUNK_HEADER_LEN);
    while buf.len() < CHUNK_HEADER_LEN {
        match stream.next().await {
            Some(Ok(b)) => buf.extend_from_slice(&b),
            Some(Err(e)) => return Err(e),
            None => {
                return Err(StorageError::Encryption(format!(
                    "stream ended before encryption header (got {} of {} bytes)",
                    buf.len(),
                    CHUNK_HEADER_LEN
                )));
            }
        }
    }
    if buf[..4] != CHUNK_MAGIC {
        return Err(StorageError::Encryption(format!(
            "bad chunked-encryption magic: {:02x?}",
            &buf[..4]
        )));
    }
    let mut base_iv = [0u8; IV_LEN];
    base_iv.copy_from_slice(&buf[4..CHUNK_HEADER_LEN]);
    Ok(base_iv)
}

/// Transparent encryption wrapper around any `StorageBackend`.
pub struct EncryptingBackend<B: StorageBackend> {
    inner: B,
    config: Arc<ArcSwap<EncryptionConfig>>,
}

impl<B: StorageBackend> EncryptingBackend<B> {
    pub fn new(inner: B, config: Arc<ArcSwap<EncryptionConfig>>) -> Self {
        Self { inner, config }
    }

    fn current_key(&self) -> Option<EncryptionKey> {
        self.config.load().key.clone()
    }

    /// Snapshot the wrapper's configured key_id. Called on writes
    /// (to stamp) and reads (to compare). Captured before the encrypt/
    /// decrypt call so a concurrent hot-reload flip between key-check
    /// and AEAD-op doesn't produce spurious "mismatch" errors.
    fn current_key_id(&self) -> Option<String> {
        self.config.load().key_id.clone()
    }

    fn current_write_mode(&self) -> WriteMode {
        self.config.load().write_mode
    }

    fn encrypt_if_enabled(
        &self,
        data: &[u8],
        metadata: &mut FileMetadata,
    ) -> Result<Vec<u8>, StorageError> {
        // WriteMode::PassThrough short-circuits encryption even when
        // a `key` is present — the decrypt-only-shim case for
        // proxy-AES → native-SSE transitions. The inner S3Backend is
        // already doing native encryption at its layer.
        if self.current_write_mode() == WriteMode::PassThrough {
            return Ok(data.to_vec());
        }
        if let Some(key) = self.current_key() {
            let encrypted = encrypt(&key, data)?;
            mark_encrypted(metadata, self.current_key_id().as_deref());
            Ok(encrypted)
        } else {
            Ok(data.to_vec())
        }
    }

    /// Pick the (key, key_id) pair that should decrypt this object.
    /// Returns `(key, Some(key_id))` for the matching current or
    /// legacy key, or the current key with no-id-match for legacy
    /// (pre-Step-3) objects. Returns an error on mismatch AND no
    /// legacy fallback — same semantics as `check_key_id_match`
    /// plus the shim overlay.
    fn pick_decrypt_key(&self, object_kid: Option<&str>) -> Result<EncryptionKey, StorageError> {
        // Snapshot under a single ArcSwap load so a concurrent
        // hot-reload can't split the decision.
        let cfg = self.config.load();
        let primary_key = cfg.key.clone();
        let primary_kid = cfg.key_id.clone();
        let legacy = match (cfg.legacy_key.clone(), cfg.legacy_key_id.clone()) {
            (Some(k), Some(kid)) => Some((k, kid)),
            _ => None,
        };
        drop(cfg); // release the guard early

        match object_kid {
            Some(obj_id) => {
                // Prefer primary when ids match.
                if let Some(pid) = primary_kid.as_deref() {
                    if pid == obj_id {
                        return primary_key.ok_or_else(|| {
                            StorageError::Encryption(
                                "object is encrypted but no key is configured".into(),
                            )
                        });
                    }
                }
                // Fall back to legacy if present and matching.
                if let Some((lk, lid)) = legacy {
                    if lid == obj_id {
                        return Ok(lk);
                    }
                }
                // Neither primary nor legacy matches — the operator
                // either rotated without the shim or routed the
                // bucket to the wrong backend. Emit the specific
                // error that cites the object's id AND both
                // candidate ids the wrapper knows about.
                let cfg_id = primary_kid.as_deref().unwrap_or("<unset>");
                Err(StorageError::Encryption(format!(
                    "object was encrypted with key id '{obj_id}', but this backend is \
                     configured with key id '{cfg_id}' (no legacy-shim match either). This \
                     usually means: (a) the key was rotated without `legacy_key` set — \
                     restore the old key alongside the new one to read historical objects; \
                     (b) this bucket is routed to the wrong backend; (c) two backends \
                     share physical storage with different keys. Refusing to run AEAD — \
                     the underlying auth failure would be opaque."
                )))
            }
            None => {
                // Legacy object (no stamp). Primary key wins. If
                // primary has no key, we return the same error as
                // pre-shim behaviour — the caller surfaces "no key
                // configured" when needed.
                primary_key.ok_or_else(|| {
                    StorageError::Encryption("object is encrypted but no key is configured".into())
                })
            }
        }
    }

    fn decrypt_if_needed(
        &self,
        data: Vec<u8>,
        metadata: &FileMetadata,
    ) -> Result<Vec<u8>, StorageError> {
        if is_encrypted(metadata) {
            let key = self.pick_decrypt_key(stamped_key_id(metadata))?;
            decrypt(&key, &data)
        } else {
            Ok(data)
        }
    }
}

/// Short-circuit check: if the object carries a stamped `dg-encryption-
/// key-id` AND the wrapper is configured with a `key_id`, they must
/// match. Returns a SPECIFIC error on mismatch — the AEAD auth failure
/// that would otherwise surface gives an opaque "decryption failed"
/// message that doesn't tell operators whether they rotated the key,
/// routed a bucket to the wrong backend, or accidentally pointed two
/// backends at the same physical bucket with different keys.
///
/// Returns `Ok(())` in three legal cases:
///   * both ids present and equal (happy path).
///   * object has no id (legacy / pre-Step-3 object).
///   * wrapper has no id (mode:none wrapper reading an encrypted
///     object; the outer `no key configured` error still fires).
pub fn check_key_id_match(
    object_kid: Option<&str>,
    configured_kid: Option<&str>,
) -> Result<(), StorageError> {
    match (object_kid, configured_kid) {
        (Some(obj), Some(cfg)) if obj != cfg => Err(StorageError::Encryption(format!(
            "object was encrypted with key id '{obj}', but this backend is configured \
             with key id '{cfg}'. This usually means: (a) the key was rotated (unsupported \
             in this release — restore the old key alongside the new one to read historical \
             objects); (b) this bucket is routed to the wrong backend; (c) two backends \
             share physical storage with different keys. Refusing to run AEAD — the \
             underlying auth failure would be opaque."
        ))),
        _ => Ok(()),
    }
}

// Generate the full StorageBackend impl. Encrypt/decrypt methods are hand-written;
// all other methods delegate to self.inner unchanged.
#[async_trait]
impl<B: StorageBackend + Send + Sync> StorageBackend for EncryptingBackend<B> {
    // ── Encrypt on write ──

    async fn put_reference(
        &self,
        bucket: &str,
        prefix: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let mut meta = metadata.clone();
        let enc = self.encrypt_if_enabled(data, &mut meta)?;
        self.inner.put_reference(bucket, prefix, &enc, &meta).await
    }

    async fn put_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let mut meta = metadata.clone();
        let enc = self.encrypt_if_enabled(data, &mut meta)?;
        self.inner
            .put_delta(bucket, prefix, filename, &enc, &meta)
            .await
    }

    async fn put_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let mut meta = metadata.clone();
        let enc = self.encrypt_if_enabled(data, &mut meta)?;
        self.inner
            .put_passthrough(bucket, prefix, filename, &enc, &meta)
            .await
    }

    // put_passthrough_chunked: re-slices incoming chunks into 64 KiB
    // plaintext windows, encrypts each into a framed ciphertext chunk,
    // and forwards a new `Vec<Bytes>` (header + all frames) to the
    // inner backend's chunked PUT. No whole-object buffer in memory —
    // the peak allocation is one 64 KiB plaintext window + one frame
    // (~130 KiB) at a time.
    //
    // When encryption is off, delegates to inner's chunked impl
    // directly — no copying.
    async fn put_passthrough_chunked(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        chunks: &[Bytes],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let Some(key) = self.current_key() else {
            return self
                .inner
                .put_passthrough_chunked(bucket, prefix, filename, chunks, metadata)
                .await;
        };

        // Random per-object base IV. Each chunk's nonce is derived from
        // this + the chunk index.
        let mut base_iv = [0u8; IV_LEN];
        rand::rngs::OsRng.fill_bytes(&mut base_iv);

        // Emit the wire-format header first: [magic][base_iv].
        let mut header = Vec::with_capacity(CHUNK_HEADER_LEN);
        header.extend_from_slice(&CHUNK_MAGIC);
        header.extend_from_slice(&base_iv);
        let mut out_frames: Vec<Bytes> = Vec::with_capacity(chunks.len() + 4);
        out_frames.push(Bytes::from(header));

        // Re-slice incoming chunks into exactly CHUNK_PLAINTEXT_SIZE
        // windows. Multipart uploads typically arrive in 5 MiB (or
        // bigger) chunks, so one incoming Bytes gets split into ~80
        // plaintext windows. Small final remainder is sent as the
        // last chunk with is_final=true.
        let mut pt_window: Vec<u8> = Vec::with_capacity(CHUNK_PLAINTEXT_SIZE);
        let mut chunk_index: u32 = 0;

        // Two-phase iteration: collect full windows, then flush the
        // tail as the final chunk. We need to know when we're on the
        // LAST non-empty window to stamp is_final=true correctly; so
        // we accumulate all full windows first, then emit them with
        // is_final=false if any tail remains, else the last one gets
        // is_final=true.
        let mut pending_frames: Vec<Vec<u8>> = Vec::new();

        for incoming in chunks {
            let mut remaining: &[u8] = incoming.as_ref();
            while !remaining.is_empty() {
                let space = CHUNK_PLAINTEXT_SIZE - pt_window.len();
                let take = std::cmp::min(space, remaining.len());
                pt_window.extend_from_slice(&remaining[..take]);
                remaining = &remaining[take..];
                if pt_window.len() == CHUNK_PLAINTEXT_SIZE {
                    // Emit this window; don't know yet if it's final.
                    pending_frames.push(pt_window.clone());
                    pt_window.clear();
                    // Soft cap: if we've buffered many pending frames
                    // that we know for sure aren't final, flush them
                    // (the earlier chunk can't be final). This keeps
                    // pending_frames memory bounded at ~1 frame (~65K)
                    // instead of growing with object size.
                    if pending_frames.len() > 1 {
                        let frame_idx = chunk_index;
                        let pt = pending_frames.remove(0);
                        let frame = encrypt_chunk(&key, &base_iv, frame_idx, false, &pt)?;
                        out_frames.push(Bytes::from(frame));
                        chunk_index = chunk_index.checked_add(1).ok_or_else(|| {
                            StorageError::Encryption(
                                "chunk index overflow (> 2^32 chunks — object too large)".into(),
                            )
                        })?;
                    }
                }
            }
        }

        // End of input. `pending_frames` holds 0 or 1 full 64-KiB
        // window; `pt_window` holds 0..CHUNK_PLAINTEXT_SIZE bytes of
        // tail.
        //
        // Cases:
        //   (a) Both empty and chunk_index == 0: object is zero-bytes.
        //       Emit one frame with empty plaintext, is_final=true.
        //   (b) Both empty and chunk_index > 0: the last emitted frame
        //       was the true tail but we stamped it is_final=false
        //       (the 2-frame pipeline stamps only after confirming a
        //       follower exists). Fix by: we always keep at least one
        //       frame queued; the invariant is that `pending_frames`
        //       has the true final frame when input ends, plus maybe
        //       a non-empty `pt_window`.
        //   (c) pending_frames has 1 frame and pt_window is empty: the
        //       pending frame IS the final frame (full 64 KiB).
        //   (d) pending_frames has 1 frame and pt_window is non-empty:
        //       the pending frame is non-final, pt_window is final.
        //   (e) pending_frames is empty and pt_window is non-empty:
        //       pt_window is the ONLY and final frame (object smaller
        //       than 64 KiB).

        if pending_frames.is_empty() && pt_window.is_empty() && chunk_index == 0 {
            // Zero-byte object (case a).
            let frame = encrypt_chunk(&key, &base_iv, 0, true, &[])?;
            out_frames.push(Bytes::from(frame));
        } else if pending_frames.len() == 1 && pt_window.is_empty() {
            // Case (c): the queued frame is final.
            let pt = pending_frames.remove(0);
            let frame = encrypt_chunk(&key, &base_iv, chunk_index, true, &pt)?;
            out_frames.push(Bytes::from(frame));
        } else if pending_frames.len() == 1 && !pt_window.is_empty() {
            // Case (d): queued frame is non-final, pt_window is final.
            let pt = pending_frames.remove(0);
            let frame = encrypt_chunk(&key, &base_iv, chunk_index, false, &pt)?;
            out_frames.push(Bytes::from(frame));
            chunk_index = chunk_index.checked_add(1).ok_or_else(|| {
                StorageError::Encryption(
                    "chunk index overflow (> 2^32 chunks — object too large)".into(),
                )
            })?;
            let tail = encrypt_chunk(&key, &base_iv, chunk_index, true, &pt_window)?;
            out_frames.push(Bytes::from(tail));
        } else if pending_frames.is_empty() && !pt_window.is_empty() {
            // Case (e): sub-64KiB object.
            let frame = encrypt_chunk(&key, &base_iv, chunk_index, true, &pt_window)?;
            out_frames.push(Bytes::from(frame));
        } else {
            // Case (b): unreachable given the drain-on-2-frames
            // invariant above. If we ever hit it, the safe play is
            // to fail loudly rather than produce a stream without a
            // final-flag-set chunk (which would fail decrypt).
            return Err(StorageError::Encryption(
                "internal: chunking invariant violated (no final frame)".into(),
            ));
        }

        let mut meta = metadata.clone();
        mark_chunked_encrypted(&mut meta, self.current_key_id().as_deref());
        self.inner
            .put_passthrough_chunked(bucket, prefix, filename, &out_frames, &meta)
            .await
    }

    // ── Decrypt on read ──

    async fn get_reference(&self, bucket: &str, prefix: &str) -> Result<Vec<u8>, StorageError> {
        let data = self.inner.get_reference(bucket, prefix).await?;
        let meta = self.inner.get_reference_metadata(bucket, prefix).await?;
        self.decrypt_if_needed(data, &meta)
    }

    async fn get_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
        let data = self.inner.get_delta(bucket, prefix, filename).await?;
        let meta = self
            .inner
            .get_delta_metadata(bucket, prefix, filename)
            .await?;
        self.decrypt_if_needed(data, &meta)
    }

    async fn get_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
        let meta = self
            .inner
            .get_passthrough_metadata(bucket, prefix, filename)
            .await?;

        // Chunked path: decrypt_if_needed's single-shot decrypt would
        // treat the first 12 bytes of the `DGE1`-prefixed wire format
        // as an IV and AEAD-reject the rest — a silent caller footgun.
        // Instead, fetch the stream and run it through the chunked
        // decoder, collecting into a contiguous Vec<u8>. This matches
        // the semantics every `get_passthrough` caller expects (one
        // blob, plaintext) without requiring them to know about the
        // two wire formats.
        //
        // Every current caller of `get_passthrough` is a unit/test
        // helper or a fallback path that won't see chunked objects in
        // production — but fixing this closes the footgun before any
        // future caller trips over it.
        if is_chunked_encrypted(&meta) {
            let stream = self
                .get_passthrough_stream(bucket, prefix, filename)
                .await?;
            use futures::TryStreamExt;
            let parts: Vec<Bytes> = stream.try_collect().await?;
            let mut buf = Vec::with_capacity(parts.iter().map(|b| b.len()).sum());
            for p in parts {
                buf.extend_from_slice(&p);
            }
            return Ok(buf);
        }

        let data = self.inner.get_passthrough(bucket, prefix, filename).await?;
        self.decrypt_if_needed(data, &meta)
    }

    async fn get_passthrough_stream(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError> {
        let meta = self
            .inner
            .get_passthrough_metadata(bucket, prefix, filename)
            .await?;

        // Chunked path: stream end-to-end, decrypt frame-by-frame, no
        // whole-object buffer. This is the whole point of the chunked
        // format — a 5 GiB download stays at ~130 KiB peak memory in
        // the decoder.
        if is_chunked_encrypted(&meta) {
            // Shim-aware key selection: primary first, legacy if set
            // and the object's stamped id matches it. Emits the
            // specific "rotated without legacy_key" error on full
            // mismatch so the operator knows what the fix is.
            let key = self.pick_decrypt_key(stamped_key_id(&meta))?;
            let ct_stream = self
                .inner
                .get_passthrough_stream(bucket, prefix, filename)
                .await?;
            let final_idx = final_chunk_index_for_plaintext_size(meta.file_size);
            return Ok(chunked_decrypt_stream(ct_stream, key, final_idx, 0, None));
        }

        // v1 single-shot path (bounded by max_object_size). Buffer the
        // encrypted blob into memory, decrypt whole, wrap as a
        // single-emission stream. Same as before — unchanged.
        if is_encrypted(&meta) {
            let data = self.inner.get_passthrough(bucket, prefix, filename).await?;
            let plain = self.decrypt_if_needed(data, &meta)?;
            return Ok(Box::pin(futures::stream::once(async {
                Ok(Bytes::from(plain))
            })));
        }

        // Not encrypted per metadata — stream straight through, but
        // peek the first 4 bytes of the body as a belt-and-suspenders
        // check against the "xattr got stripped during backup/restore
        // and the on-disk body is still ciphertext" scenario. If we
        // see the chunked-format `DGE1` magic on an object that
        // metadata claims is plaintext, refuse rather than serving
        // ciphertext to the client. The odds of plaintext happening
        // to start with those 4 bytes are 1/2^32 and in practice zero
        // for any realistic file type.
        let stream = self
            .inner
            .get_passthrough_stream(bucket, prefix, filename)
            .await?;
        Ok(Box::pin(sniff_dge1_magic(stream)))
    }

    async fn get_passthrough_stream_range(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        start: u64,
        end: u64,
    ) -> Result<(BoxStream<'static, Result<Bytes, StorageError>>, u64), StorageError> {
        let meta = self
            .inner
            .get_passthrough_metadata(bucket, prefix, filename)
            .await?;

        // Chunked path: fetch only the wire bytes covering the target
        // chunks, using O(1) offset math — every non-final chunk is
        // exactly `CHUNK_FRAME_WIRE_LEN` bytes. We issue TWO backend
        // reads:
        //
        //   1. Header fetch: wire bytes `[0, CHUNK_HEADER_LEN)` — 16
        //      bytes for the magic + base_iv. Tiny, always needed.
        //   2. Body fetch: wire bytes from `wire_offset_of_chunk(
        //      first_chunk)` through the end of `last_chunk`. For
        //      non-final `last_chunk` this is a bounded window; for
        //      `last_chunk == final_idx` we ask for EOF (the final
        //      chunk may be shorter than a full frame).
        //
        // The alternative — fetching from wire-offset 0 through
        // last_chunk and relying on the decoder to discard the
        // leading chunks — is O(N) for reads near the end of large
        // objects (before this fix: "last 100 bytes of a 10 GiB file"
        // pulled all 10 GiB). The two-fetch approach trades one extra
        // tiny request for bounded cost on every range shape.
        if is_chunked_encrypted(&meta) {
            // Shim-aware key selection; same behaviour as the full-
            // stream path. Surfaces the specific error before any
            // backend range reads fire.
            let key = self.pick_decrypt_key(stamped_key_id(&meta))?;
            let final_idx = final_chunk_index_for_plaintext_size(meta.file_size);
            // Clamp `end` (inclusive) to the actual plaintext size.
            let effective_end = std::cmp::min(end, meta.file_size.saturating_sub(1));
            if effective_end < start {
                // Out-of-range for this object. HTTP handlers clamp
                // via resolve_range before reaching us (returning 416
                // upstream), so this is unreachable in the serving
                // path today. Future callers that reach here get a
                // hard error rather than the pre-B3 `(empty, 0)`
                // signal, which overlapped with the default-impl's
                // "full stream, not range" contract and mis-routed
                // callers into buffered fallbacks.
                return Err(StorageError::Other(format!(
                    "range out of bounds: start={} effective_end={} file_size={}",
                    start, effective_end, meta.file_size
                )));
            }
            let (first_chunk, _) = chunk_index_for_plaintext_offset(start);
            let (last_chunk, _) = chunk_index_for_plaintext_offset(effective_end);

            // Fetch #1: just the header. Size is tiny
            // (CHUNK_HEADER_LEN = 16 bytes).
            let base_iv = fetch_chunked_header(&self.inner, bucket, prefix, filename).await?;

            // Fetch #2: body covering `first_chunk..=last_chunk`.
            let wire_start = wire_offset_of_chunk(first_chunk);
            let wire_end = if last_chunk < final_idx {
                wire_offset_of_chunk(last_chunk) + CHUNK_FRAME_WIRE_LEN as u64 - 1
            } else {
                // Last chunk IS the object's final chunk; it may be
                // shorter than a full frame, so ask for EOF. The
                // `u64::MAX - 1` sentinel works across both backends:
                // S3 interprets it per RFC 7233 (clamp to resource
                // length), filesystem `File::take` limits on actual
                // EOF.
                u64::MAX - 1
            };
            let (ct_stream, _) = self
                .inner
                .get_passthrough_stream_range(bucket, prefix, filename, wire_start, wire_end)
                .await?;

            // Skip any plaintext bytes before `start` within the
            // first fetched chunk. E.g. for start=70000 and chunk
            // size 65536, first_chunk=1 (starts at plaintext 65536)
            // and we skip 70000 - 65536 = 4464 bytes of its
            // plaintext. The preceding full chunks (index 0) are
            // never fetched or decrypted.
            let skip_bytes = start - (first_chunk as u64) * (CHUNK_PLAINTEXT_SIZE as u64);
            let plaintext_len = effective_end - start + 1;

            let plain = chunked_decrypt_stream_from_chunk(
                ct_stream,
                key,
                base_iv,
                first_chunk,
                final_idx,
                skip_bytes,
                Some(plaintext_len),
            );
            return Ok((plain, plaintext_len));
        }

        // v1 single-shot path (bounded by max_object_size). Same as
        // before — buffer-and-slice.
        if is_encrypted(&meta) {
            let data = self.inner.get_passthrough(bucket, prefix, filename).await?;
            let plain = self.decrypt_if_needed(data, &meta)?;
            let s = start as usize;
            let e = std::cmp::min(end as usize + 1, plain.len());
            let slice = Bytes::from(plain[s..e].to_vec());
            let len = slice.len() as u64;
            return Ok((Box::pin(futures::stream::once(async { Ok(slice) })), len));
        }

        // Not encrypted — delegate.
        self.inner
            .get_passthrough_stream_range(bucket, prefix, filename, start, end)
            .await
    }

    // ── Pass-through (no encryption) ──

    async fn create_bucket(&self, b: &str) -> Result<(), StorageError> {
        self.inner.create_bucket(b).await
    }
    async fn delete_bucket(&self, b: &str) -> Result<(), StorageError> {
        self.inner.delete_bucket(b).await
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
    async fn has_reference(&self, b: &str, p: &str) -> bool {
        self.inner.has_reference(b, p).await
    }
    async fn get_reference_metadata(&self, b: &str, p: &str) -> Result<FileMetadata, StorageError> {
        self.inner.get_reference_metadata(b, p).await
    }
    async fn get_delta_metadata(
        &self,
        b: &str,
        p: &str,
        f: &str,
    ) -> Result<FileMetadata, StorageError> {
        self.inner.get_delta_metadata(b, p, f).await
    }
    async fn get_passthrough_metadata(
        &self,
        b: &str,
        p: &str,
        f: &str,
    ) -> Result<FileMetadata, StorageError> {
        self.inner.get_passthrough_metadata(b, p, f).await
    }
    async fn put_reference_metadata(
        &self,
        b: &str,
        p: &str,
        m: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.inner.put_reference_metadata(b, p, m).await
    }
    async fn delete_reference(&self, b: &str, p: &str) -> Result<(), StorageError> {
        self.inner.delete_reference(b, p).await
    }
    async fn delete_delta(&self, b: &str, p: &str, f: &str) -> Result<(), StorageError> {
        self.inner.delete_delta(b, p, f).await
    }
    async fn delete_passthrough(&self, b: &str, p: &str, f: &str) -> Result<(), StorageError> {
        self.inner.delete_passthrough(b, p, f).await
    }
    async fn scan_deltaspace(&self, b: &str, p: &str) -> Result<Vec<FileMetadata>, StorageError> {
        self.inner.scan_deltaspace(b, p).await
    }
    async fn list_deltaspaces(&self, b: &str) -> Result<Vec<String>, StorageError> {
        self.inner.list_deltaspaces(b).await
    }
    async fn total_size(&self, b: Option<&str>) -> Result<u64, StorageError> {
        self.inner.total_size(b).await
    }
    async fn put_directory_marker(&self, b: &str, k: &str) -> Result<(), StorageError> {
        self.inner.put_directory_marker(b, k).await
    }
    async fn bulk_list_objects(
        &self,
        b: &str,
        p: &str,
    ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
        self.inner.bulk_list_objects(b, p).await
    }
    async fn enrich_list_metadata(
        &self,
        b: &str,
        o: Vec<(String, FileMetadata)>,
    ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
        self.inner.enrich_list_metadata(b, o).await
    }
    async fn list_objects_delegated(
        &self,
        b: &str,
        p: &str,
        d: &str,
        m: u32,
        t: Option<&str>,
    ) -> Result<Option<DelegatedListResult>, StorageError> {
        self.inner.list_objects_delegated(b, p, d, m, t).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> EncryptionKey {
        EncryptionKey::from_hex("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
            .unwrap()
    }

    fn other_key() -> EncryptionKey {
        EncryptionKey::from_hex("fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210")
            .unwrap()
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = test_key();
        let pt = b"hello, encryption at rest!";
        let blob = encrypt(&key, pt).unwrap();
        assert_eq!(decrypt(&key, &blob).unwrap(), pt);
    }

    #[test]
    fn test_unique_ivs() {
        let key = test_key();
        let pt = b"same data";
        let b1 = encrypt(&key, pt).unwrap();
        let b2 = encrypt(&key, pt).unwrap();
        assert_ne!(b1, b2);
        assert_eq!(decrypt(&key, &b1).unwrap(), pt);
        assert_eq!(decrypt(&key, &b2).unwrap(), pt);
    }

    #[test]
    fn test_wrong_key_error() {
        let blob = encrypt(&test_key(), b"secret").unwrap();
        let r = decrypt(&other_key(), &blob);
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("decryption failed"));
    }

    #[test]
    fn test_tampered_ciphertext() {
        let key = test_key();
        let mut blob = encrypt(&key, b"important").unwrap();
        blob[IV_LEN + 5] ^= 0xFF;
        assert!(decrypt(&key, &blob).is_err());
    }

    #[test]
    fn test_empty_data() {
        let key = test_key();
        let blob = encrypt(&key, b"").unwrap();
        assert_eq!(blob.len(), IV_LEN + 16);
        assert!(decrypt(&key, &blob).unwrap().is_empty());
    }

    #[test]
    fn test_large_data() {
        let key = test_key();
        let pt: Vec<u8> = (0..10_000_000u32).map(|i| (i % 256) as u8).collect();
        let blob = encrypt(&key, &pt).unwrap();
        assert_eq!(decrypt(&key, &blob).unwrap(), pt);
    }

    #[test]
    fn test_metadata_detection() {
        let mut m = FileMetadata::fallback(
            "test".into(),
            100,
            "md5".into(),
            chrono::Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        assert!(!is_encrypted(&m));
        mark_encrypted(&mut m, None);
        assert!(is_encrypted(&m));
    }

    #[test]
    fn test_key_validation() {
        assert!(EncryptionKey::from_hex("0123").is_err());
        assert!(EncryptionKey::from_hex("zzzz").is_err());
        assert!(EncryptionKey::from_hex(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        )
        .is_ok());
    }

    #[test]
    fn test_blob_too_short() {
        let r = decrypt(&test_key(), &[0u8; 10]);
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("too short"));
    }

    // ─────────────────────────────────────────────────────────────────
    // Chunked-format codec tests
    //
    // These cover the AEAD primitives in isolation; integration tests in
    // `tests/encryption_test.rs` exercise the streaming trait impl
    // (chunking on upload, decoding on range GET, etc.).
    // ─────────────────────────────────────────────────────────────────

    fn test_base_iv() -> [u8; IV_LEN] {
        // Fixed value for deterministic tests — real callers generate with OsRng.
        [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]
    }

    #[test]
    fn test_chunk_nonce_is_derived_deterministically() {
        let iv = test_base_iv();
        let n0 = chunk_nonce(&iv, 0);
        // chunk_index=0 XORs zeros into the low 4 bytes — nonce equals base_iv.
        assert_eq!(n0, iv, "chunk 0 nonce must equal base_iv");

        let n1 = chunk_nonce(&iv, 1);
        assert_ne!(n1, iv, "chunk 1 nonce differs from base_iv");
        assert_eq!(n1[8], iv[8]);
        assert_eq!(n1[9], iv[9]);
        assert_eq!(n1[10], iv[10]);
        assert_eq!(n1[11], iv[11] ^ 0x01);
    }

    #[test]
    fn test_chunk_nonces_unique_across_sequential_indices() {
        // A real stream might have millions of chunks; we sanity-check a
        // small range and confirm each maps to a distinct nonce.
        let iv = test_base_iv();
        let mut seen = std::collections::HashSet::new();
        for i in 0u32..10_000 {
            let n = chunk_nonce(&iv, i);
            assert!(seen.insert(n), "duplicate nonce at index {i}");
        }
    }

    #[test]
    fn test_chunk_aad_distinguishes_final_flag() {
        // A truncation attack would try to reuse an AAD from a non-final
        // chunk but claim it as final (or vice versa). The decrypt-time
        // AAD rebuild must differ to catch this.
        let a = chunk_aad(42, false);
        let b = chunk_aad(42, true);
        assert_ne!(a, b, "AAD must differ when final flag differs");
        assert_eq!(a[8], 0);
        assert_eq!(b[8], 1);
    }

    #[test]
    fn test_encrypt_decrypt_chunk_roundtrip() {
        let key = test_key();
        let iv = test_base_iv();
        let pt = b"chunk zero plaintext";
        let frame = encrypt_chunk(&key, &iv, 0, false, pt).unwrap();

        // Frame layout: [4 B length prefix] [ciphertext + tag]
        let declared_len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
        let ct = &frame[4..];
        assert_eq!(ct.len(), declared_len);
        assert_eq!(ct.len(), pt.len() + GCM_TAG_LEN);

        let decrypted = decrypt_chunk(&key, &iv, 0, false, ct).unwrap();
        assert_eq!(decrypted, pt);
    }

    #[test]
    fn test_encrypt_decrypt_chunk_final_flag_preserved() {
        // Writer encrypts the final chunk with is_final=true; reader must
        // pass the same flag to decrypt or GCM auth fails (the whole
        // point of binding final into AAD).
        let key = test_key();
        let iv = test_base_iv();
        let pt = b"tail chunk";
        let frame = encrypt_chunk(&key, &iv, 5, true, pt).unwrap();
        let ct = &frame[4..];

        // Honest reader — matches flag.
        assert_eq!(decrypt_chunk(&key, &iv, 5, true, ct).unwrap(), pt);

        // Malicious reader claiming final=false — must fail (truncation guard).
        let bad = decrypt_chunk(&key, &iv, 5, false, ct);
        assert!(bad.is_err(), "AAD mismatch on final flag must reject");
    }

    #[test]
    fn test_chunk_reordering_is_detected() {
        // Simulate an attacker swapping two chunks on disk: their
        // ciphertexts are valid AEAD outputs, but the AAD they were
        // signed with had different chunk_index values. Decrypt with the
        // SWAPPED index (what an out-of-order reader would compute) must
        // fail.
        let key = test_key();
        let iv = test_base_iv();

        let frame0 = encrypt_chunk(&key, &iv, 0, false, b"chunk-zero").unwrap();
        let frame1 = encrypt_chunk(&key, &iv, 1, false, b"chunk-one_").unwrap();
        let ct0 = &frame0[4..];
        let ct1 = &frame1[4..];

        // Honest sequential decrypt works.
        assert_eq!(
            decrypt_chunk(&key, &iv, 0, false, ct0).unwrap(),
            b"chunk-zero"
        );
        assert_eq!(
            decrypt_chunk(&key, &iv, 1, false, ct1).unwrap(),
            b"chunk-one_"
        );

        // Swapped: try to decrypt chunk 0's ciphertext AS IF it were chunk 1.
        assert!(decrypt_chunk(&key, &iv, 1, false, ct0).is_err());
        assert!(decrypt_chunk(&key, &iv, 0, false, ct1).is_err());
    }

    #[test]
    fn test_chunk_oversized_plaintext_rejected() {
        // encrypt_chunk guards against accidental oversized plaintext
        // (would exceed the frame-size ceiling on disk). Writers must
        // re-slice before calling.
        let key = test_key();
        let iv = test_base_iv();
        let too_big = vec![0u8; CHUNK_PLAINTEXT_SIZE + 1];
        let r = encrypt_chunk(&key, &iv, 0, false, &too_big);
        assert!(r.is_err());
        assert!(r
            .unwrap_err()
            .to_string()
            .contains("chunk plaintext too large"));
    }

    #[test]
    fn test_chunk_tampered_ciphertext_rejected() {
        // Standard AEAD property: any single-bit flip in the ciphertext
        // invalidates the tag. We verify it holds for the chunked path.
        let key = test_key();
        let iv = test_base_iv();
        let frame = encrypt_chunk(&key, &iv, 0, false, b"sensitive").unwrap();
        let mut ct = frame[4..].to_vec();
        ct[0] ^= 0xFF;
        assert!(decrypt_chunk(&key, &iv, 0, false, &ct).is_err());
    }

    #[test]
    fn test_chunk_wrong_key_rejected() {
        let iv = test_base_iv();
        let frame = encrypt_chunk(&test_key(), &iv, 0, false, b"secret").unwrap();
        let ct = &frame[4..];
        assert!(decrypt_chunk(&other_key(), &iv, 0, false, ct).is_err());
    }

    #[test]
    fn test_chunk_empty_plaintext_is_legal() {
        // A zero-byte object still gets ONE frame (a zero-length plaintext)
        // with is_final=true. The frame carries just the GCM tag.
        let key = test_key();
        let iv = test_base_iv();
        let frame = encrypt_chunk(&key, &iv, 0, true, b"").unwrap();
        let declared_len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(declared_len, GCM_TAG_LEN);
        let ct = &frame[4..];
        assert_eq!(decrypt_chunk(&key, &iv, 0, true, ct).unwrap(), b"");
    }

    #[test]
    fn test_chunk_index_for_plaintext_offset() {
        // Boundary and mid-chunk math. If this is wrong, range reads will
        // return garbage. Cover: offset 0, mid-chunk-0, exactly-chunk-1,
        // mid-chunk-1, a huge offset.
        assert_eq!(chunk_index_for_plaintext_offset(0), (0, 0));
        assert_eq!(chunk_index_for_plaintext_offset(1), (0, 1));
        assert_eq!(
            chunk_index_for_plaintext_offset(CHUNK_PLAINTEXT_SIZE as u64 - 1),
            (0, CHUNK_PLAINTEXT_SIZE as u32 - 1)
        );
        assert_eq!(
            chunk_index_for_plaintext_offset(CHUNK_PLAINTEXT_SIZE as u64),
            (1, 0)
        );
        assert_eq!(
            chunk_index_for_plaintext_offset(CHUNK_PLAINTEXT_SIZE as u64 + 42),
            (1, 42)
        );
        // 10 GiB at 64 KiB chunks = 163840 chunks; pick a midway offset.
        let offset_10gib = 10u64 * 1024 * 1024 * 1024 + 777;
        let (idx, off) = chunk_index_for_plaintext_offset(offset_10gib);
        assert_eq!(
            idx as u64 * CHUNK_PLAINTEXT_SIZE as u64 + off as u64,
            offset_10gib
        );
    }

    #[test]
    fn test_wire_offset_of_chunk() {
        // Header is 16 bytes (4 magic + 12 iv). Every chunk is 65556 bytes
        // on the wire (except possibly the final one — the helper is only
        // correct for non-final chunks, but that's all the range path needs:
        // it uses this to SEEK to the start of a chunk, then decrypts from
        // there).
        assert_eq!(wire_offset_of_chunk(0), CHUNK_HEADER_LEN as u64);
        assert_eq!(
            wire_offset_of_chunk(1),
            CHUNK_HEADER_LEN as u64 + CHUNK_FRAME_WIRE_LEN as u64
        );
        assert_eq!(
            wire_offset_of_chunk(100),
            CHUNK_HEADER_LEN as u64 + 100 * CHUNK_FRAME_WIRE_LEN as u64
        );
    }

    #[test]
    fn test_chunk_marker_detection() {
        // is_encrypted is true for BOTH formats; is_chunked_encrypted is
        // true only for the chunked format.
        let mut m = FileMetadata::fallback(
            "test".into(),
            100,
            "md5".into(),
            chrono::Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        assert!(!is_encrypted(&m));
        assert!(!is_chunked_encrypted(&m));

        mark_encrypted(&mut m, None);
        assert!(is_encrypted(&m));
        assert!(!is_chunked_encrypted(&m));

        let mut m2 = FileMetadata::fallback(
            "test".into(),
            100,
            "md5".into(),
            chrono::Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        mark_chunked_encrypted(&mut m2, None);
        assert!(is_encrypted(&m2));
        assert!(is_chunked_encrypted(&m2));
    }

    // ─────────────────────────────────────────────────────────────────
    // Step 3: key_id stamping + mismatch detection
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_mark_encrypted_stamps_key_id_when_present() {
        let mut m = FileMetadata::fallback(
            "x".into(),
            10,
            "md5".into(),
            chrono::Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        mark_encrypted(&mut m, Some("my-kid"));
        assert_eq!(stamped_key_id(&m), Some("my-kid"));
        // Marker and key_id are distinct fields.
        assert!(is_encrypted(&m));
    }

    #[test]
    fn test_mark_encrypted_no_key_id_leaves_field_absent() {
        let mut m = FileMetadata::fallback(
            "x".into(),
            10,
            "md5".into(),
            chrono::Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        mark_encrypted(&mut m, None);
        assert_eq!(stamped_key_id(&m), None);
    }

    #[test]
    fn test_mark_chunked_encrypted_stamps_key_id() {
        let mut m = FileMetadata::fallback(
            "x".into(),
            10,
            "md5".into(),
            chrono::Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        mark_chunked_encrypted(&mut m, Some("chunked-kid"));
        assert_eq!(stamped_key_id(&m), Some("chunked-kid"));
        assert!(is_chunked_encrypted(&m));
    }

    #[test]
    fn test_check_key_id_match_happy_path() {
        assert!(check_key_id_match(Some("same"), Some("same")).is_ok());
    }

    #[test]
    fn test_check_key_id_match_object_has_no_id_is_legacy_ok() {
        // Legacy objects written before Step 3 have no stamp. The
        // wrapper can still decrypt them — the check must NOT fire
        // when only one side has an id.
        assert!(check_key_id_match(None, Some("configured")).is_ok());
    }

    #[test]
    fn test_check_key_id_match_configured_has_no_id_is_ok() {
        // Symmetric. A mode:none-but-somehow-reading-an-encrypted-
        // object path — the OUTER "no key configured" error is the
        // right surface here, not a key_id mismatch.
        assert!(check_key_id_match(Some("obj"), None).is_ok());
    }

    #[test]
    fn test_check_key_id_match_both_absent_is_ok() {
        assert!(check_key_id_match(None, None).is_ok());
    }

    #[test]
    fn test_check_key_id_match_mismatch_errors_with_specifics() {
        let err = check_key_id_match(Some("obj-id"), Some("cfg-id")).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("obj-id") && msg.contains("cfg-id"),
            "error must cite BOTH ids so the operator can reason about \
             the rotation/routing/split-storage cause, got: {msg}"
        );
        // And the hint is present.
        assert!(
            msg.contains("rotated")
                || msg.contains("wrong backend")
                || msg.contains("different keys"),
            "error should explain the typical causes, got: {msg}"
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // B3 regression: range reads on chunked-encrypted objects must
    // fetch only the header + the target chunks, NOT the whole file.
    //
    // Before the fix, `wire_start = 0` pulled the header + every chunk
    // from 0 up to the target, then the decoder threw the leading
    // chunks away. For a request like "last 100 bytes of a 10 GiB
    // object" that meant pulling and decrypting all 10 GiB to emit
    // 100 plaintext bytes.
    //
    // We can't easily prove this with an integration test (mocking
    // network I/O is heavy-weight). Instead we use a tiny counting
    // backend that records the `(start, end)` ranges that the
    // encrypting wrapper asks for from its inner layer. The math
    // guarantees hold: the body fetch must start at or after
    // `wire_offset_of_chunk(first_chunk)`, and the separate header
    // fetch must cover `[0, CHUNK_HEADER_LEN)`.
    // ─────────────────────────────────────────────────────────────────

    use async_trait::async_trait;
    use bytes::Bytes;
    use chrono::Utc;
    use futures::stream::BoxStream;
    use std::sync::Mutex;

    /// A spy backend that records every `get_passthrough_stream_range`
    /// call so tests can assert on WHERE the wrapper reads from. Holds
    /// one passthrough object in memory; everything else no-ops or
    /// errors.
    struct CountingBackend {
        bytes: Vec<u8>,
        metadata: Mutex<Option<FileMetadata>>,
        ranges_requested: Mutex<Vec<(u64, u64)>>,
    }

    impl CountingBackend {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                metadata: Mutex::new(None),
                ranges_requested: Mutex::new(Vec::new()),
            }
        }

        fn set_contents(&mut self, bytes: Vec<u8>, meta: FileMetadata) {
            self.bytes = bytes;
            *self.metadata.lock().unwrap() = Some(meta);
        }
    }

    fn cb_err() -> StorageError {
        StorageError::Other("CountingBackend: not implemented for this test".into())
    }

    #[async_trait]
    impl StorageBackend for CountingBackend {
        async fn get_passthrough_stream_range(
            &self,
            _: &str,
            _: &str,
            _: &str,
            start: u64,
            end: u64,
        ) -> Result<(BoxStream<'static, Result<Bytes, StorageError>>, u64), StorageError> {
            self.ranges_requested.lock().unwrap().push((start, end));
            let end_clamped = std::cmp::min(end, self.bytes.len() as u64 - 1);
            let slice = self.bytes[start as usize..=end_clamped as usize].to_vec();
            let len = slice.len() as u64;
            Ok((
                Box::pin(futures::stream::once(async move { Ok(Bytes::from(slice)) })),
                len,
            ))
        }

        /// Full-stream read. Serves the whole byte vec as a single
        /// Bytes so the chunked decoder's phase-1 header parse hits
        /// the same code path it would over a real network stream.
        async fn get_passthrough_stream(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError> {
            let b = Bytes::from(self.bytes.clone());
            Ok(Box::pin(futures::stream::once(async move { Ok(b) })))
        }

        async fn get_passthrough_metadata(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<FileMetadata, StorageError> {
            self.metadata.lock().unwrap().clone().ok_or_else(cb_err)
        }

        // All other trait methods: not needed for these tests.
        async fn create_bucket(&self, _: &str) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn delete_bucket(&self, _: &str) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
            Err(cb_err())
        }
        async fn list_buckets_with_dates(
            &self,
        ) -> Result<Vec<(String, chrono::DateTime<chrono::Utc>)>, StorageError> {
            Err(cb_err())
        }
        async fn head_bucket(&self, _: &str) -> Result<bool, StorageError> {
            Err(cb_err())
        }
        async fn has_reference(&self, _: &str, _: &str) -> bool {
            false
        }
        async fn put_reference(
            &self,
            _: &str,
            _: &str,
            _: &[u8],
            _: &FileMetadata,
        ) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn get_reference(&self, _: &str, _: &str) -> Result<Vec<u8>, StorageError> {
            Err(cb_err())
        }
        async fn get_reference_metadata(
            &self,
            _: &str,
            _: &str,
        ) -> Result<FileMetadata, StorageError> {
            Err(cb_err())
        }
        async fn put_reference_metadata(
            &self,
            _: &str,
            _: &str,
            _: &FileMetadata,
        ) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn delete_reference(&self, _: &str, _: &str) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn put_delta(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[u8],
            _: &FileMetadata,
        ) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn get_delta(&self, _: &str, _: &str, _: &str) -> Result<Vec<u8>, StorageError> {
            Err(cb_err())
        }
        async fn get_delta_metadata(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<FileMetadata, StorageError> {
            Err(cb_err())
        }
        async fn delete_delta(&self, _: &str, _: &str, _: &str) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn put_passthrough(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[u8],
            _: &FileMetadata,
        ) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn get_passthrough(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<Vec<u8>, StorageError> {
            Err(cb_err())
        }
        async fn delete_passthrough(&self, _: &str, _: &str, _: &str) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn scan_deltaspace(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Vec<FileMetadata>, StorageError> {
            Err(cb_err())
        }
        async fn list_deltaspaces(&self, _: &str) -> Result<Vec<String>, StorageError> {
            Err(cb_err())
        }
        async fn total_size(&self, _: Option<&str>) -> Result<u64, StorageError> {
            Err(cb_err())
        }
        async fn put_directory_marker(&self, _: &str, _: &str) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn bulk_list_objects(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
            Err(cb_err())
        }
        async fn enrich_list_metadata(
            &self,
            _: &str,
            o: Vec<(String, FileMetadata)>,
        ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
            Ok(o)
        }
    }

    /// Shared helper: encrypt a plaintext blob into a chunked-format
    /// wire stream, return the bytes + the final_chunk_index.
    fn encode_chunked(key: &EncryptionKey, plaintext: &[u8]) -> (Vec<u8>, [u8; IV_LEN], u32) {
        let base_iv: [u8; IV_LEN] = [7u8; IV_LEN]; // fixed for determinism
        let mut out = Vec::new();
        out.extend_from_slice(&CHUNK_MAGIC);
        out.extend_from_slice(&base_iv);
        if plaintext.is_empty() {
            let frame = encrypt_chunk(key, &base_iv, 0, true, &[]).unwrap();
            out.extend_from_slice(&frame);
            return (out, base_iv, 0);
        }
        let final_idx = final_chunk_index_for_plaintext_size(plaintext.len() as u64);
        for (idx, pt_chunk) in plaintext.chunks(CHUNK_PLAINTEXT_SIZE).enumerate() {
            let is_final = idx as u32 == final_idx;
            let frame = encrypt_chunk(key, &base_iv, idx as u32, is_final, pt_chunk).unwrap();
            out.extend_from_slice(&frame);
        }
        (out, base_iv, final_idx)
    }

    /// Construct a CountingBackend pre-loaded with a chunked-encrypted
    /// object, wrap it in an EncryptingBackend, and return the wrapper
    /// plus the plaintext for assertion.
    async fn setup_wrapper_with_chunked_object(
        key: EncryptionKey,
        plaintext_size: usize,
    ) -> (EncryptingBackend<CountingBackend>, Vec<u8>) {
        let plaintext: Vec<u8> = (0..plaintext_size).map(|i| (i & 0xff) as u8).collect();
        let (ciphertext, _base_iv, _final_idx) = encode_chunked(&key, &plaintext);

        let mut meta = FileMetadata::fallback(
            "test.bin".into(),
            plaintext_size as u64,
            "md5".into(),
            Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        mark_chunked_encrypted(&mut meta, None);

        let mut backend = CountingBackend::new();
        backend.set_contents(ciphertext, meta);

        let enc_config = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
            key: Some(key),
            key_id: None,
            ..Default::default()
        })));
        let wrapper = EncryptingBackend::new(backend, enc_config);
        (wrapper, plaintext)
    }

    #[tokio::test]
    async fn test_range_read_fetches_only_target_chunks() {
        // 10 × 64 KiB plaintext = 640 KiB, chunks 0..=9. Request bytes
        // covering ONLY chunks 7-8. The wrapper must:
        //   1. Fetch the 16-byte header via a short range [0, 15].
        //   2. Fetch the body covering chunks 7-8 via a widened range
        //      starting at `wire_offset_of_chunk(7)`, NOT at 0.
        // Regression: before the fix, wire_start was hardcoded to 0
        // and the whole file up to last_chunk was pulled.
        let key = test_key();
        let plaintext_size = 10 * CHUNK_PLAINTEXT_SIZE;
        let (wrapper, plaintext) = setup_wrapper_with_chunked_object(key, plaintext_size).await;

        // Bytes covering chunks 7 (pt offset 458752..524287) and 8.
        let start = 7 * CHUNK_PLAINTEXT_SIZE as u64 + 100;
        let end = 8 * CHUNK_PLAINTEXT_SIZE as u64 + 500;
        let (stream, content_length) = wrapper
            .get_passthrough_stream_range("b", "p", "test.bin", start, end)
            .await
            .unwrap();
        assert_eq!(content_length, end - start + 1);

        use futures::TryStreamExt;
        let got: Vec<Bytes> = stream.try_collect().await.unwrap();
        let got: Vec<u8> = got.into_iter().flatten().collect();
        assert_eq!(got, &plaintext[start as usize..=end as usize]);

        // Now the assertion that justifies this test: the wrapper must
        // have made exactly TWO range requests to the inner backend —
        // one tiny one for the header, and one covering chunks 7-8.
        // It must NOT have fetched from wire offset 0 for the body
        // (that would mean pulling chunks 0-6 and throwing them away,
        // the pre-fix behaviour).
        let ranges = wrapper.inner.ranges_requested.lock().unwrap().clone();
        assert_eq!(
            ranges.len(),
            2,
            "expected exactly 2 inner range requests (header + body), got {}: {:?}",
            ranges.len(),
            ranges
        );
        assert_eq!(
            ranges[0],
            (0, CHUNK_HEADER_LEN as u64 - 1),
            "first request must be the 16-byte header fetch"
        );
        let body_wire_start = ranges[1].0;
        let expected_body_start = wire_offset_of_chunk(7);
        assert_eq!(
            body_wire_start, expected_body_start,
            "body fetch must start at wire_offset_of_chunk(first_chunk) = {}, \
             got {} — if this is 0, the wrapper is back to pulling from the \
             file start and the B3 perf fix is broken",
            expected_body_start, body_wire_start
        );
        // Body range must not extend past last_chunk's frame end (chunk
        // 8 is non-final since final is 9).
        let expected_body_end = wire_offset_of_chunk(8) + CHUNK_FRAME_WIRE_LEN as u64 - 1;
        assert_eq!(ranges[1].1, expected_body_end);
    }

    /// B4 regression: `get_passthrough` must return plaintext for BOTH
    /// wire formats. Before the fix, chunked-encrypted objects fell
    /// through to the single-shot `decrypt()` which parsed the 12-byte
    /// segment after the `DGE1` magic as an IV and AEAD-rejected —
    /// breaking any caller who called `get_passthrough` on a chunked
    /// object (latent footgun; no current production caller exercised
    /// the path).
    #[tokio::test]
    async fn test_get_passthrough_handles_chunked_objects() {
        let key = test_key();
        // Cross chunk boundaries — otherwise single-shot might
        // coincidentally look valid.
        let plaintext_size = 3 * CHUNK_PLAINTEXT_SIZE + 123;
        let (wrapper, plaintext) = setup_wrapper_with_chunked_object(key, plaintext_size).await;

        let got = wrapper.get_passthrough("b", "p", "test.bin").await.unwrap();
        assert_eq!(got, plaintext);
    }

    /// B9 regression: if xattrs are stripped during backup/restore,
    /// an on-disk chunked-encrypted body loses its `dg-encrypted`
    /// metadata marker. Without defense-in-depth, the wrapper would
    /// serve raw ciphertext as plaintext. The magic-sniff on the
    /// stream's first emission catches it and errors instead.
    #[tokio::test]
    async fn test_stripped_xattr_with_dge1_body_refuses_to_serve() {
        let key = test_key();
        let plaintext: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
        let (ciphertext, _iv, _final) = encode_chunked(&key, &plaintext);

        // Metadata says plaintext (no encryption marker) — simulates
        // the post-xattr-strip state.
        let meta = FileMetadata::fallback(
            "test.bin".into(),
            plaintext.len() as u64,
            "md5".into(),
            Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );

        let mut backend = CountingBackend::new();
        backend.set_contents(ciphertext, meta);

        let enc_config = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
            key: None,
            key_id: None,
            ..Default::default()
        })));
        let wrapper = EncryptingBackend::new(backend, enc_config);

        let stream = wrapper
            .get_passthrough_stream("b", "p", "test.bin")
            .await
            .expect("stream open should succeed — the error surfaces on first pull");

        use futures::TryStreamExt;
        let res: Result<Vec<Bytes>, _> = stream.try_collect().await;
        let err = res.expect_err(
            "stream must error when body begins with DGE1 but metadata has no marker — \
             otherwise we'd serve ciphertext as plaintext after an xattr strip",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("xattrs") || msg.contains("dg-encrypted"),
            "error must explain the xattr-strip scenario, got: {msg}"
        );
    }

    #[tokio::test]
    async fn test_range_read_last_bytes_of_large_object_bounded_fetch() {
        // The scenario that motivates B3: "last 100 bytes of a large
        // object" must NOT pull the whole file. Here large=64×64KiB=4
        // MiB; the principle is identical for 10 GiB.
        let key = test_key();
        let plaintext_size = 64 * CHUNK_PLAINTEXT_SIZE;
        let (wrapper, plaintext) = setup_wrapper_with_chunked_object(key, plaintext_size).await;

        let start = (plaintext_size - 100) as u64;
        let end = (plaintext_size - 1) as u64;
        let (stream, content_length) = wrapper
            .get_passthrough_stream_range("b", "p", "test.bin", start, end)
            .await
            .unwrap();
        assert_eq!(content_length, 100);

        use futures::TryStreamExt;
        let got: Vec<Bytes> = stream.try_collect().await.unwrap();
        let got: Vec<u8> = got.into_iter().flatten().collect();
        assert_eq!(got, &plaintext[start as usize..=end as usize]);

        let ranges = wrapper.inner.ranges_requested.lock().unwrap().clone();
        assert_eq!(ranges.len(), 2, "expected header + body = 2 requests");
        let body_start = ranges[1].0;
        let last_chunk = final_chunk_index_for_plaintext_size(plaintext_size as u64);
        let expected = wire_offset_of_chunk(last_chunk);
        assert_eq!(
            body_start, expected,
            "must seek to the last chunk's boundary, not file start. \
             expected {}, got {}",
            expected, body_start
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // Step 3: key_id end-to-end — round-trip, mismatch, legacy object
    // ─────────────────────────────────────────────────────────────────

    /// Reconstruct a chunked-encrypted object in memory, wrap with a
    /// given (key, key_id) pair, and return the wrapper + expected
    /// plaintext. Differs from `setup_wrapper_with_chunked_object`:
    /// this variant writes the `dg-encryption-key-id` metadata stamp
    /// directly so we can exercise the READ path's mismatch check
    /// without needing a write cycle.
    async fn setup_wrapper_with_stamped_object(
        key: EncryptionKey,
        stamped_kid: Option<&str>,
        plaintext_size: usize,
        wrapper_key_id: Option<String>,
    ) -> (EncryptingBackend<CountingBackend>, Vec<u8>) {
        let plaintext: Vec<u8> = (0..plaintext_size).map(|i| (i & 0xff) as u8).collect();
        let (ciphertext, _base_iv, _final_idx) = encode_chunked(&key, &plaintext);

        let mut meta = FileMetadata::fallback(
            "test.bin".into(),
            plaintext_size as u64,
            "md5".into(),
            Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        mark_chunked_encrypted(&mut meta, stamped_kid);

        let mut backend = CountingBackend::new();
        backend.set_contents(ciphertext, meta);

        let enc_config = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
            key: Some(key),
            key_id: wrapper_key_id,
            ..Default::default()
        })));
        let wrapper = EncryptingBackend::new(backend, enc_config);
        (wrapper, plaintext)
    }

    #[tokio::test]
    async fn test_read_succeeds_when_key_ids_match() {
        let key = test_key();
        let (wrapper, plaintext) = setup_wrapper_with_stamped_object(
            key,
            Some("matching-id"),
            4096,
            Some("matching-id".to_string()),
        )
        .await;
        let got = wrapper.get_passthrough("b", "p", "test.bin").await.unwrap();
        assert_eq!(got, plaintext);
    }

    #[tokio::test]
    async fn test_read_fails_with_specific_error_when_key_ids_mismatch() {
        // Object says "written with key_id A"; wrapper says "I have
        // key_id B". The AEAD would fail with an opaque message —
        // the specific error must fire FIRST.
        let key = test_key();
        let (wrapper, _plaintext) = setup_wrapper_with_stamped_object(
            key,
            Some("object-a"),
            4096,
            Some("wrapper-b".to_string()),
        )
        .await;

        let res = wrapper.get_passthrough("b", "p", "test.bin").await;
        let err = match res {
            Ok(_) => panic!("must error on key_id mismatch"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("object-a") && msg.contains("wrapper-b"),
            "must cite both key ids, got: {msg}"
        );
    }

    #[tokio::test]
    async fn test_read_fails_via_streaming_range_when_key_ids_mismatch() {
        // Same mismatch, this time through get_passthrough_stream_range.
        // The check fires at stream-open time — BEFORE any AEAD
        // attempt — so the error surfaces as a failed open, not a
        // mid-stream fail.
        let key = test_key();
        let (wrapper, _plaintext) = setup_wrapper_with_stamped_object(
            key,
            Some("object-c"),
            4096,
            Some("wrapper-d".to_string()),
        )
        .await;

        let res = wrapper
            .get_passthrough_stream_range("b", "p", "test.bin", 0, 99)
            .await;
        let err = match res {
            Ok(_) => panic!("range-read must error on key_id mismatch"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("object-c") && msg.contains("wrapper-d"),
            "range-read mismatch must also cite both ids, got: {msg}"
        );
    }

    #[tokio::test]
    async fn test_legacy_object_without_key_id_still_decrypts() {
        // Object written before Step 3 has no `dg-encryption-key-id`
        // stamp. The wrapper has a key_id today. The check is
        // conditional — one-sided absence is legal — so decrypt
        // succeeds as long as the key material matches.
        let key = test_key();
        let (wrapper, plaintext) = setup_wrapper_with_stamped_object(
            key,
            None, // pre-Step-3 object
            4096,
            Some("current-wrapper-id".to_string()),
        )
        .await;
        let got = wrapper.get_passthrough("b", "p", "test.bin").await.unwrap();
        assert_eq!(
            got, plaintext,
            "legacy objects must decrypt when the key itself still matches"
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // Step 5: decrypt-only shim + WriteMode::PassThrough
    // ─────────────────────────────────────────────────────────────────

    /// Inputs for the shim-wrapper fixture. Struct keeps the param
    /// count under clippy's `too_many_arguments` threshold while
    /// still being readable at call sites (field names document
    /// intent better than positional args).
    struct ShimSetup {
        primary_key: Option<EncryptionKey>,
        primary_kid: Option<String>,
        write_mode: WriteMode,
        legacy_key: Option<EncryptionKey>,
        legacy_kid: Option<String>,
        stamped_key: EncryptionKey,
        stamped_kid: Option<&'static str>,
        plaintext_size: usize,
    }

    /// Build a wrapper with a two-key config (primary + legacy shim)
    /// around a CountingBackend pre-loaded with a chunked-encrypted
    /// object encrypted under `stamped_key` + `stamped_kid`.
    async fn setup_shim_wrapper(s: ShimSetup) -> (EncryptingBackend<CountingBackend>, Vec<u8>) {
        let plaintext: Vec<u8> = (0..s.plaintext_size).map(|i| (i & 0xff) as u8).collect();
        let (ciphertext, _iv, _final_idx) = encode_chunked(&s.stamped_key, &plaintext);
        let mut meta = FileMetadata::fallback(
            "test.bin".into(),
            s.plaintext_size as u64,
            "md5".into(),
            Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        mark_chunked_encrypted(&mut meta, s.stamped_kid);
        let mut backend = CountingBackend::new();
        backend.set_contents(ciphertext, meta);
        let cfg = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
            key: s.primary_key,
            key_id: s.primary_kid,
            write_mode: s.write_mode,
            legacy_key: s.legacy_key,
            legacy_key_id: s.legacy_kid,
        })));
        (EncryptingBackend::new(backend, cfg), plaintext)
    }

    #[tokio::test]
    async fn test_shim_decrypts_legacy_stamped_objects() {
        // Scenario: operator migrated from aes256-gcm-proxy (key=K1,
        // id=id-K1) to sse-kms (on S3 side), keeping K1/id-K1 as the
        // legacy shim. A historical object is stamped with id-K1;
        // the wrapper has NO primary key (native mode), write_mode
        // PassThrough. Read must succeed by matching against the
        // legacy shim.
        let k1 = test_key();
        let k1_for_stamp = test_key(); // same bytes, independent allocation
        let (wrapper, plaintext) = setup_shim_wrapper(ShimSetup {
            primary_key: None, // native mode primary
            primary_kid: None,
            write_mode: WriteMode::PassThrough,
            legacy_key: Some(k1),
            legacy_kid: Some("id-K1".to_string()),
            stamped_key: k1_for_stamp,
            stamped_kid: Some("id-K1"),
            plaintext_size: 4096,
        })
        .await;
        let got = wrapper.get_passthrough("b", "p", "test.bin").await.unwrap();
        assert_eq!(got, plaintext);
    }

    #[tokio::test]
    async fn test_shim_primary_key_takes_precedence() {
        // Wrapper has BOTH primary and legacy keys configured. An
        // object stamped with the primary's id decrypts under the
        // primary; the legacy is never consulted. Guards against
        // ambiguous routing where both happen to have the same id
        // (the collision-detect path elsewhere prevents this, but
        // this test pins the tie-break in the wrapper itself).
        let k_primary = test_key();
        let k_primary_stamp = test_key();
        let k_legacy = other_key();
        let (wrapper, plaintext) = setup_shim_wrapper(ShimSetup {
            primary_key: Some(k_primary),
            primary_kid: Some("id-primary".to_string()),
            write_mode: WriteMode::Encrypt,
            legacy_key: Some(k_legacy),
            legacy_kid: Some("id-legacy".to_string()),
            stamped_key: k_primary_stamp,
            stamped_kid: Some("id-primary"),
            plaintext_size: 4096,
        })
        .await;
        let got = wrapper.get_passthrough("b", "p", "test.bin").await.unwrap();
        assert_eq!(got, plaintext);
    }

    #[tokio::test]
    async fn test_shim_legacy_id_only_fires_when_object_matches() {
        // Object stamped with an id that matches NEITHER primary nor
        // legacy. Must error with the specific "rotated without
        // legacy_key" message so the operator knows what's missing.
        let k_primary = test_key();
        let k_orphan_stamp = other_key(); // stamped with DIFFERENT key material
        let (wrapper, _plaintext) = setup_shim_wrapper(ShimSetup {
            primary_key: Some(k_primary),
            primary_kid: Some("id-primary".to_string()),
            write_mode: WriteMode::Encrypt,
            legacy_key: None, // NO legacy — expected to fail hard
            legacy_kid: None,
            stamped_key: k_orphan_stamp,
            stamped_kid: Some("id-orphan"),
            plaintext_size: 4096,
        })
        .await;
        let err = match wrapper.get_passthrough("b", "p", "test.bin").await {
            Ok(_) => panic!("must error when object id matches nothing"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("id-orphan") && msg.contains("id-primary"),
            "error must cite BOTH ids so the operator can diagnose, got: {msg}"
        );
        assert!(
            msg.contains("legacy_key"),
            "error must mention legacy_key as the recovery path, got: {msg}"
        );
    }

    #[test]
    fn test_write_mode_passthrough_skips_encryption() {
        // Direct unit test on `encrypt_if_enabled` via a wrapper.
        // WriteMode::PassThrough must return the plaintext verbatim
        // and NOT stamp the `dg-encrypted` marker — even when a
        // primary key is configured. This is the native-SSE
        // transition invariant: writes go through the proxy wrapper
        // unchanged while old objects still decrypt via the shim.
        let key = test_key();
        let cfg = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
            key: Some(key),
            key_id: Some("current".into()),
            write_mode: WriteMode::PassThrough,
            ..Default::default()
        })));
        let wrapper: EncryptingBackend<CountingBackend> =
            EncryptingBackend::new(CountingBackend::new(), cfg);

        let mut meta = FileMetadata::fallback(
            "x".into(),
            10,
            "md5".into(),
            Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        let plaintext = b"no encrypt for me";
        let out = wrapper.encrypt_if_enabled(plaintext, &mut meta).unwrap();
        assert_eq!(out, plaintext, "PassThrough must return plaintext verbatim");
        assert!(
            !is_encrypted(&meta),
            "PassThrough must NOT stamp dg-encrypted marker"
        );
        assert!(
            stamped_key_id(&meta).is_none(),
            "PassThrough must NOT stamp dg-encryption-key-id"
        );
    }

    #[test]
    fn test_write_mode_encrypt_still_works_normally() {
        // The Encrypt default continues to encrypt + stamp markers.
        // Regression guard that the Default impl on WriteMode is
        // Encrypt (not PassThrough), which the struct-update
        // `..Default::default()` calls in this module rely on.
        let key = test_key();
        let cfg = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
            key: Some(key),
            key_id: Some("id".into()),
            ..Default::default()
        })));
        assert_eq!(cfg.load().write_mode, WriteMode::Encrypt);
        let wrapper: EncryptingBackend<CountingBackend> =
            EncryptingBackend::new(CountingBackend::new(), cfg);

        let mut meta = FileMetadata::fallback(
            "x".into(),
            10,
            "md5".into(),
            Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        let out = wrapper.encrypt_if_enabled(b"secret", &mut meta).unwrap();
        assert_ne!(out, b"secret", "Encrypt must actually encrypt");
        assert!(is_encrypted(&meta), "Encrypt must stamp dg-encrypted");
        assert_eq!(
            stamped_key_id(&meta),
            Some("id"),
            "Encrypt must stamp the key_id"
        );
    }

    #[test]
    fn test_pick_decrypt_key_legacy_object_uses_primary() {
        // Pre-Step-3 objects have no stamp. `pick_decrypt_key`
        // returns the primary key (legacy shim is ignored when the
        // object has no id to match).
        let k_primary = test_key();
        let cfg = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
            key: Some(k_primary),
            key_id: Some("primary-id".into()),
            legacy_key: Some(other_key()),
            legacy_key_id: Some("legacy-id".into()),
            ..Default::default()
        })));
        let wrapper: EncryptingBackend<CountingBackend> =
            EncryptingBackend::new(CountingBackend::new(), cfg);
        // `None` = the object had no `dg-encryption-key-id` stamp.
        let picked = wrapper.pick_decrypt_key(None).unwrap();
        // Assert it's the PRIMARY key by re-encrypting with it and
        // decrypting a round-trip.
        let ct = encrypt(&picked, b"hello").unwrap();
        assert_eq!(decrypt(&test_key(), &ct).unwrap(), b"hello");
    }

    #[test]
    fn test_pick_decrypt_key_no_primary_no_legacy_errors() {
        // Wrapper has no key at all. `pick_decrypt_key` called for
        // an encrypted object must error with the "no key configured"
        // message — same as the pre-shim behaviour.
        let cfg = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig::default())));
        let wrapper: EncryptingBackend<CountingBackend> =
            EncryptingBackend::new(CountingBackend::new(), cfg);
        let err = match wrapper.pick_decrypt_key(Some("some-id")) {
            Ok(_) => panic!("must error when wrapper has no keys"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("some-id") && msg.contains("no legacy-shim match"),
            "error must cite the object id and the missing-legacy hint, got: {msg}"
        );
    }
}
