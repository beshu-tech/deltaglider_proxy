// SPDX-License-Identifier: BUSL-1.1

//! Secret material with non-secret identifiers.
//!
//! Move D from the architectural review. Today the codebase has
//! three secret subsystems with no shared abstraction:
//!
//! 1. **Per-backend AES key** ([`crate::storage::encrypting::EncryptionKey`])
//!    — 32 bytes, zeroize-on-drop, paired with a non-secret `key_id`
//!    derived from `SHA-256(backend_name || key)`. The cleanest of
//!    the three; this trait formalises its shape.
//! 2. **Bootstrap password** ([`crate::config::Config::bootstrap_password_hash`])
//!    — bcrypt hash, no separate id concept. Encrypts the SQLCipher
//!    config DB AND signs admin GUI sessions. Predates the IAM era.
//!    Not yet migrated to this trait.
//! 3. **SQLCipher key** — derived from the bootstrap password,
//!    opaque to everything except the SQLCipher driver. Not yet
//!    migrated.
//!
//! When the next enterprise integration lands (KMS, HashiCorp Vault,
//! AWS Secrets Manager), it should arrive as a single new [`Secret`]
//! impl rather than three parallel implementations across the three
//! subsystems. That's the load-bearing payoff of this trait.
//!
//! ## Design choices
//!
//! - **Material is opaque**, exposed only via [`Secret::material`]
//!   which returns a borrowed `&[u8]`. Callers must NOT copy the
//!   bytes longer than one operation. Owned access requires
//!   explicit cloning of the underlying `Secret` impl.
//! - **`id()` is non-secret** and stable. It's the answer to "do
//!   two operations agree on which key was used?" Used by
//!   `dg-encryption-key-id` object metadata to detect mismatches.
//! - **Zeroize-on-drop** is enforced by the impl, not the trait.
//!   The trait guarantees the *contract*; impls choose how (some
//!   secrets — KMS handles, for instance — never leave the remote
//!   service and have nothing to zeroize locally).
//! - **No `From<&str>` constructor**. Each impl owns its
//!   construction story (hex-decode, KMS-fetch, file-read,
//!   environment-variable). The trait stays minimal.

/// Non-secret identifier for a [`Secret`]. Stable across loads of
/// the same secret material; varies when the material varies.
///
/// Use cases:
///   - Stamped on encrypted objects so reads can detect
///     cross-backend key mismatch.
///   - Logged on key rotation events so operators can audit which
///     key was active when.
///   - Used as the lookup key in legacy-shim resolvers.
///
/// MUST NOT contain secret material or anything derived from it
/// without an irreversible hash. The current `EncryptionKey`
/// implementation hashes `(backend_name, key_bytes)` together to
/// avoid revealing the same key being reused across backends.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SecretId(String);

impl SecretId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SecretId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A secret with a stable non-secret identifier.
///
/// Implementations MUST:
/// - Zeroize material on drop (or ensure material never lives in
///   process memory in the first place — for KMS-style
///   implementations).
/// - Return a stable `id()` for the lifetime of the secret.
/// - Not implement `Debug` in a way that exposes material.
pub trait Secret: Send + Sync {
    /// Stable, non-secret identifier. Same input material always
    /// produces the same id; different material always produces
    /// different ids.
    fn id(&self) -> &SecretId;

    /// Borrowed access to the secret bytes. Callers must not copy
    /// the slice into long-lived storage; clone the `Secret`
    /// instead so zeroize-on-drop semantics are preserved.
    ///
    /// For KMS-style impls where the material doesn't live in
    /// process memory, this method returns the secret's local
    /// representation (cached after a fetch, zeroized on drop).
    fn material(&self) -> &[u8];

    /// Length of the material in bytes. Default delegates to
    /// `material().len()` but impls can override for KMS-style
    /// remote secrets where computing the length doesn't need a
    /// fetch.
    fn len(&self) -> usize {
        self.material().len()
    }

    /// True iff the secret is the empty byte string. Provided for
    /// `clippy::len_without_is_empty` compliance; very few real
    /// secrets are empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Boxed dynamic dispatch over [`Secret`] for hot-reload paths.
/// Same shape as `Arc<dyn StorageBackend>` elsewhere in the
/// codebase. Use this when the secret subsystem (KMS-vs-local-vs-
/// Vault) is a runtime config decision.
pub type DynSecret = std::sync::Arc<dyn Secret>;

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial in-memory impl for the trait tests. Not for use
    /// outside this module.
    struct InMemorySecret {
        id: SecretId,
        material: Vec<u8>,
    }

    impl InMemorySecret {
        fn new(id: &str, material: Vec<u8>) -> Self {
            Self {
                id: SecretId::new(id),
                material,
            }
        }
    }

    impl Drop for InMemorySecret {
        fn drop(&mut self) {
            zeroize::Zeroize::zeroize(&mut self.material);
        }
    }

    impl Secret for InMemorySecret {
        fn id(&self) -> &SecretId {
            &self.id
        }
        fn material(&self) -> &[u8] {
            &self.material
        }
    }

    #[test]
    fn secret_id_round_trips() {
        let id = SecretId::new("kid-abc");
        assert_eq!(id.as_str(), "kid-abc");
        assert_eq!(id.to_string(), "kid-abc");
    }

    #[test]
    fn secret_trait_methods_work() {
        let s = InMemorySecret::new("test-id", vec![1, 2, 3, 4, 5]);
        assert_eq!(s.id().as_str(), "test-id");
        assert_eq!(s.material(), &[1, 2, 3, 4, 5]);
        assert_eq!(s.len(), 5);
        assert!(!s.is_empty());
    }

    #[test]
    fn empty_secret_reports_empty() {
        let s = InMemorySecret::new("empty", vec![]);
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
    }

    /// Zeroize-on-drop happens, but we can't observe it after drop
    /// (the memory is gone). We assert on the live behaviour: the
    /// Drop impl exists and runs. The fact that `Vec::zeroize` is
    /// called at drop is a property of the impl, not the trait.
    #[test]
    fn drop_runs_without_panic() {
        let s = InMemorySecret::new("dropme", vec![0xff; 32]);
        drop(s); // No panic; zeroize is called inside Drop.
    }

    /// Different material → different id when constructed properly.
    /// (For the InMemorySecret test impl, the caller picks the id;
    /// real impls — like EncryptionKey — derive the id from the
    /// material so this property holds automatically.)
    #[test]
    fn different_ids_distinguishable() {
        let a = InMemorySecret::new("a", vec![1, 2]);
        let b = InMemorySecret::new("b", vec![3, 4]);
        assert_ne!(a.id(), b.id());
    }

    /// `DynSecret` (Arc<dyn Secret>) works for hot-reload patterns.
    #[test]
    fn dyn_secret_arc_works() {
        let s: DynSecret = std::sync::Arc::new(InMemorySecret::new("dyn", vec![9, 8, 7]));
        let cloned: DynSecret = s.clone();
        assert_eq!(cloned.id().as_str(), "dyn");
        assert_eq!(cloned.material(), &[9, 8, 7]);
    }
}
