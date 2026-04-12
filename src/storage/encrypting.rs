//! Transparent encryption-at-rest wrapper for any StorageBackend.
//!
//! `EncryptingBackend<B>` wraps a storage backend and encrypts all object data
//! with AES-256-GCM before writing, decrypting on read. Metadata is NOT encrypted.
//!
//! Wire format: `[12-byte IV] [ciphertext + 16-byte GCM auth tag]`
//! Overhead: 28 bytes per object.
//!
//! Detection: objects with `dg-encrypted: aes-256-gcm-v1` in metadata are decrypted;
//! objects without it are returned as-is (backward compatible).

use super::traits::{DelegatedListResult, StorageBackend, StorageError};
use crate::types::FileMetadata;
use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use rand::RngCore;
use std::sync::Arc;

pub const ENCRYPTION_MARKER_KEY: &str = "dg-encrypted";
pub const ENCRYPTION_MARKER_VALUE: &str = "aes-256-gcm-v1";
const IV_LEN: usize = 12;

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
        .map(|v| v == ENCRYPTION_MARKER_VALUE)
        .unwrap_or(false)
}

pub fn mark_encrypted(metadata: &mut FileMetadata) {
    metadata.user_metadata.insert(
        ENCRYPTION_MARKER_KEY.to_string(),
        ENCRYPTION_MARKER_VALUE.to_string(),
    );
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
}
