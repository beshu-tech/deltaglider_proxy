// SPDX-License-Identifier: BUSL-1.1

//! The at-rest formats: the key, the metadata markers, the single-shot
//! `aes-256-gcm-v1` blob and the chunked `aes-256-gcm-chunked-v1` stream
//! (framing, nonces, AAD, the decoder and the `DGE1` sniff). Pure: no
//! backend I/O.

use super::*;

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

pub(super) const IV_LEN: usize = 12;

pub(super) const GCM_TAG_LEN: usize = 16;

// ── Chunked format constants ──
//
// Plaintext chunk size of 64 KiB was picked for four reasons:
//   1. Overhead = 4 B length prefix + 16 B GCM tag = 20 B per chunk ≈ 0.03%.
//   2. Range-read trim cost is at most one extra chunk at each end (≤128 KiB).
//   3. Worker memory per in-flight chunk: ~130 KiB — trivial.
//   4. Nonce space: 2^32 chunks × 64 KiB = 256 TiB per object.
pub(super) const CHUNK_MAGIC: [u8; 4] = *b"DGE1";

pub const CHUNK_PLAINTEXT_SIZE: usize = 64 * 1024;

pub(super) const CHUNK_FRAME_LEN_FIELD: usize = 4;

pub(super) const CHUNK_HEADER_LEN: usize = 4 /*magic*/ + 12 /*base_iv*/;

/// Wire size of every non-final chunk (length-prefix + ciphertext + tag).
pub const CHUNK_FRAME_WIRE_LEN: usize = CHUNK_FRAME_LEN_FIELD + CHUNK_PLAINTEXT_SIZE + GCM_TAG_LEN;

/// Upper bound of the chunked wire size of `plaintext` bytes: the header,
/// the bytes, and a length field + tag per frame (one frame more than the
/// full windows, for the final one). Sizes the spool reservation.
pub(super) fn chunked_wire_len_bound(plaintext: u64) -> u64 {
    let frames = plaintext / CHUNK_PLAINTEXT_SIZE as u64 + 1;
    CHUNK_HEADER_LEN as u64 + plaintext + frames * (CHUNK_FRAME_LEN_FIELD + GCM_TAG_LEN) as u64
}

/// Cap on the length-prefix to foil DOS-via-crafted-length allocations.
/// A legitimate chunk can never exceed 64 KiB + tag + a tiny buffer.
/// Enforced by the streaming chunk decoders
/// (`chunked_decrypt_stream` + `chunked_decrypt_stream_from_chunk`)
/// before they allocate the per-chunk buffer.
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

/// Derive the per-chunk nonce: `base_iv XOR (chunk_index as big-endian u96)`.
///
/// We XOR rather than append/concatenate because `base_iv` is already 12 bytes
/// (the exact nonce size) and we need a deterministic, collision-free mapping
/// from `(base_iv, index)` to a 12-byte nonce. XOR gives 2^32 distinct nonces
/// per object, well past any passthrough we'd see.
pub(super) fn chunk_nonce(base_iv: &[u8; IV_LEN], chunk_index: u32) -> [u8; IV_LEN] {
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
pub(super) fn chunk_aad(chunk_index: u32, is_final: bool) -> [u8; 16] {
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

/// Sequences a stream of ≤64 KiB plaintext windows into framed ciphertext,
/// owning the ONE tricky invariant of the chunked format: the LAST frame — and
/// only the last — is stamped `is_final=true` (the read-side truncation guard),
/// with monotonic `chunk_index` and overflow protection. A zero-byte object
/// yields exactly one empty final frame.
///
/// It works one window AHEAD: `push_window` emits the PREVIOUSLY-held window as
/// a non-final frame (we now know a successor exists) and holds the new one;
/// `finish` emits whatever is held (or an empty window) as the final frame. Both
/// the file→tempfile and the in-memory `Vec<Bytes>` paths drive this, so the
/// is_final/index logic lives in exactly one place.
pub(super) struct ChunkFramer {
    base_iv: [u8; IV_LEN],
    index: u32,
    /// The window seen most recently but not yet emitted (we don't know if it's
    /// final until either another window arrives or `finish` is called).
    pending: Option<Vec<u8>>,
    started: bool,
}

impl ChunkFramer {
    /// A framer with a fresh random per-object base IV, plus the wire-format
    /// header (`[magic][base_iv]`) that must precede its frames.
    pub(super) fn with_random_iv() -> (Self, Vec<u8>) {
        let mut base_iv = [0u8; IV_LEN];
        rand::rngs::OsRng.fill_bytes(&mut base_iv);
        let mut header = Vec::with_capacity(CHUNK_HEADER_LEN);
        header.extend_from_slice(&CHUNK_MAGIC);
        header.extend_from_slice(&base_iv);
        (Self::new(base_iv), header)
    }

    pub(super) fn new(base_iv: [u8; IV_LEN]) -> Self {
        Self {
            base_iv,
            index: 0,
            pending: None,
            started: false,
        }
    }

    /// Take ownership of the next plaintext window. Returns the frame for the
    /// PREVIOUS window (now known non-final), or `None` for the first window.
    pub(super) fn push_window(
        &mut self,
        key: &EncryptionKey,
        window: Vec<u8>,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        self.started = true;
        let emitted = match self.pending.take() {
            Some(prev) => {
                let frame = encrypt_chunk(key, &self.base_iv, self.index, false, &prev)?;
                self.index = self.index.checked_add(1).ok_or_else(|| {
                    StorageError::Encryption("chunk index overflow (object too large)".into())
                })?;
                Some(frame)
            }
            None => None,
        };
        self.pending = Some(window);
        Ok(emitted)
    }

    /// Emit the final frame: the held window, or an empty final frame for a
    /// zero-byte object (nothing was ever pushed).
    pub(super) fn finish(mut self, key: &EncryptionKey) -> Result<Vec<u8>, StorageError> {
        let final_pt = self.pending.take().unwrap_or_default();
        let _ = self.started;
        encrypt_chunk(key, &self.base_iv, self.index, true, &final_pt)
    }
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

/// State machine for the chunked wire-format decoder.
///
/// Carried through `futures::stream::unfold` so we don't need a
/// manual `pin_project` dependency. See `chunked_decrypt_stream`
/// below for the public builder.
pub(super) struct DecryptState<S>
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
pub(super) fn chunked_decrypt_stream<S>(
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
    decrypt_stream_from_state(state)
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
pub(super) fn chunked_decrypt_stream_from_chunk<S>(
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
    decrypt_stream_from_state(state)
}

/// Shared unfold body for the two chunked-decrypt entry points above.
/// The only difference between "full stream" and "range stream" is the
/// initial `DecryptState` — the iteration logic (phase-1 header parse,
/// phase-2 frame parse, decrypt, skip/take plaintext trim) is
/// bit-for-bit identical. Keeping one copy of this AEAD-critical loop
/// avoids the "fix a bug in one, forget the other" risk.
pub(super) fn decrypt_stream_from_state<S>(
    state: DecryptState<S>,
) -> BoxStream<'static, Result<Bytes, StorageError>>
where
    S: futures::Stream<Item = Result<Bytes, StorageError>> + Unpin + Send + 'static,
{
    Box::pin(futures::stream::unfold(state, |mut st| async move {
        use futures::StreamExt;
        loop {
            // Early termination by caller bound.
            if matches!(st.take_bytes, Some(0)) {
                return None;
            }

            // Phase 1: header ([magic][base_iv]). Skipped when the
            // caller (range decoder) already consumed the header out
            // of band — they pre-set `header_done = true`.
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

            // If we've already emitted the final chunk, we're done.
            // Any trailing bytes from the inner stream are a framing
            // violation — the final chunk's AAD authenticated "this
            // is the end", so trailing bytes mean the file was modified
            // post-write (backup/restore dropped xattrs + left trailing
            // data, concatenation attack, disk corruption that missed
            // the GCM tag but mangled tail bytes).
            //
            // H7: bumped from a silent debug-log to WARN so the oddity
            // isn't swallowed by production log filtering. We continue
            // to return None rather than an Err because the plaintext
            // has already been streamed to the client and we can't
            // unring that bell — but operators need to see the warn
            // to catch a backup/restore regression before it spreads.
            if st.emitted_final {
                if !st.buf.is_empty() {
                    tracing::warn!(
                        "chunked-encryption decoder: {} trailing bytes after final frame — \
                         the plaintext has already been emitted (the AAD-authenticated final \
                         flag fires at the right place). Check for a broken backup/restore \
                         path or post-write tampering. First 16 bytes (hex): {}",
                        st.buf.len(),
                        hex::encode(&st.buf[..st.buf.len().min(16)])
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

/// Compute the index of the final chunk given a plaintext byte
/// count. Zero-byte objects still have one chunk (index 0 with empty
/// plaintext) — the write path guarantees this.
pub(super) fn final_chunk_index_for_plaintext_size(plaintext_size: u64) -> u32 {
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
/// Cost: buffers the leading chunks until 4 bytes (or EOF) are seen, then
/// re-emits them as one `Bytes`. Chunks shorter than 4 bytes are joined,
/// so a split magic is still caught. Zero extra network/disk round-trips.
pub(super) fn sniff_dge1_magic<S>(inner: S) -> BoxStream<'static, Result<Bytes, StorageError>>
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
                State::Initial(mut inner) => {
                    let mut head = bytes::BytesMut::new();
                    let mut ended = false;
                    while head.len() < CHUNK_MAGIC.len() {
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
                    if head.starts_with(&CHUNK_MAGIC) {
                        return Some((Err(stripped_marker_error()), State::Done));
                    }
                    // Never poll an ended stream again: the S3 body stream
                    // (an `unfold`) panics when polled after `None`.
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
        "object body begins with chunked-encryption magic but \
         metadata has no dg-encrypted marker — xattrs may have \
         been stripped during backup/restore. Refusing to serve \
         ciphertext as plaintext."
            .into(),
    )
}
