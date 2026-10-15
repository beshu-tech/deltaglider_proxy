// SPDX-License-Identifier: BUSL-1.1

//! The key type and the metadata marker names of the at-rest encryption.
//! The format itself is in `v2.rs`, the sealed metadata in `seal.rs`.

/// The user-metadata key of the encryption marker. Callers above the
/// wrapper see `v2` on an encrypted object (`seal::PRESENTED_MARKER_VALUE`);
/// the backend stores `seal::DISK_MARKER`.
pub const ENCRYPTION_MARKER_KEY: &str = crate::types::meta_keys::ENCRYPTED;

/// The user-metadata key that names, above the wrapper, the id of the key
/// that encrypted an object. Never stored: the wrapper derives it from the
/// key fingerprint in the object's seal.
pub const ENCRYPTION_KEY_ID_KEY: &str = "dg-encryption-key-id";

/// AES-256 encryption key (32 bytes). Zeroized on drop.
#[derive(Clone)]
pub struct EncryptionKey(pub(crate) [u8; 32]);

impl EncryptionKey {
    pub fn from_hex(hex_str: &str) -> Result<Self, String> {
        // The decoded copy is wiped on drop too, not only the key.
        let bytes = zeroize::Zeroizing::new(
            hex::decode(hex_str).map_err(|e| format!("invalid hex key: {}", e))?,
        );
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
