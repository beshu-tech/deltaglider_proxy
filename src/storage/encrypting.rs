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
//! **`aes-256-gcm-chunked-v1`** (chunked, streaming) — used ONLY for
//! `put_passthrough_chunked`. Passthrough objects are user uploads with no
//! size ceiling — a 5 GiB upload must not OOM the process, so we encrypt
//! in 64-KiB plaintext windows and decrypt chunk-by-chunk on read.
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

/// Hot-reloadable encryption configuration.
pub struct EncryptionConfig {
    pub key: Option<EncryptionKey>,
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

pub fn mark_encrypted(metadata: &mut FileMetadata) {
    metadata.user_metadata.insert(
        ENCRYPTION_MARKER_KEY.to_string(),
        ENCRYPTION_MARKER_VALUE.to_string(),
    );
    // TODO(key-rotation): once we ship multi-key support, also stamp a
    // `dg-encryption-key-id: <hex>` so reads can dispatch to the right
    // key. Until then, the current key is assumed for every encrypted
    // object. See docs/dev/historical/ for the rotation design sketch.
}

/// Stamp the chunked-format marker. Called when writing via the streaming
/// chunked path; distinguishes the wire format on read.
pub fn mark_chunked_encrypted(metadata: &mut FileMetadata) {
    metadata.user_metadata.insert(
        ENCRYPTION_MARKER_KEY.to_string(),
        CHUNK_MARKER_VALUE.to_string(),
    );
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

    fn encrypt_if_enabled(
        &self,
        data: &[u8],
        metadata: &mut FileMetadata,
    ) -> Result<Vec<u8>, StorageError> {
        if let Some(key) = self.current_key() {
            let encrypted = encrypt(&key, data)?;
            mark_encrypted(metadata);
            Ok(encrypted)
        } else {
            Ok(data.to_vec())
        }
    }

    fn decrypt_if_needed(
        &self,
        data: Vec<u8>,
        metadata: &FileMetadata,
    ) -> Result<Vec<u8>, StorageError> {
        if is_encrypted(metadata) {
            if let Some(key) = self.current_key() {
                decrypt(&key, &data)
            } else {
                Err(StorageError::Encryption(
                    "object is encrypted but no key is configured".into(),
                ))
            }
        } else {
            Ok(data)
        }
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

    // put_passthrough_chunked: concatenates chunks, encrypts whole blob, delegates to put_passthrough.
    // When encryption is off, delegates to inner's chunked impl directly.
    async fn put_passthrough_chunked(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        chunks: &[Bytes],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        if self.current_key().is_some() {
            let total: usize = chunks.iter().map(|c| c.len()).sum();
            let mut buf = Vec::with_capacity(total);
            for c in chunks {
                buf.extend_from_slice(c);
            }
            let mut meta = metadata.clone();
            let enc = self.encrypt_if_enabled(&buf, &mut meta)?;
            self.inner
                .put_passthrough(bucket, prefix, filename, &enc, &meta)
                .await
        } else {
            self.inner
                .put_passthrough_chunked(bucket, prefix, filename, chunks, metadata)
                .await
        }
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
        let data = self.inner.get_passthrough(bucket, prefix, filename).await?;
        let meta = self
            .inner
            .get_passthrough_metadata(bucket, prefix, filename)
            .await?;
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
        if is_encrypted(&meta) {
            let data = self.inner.get_passthrough(bucket, prefix, filename).await?;
            let plain = self.decrypt_if_needed(data, &meta)?;
            Ok(Box::pin(futures::stream::once(async {
                Ok(Bytes::from(plain))
            })))
        } else {
            self.inner
                .get_passthrough_stream(bucket, prefix, filename)
                .await
        }
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
        if is_encrypted(&meta) {
            let data = self.inner.get_passthrough(bucket, prefix, filename).await?;
            let plain = self.decrypt_if_needed(data, &meta)?;
            let s = start as usize;
            let e = std::cmp::min(end as usize, plain.len());
            let slice = Bytes::from(plain[s..e].to_vec());
            let len = slice.len() as u64;
            Ok((Box::pin(futures::stream::once(async { Ok(slice) })), len))
        } else {
            self.inner
                .get_passthrough_stream_range(bucket, prefix, filename, start, end)
                .await
        }
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
        mark_encrypted(&mut m);
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

        mark_encrypted(&mut m);
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
        mark_chunked_encrypted(&mut m2);
        assert!(is_encrypted(&m2));
        assert!(is_chunked_encrypted(&m2));
    }
}
