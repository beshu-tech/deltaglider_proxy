// SPDX-License-Identifier: BUSL-1.1

//! Sealed LIST continuation tokens.
//!
//! A filtered LIST (a caller who may see only some keys) that spends its
//! scan budget inside a run of hidden keys stops at a scan position that can
//! be a hidden key. The next page must start there: a restart from the last
//! visible entry would read the same hidden run again and never end. So
//! that position goes to the client SEALED: encrypted and authenticated
//! with AES-256-GCM under a key derived from the config DB key. The client
//! cannot read it and cannot change it.
//!
//! The key: HKDF-SHA256 of the config DB key (`DGP_CONFIG_DB_KEY`, else the
//! node's `<db>.key` file) with the label `dgp-list-token-v1`. Every node of
//! a multi-instance deployment shares `DGP_CONFIG_DB_KEY`, so a token from
//! one node opens on every other node. The key is installed once at startup
//! ([`install_list_token_keys`]); the request path never reads the
//! environment. Before installation (tests, tools), a random per-process key.
//!
//! The token is bound to the bucket, the prefix and the delimiter of the
//! listing (AEAD associated data): a sealed position is a cursor of ONE
//! listing, never a probe into another bucket or prefix.

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Marks a sealed token. A `dg1.` token carries a visible key in clear.
pub(super) const SEALED_TOKEN_PREFIX: &str = "dg2.";

/// The HKDF `info` label of the token key.
const KEY_LABEL: &[u8] = b"dgp-list-token-v1";

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

/// A token key (32 bytes, AES-256).
pub(super) type TokenKey = [u8; 32];

/// The keys tokens open with; the first one seals.
static TOKEN_KEYS: std::sync::OnceLock<Vec<TokenKey>> = std::sync::OnceLock::new();

/// Pure: HKDF-SHA256 (RFC 5869) with 32 bytes of output. An empty `salt` is
/// a string of zeros, as the RFC says.
fn hkdf_sha256_32(salt: &[u8], ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let zeros = [0u8; 32];
    let salt = if salt.is_empty() { &zeros[..] } else { salt };
    let mut extract =
        <Hmac<Sha256> as Mac>::new_from_slice(salt).expect("HMAC takes a key of any size");
    extract.update(ikm);
    let prk = extract.finalize().into_bytes();
    let mut expand =
        <Hmac<Sha256> as Mac>::new_from_slice(&prk).expect("HMAC takes a key of any size");
    expand.update(info);
    expand.update(&[1]);
    expand.finalize().into_bytes().into()
}

/// Pure: the token key for a config DB key. Never the DB key itself.
pub(super) fn derive_list_token_key(db_key: &str) -> TokenKey {
    hkdf_sha256_32(&[], db_key.as_bytes(), KEY_LABEL)
}

/// Pure: the token keys for the config DB keys; the first one seals. A
/// token sealed before a key rotation (`DGP_CONFIG_DB_KEY_PREVIOUS`), or
/// before a move from the key file to `DGP_CONFIG_DB_KEY`, still opens. The
/// legacy bootstrap hash never sealed a token, so it is not a token key.
fn token_keys_for(keys: &crate::config_db::ConfigDbKeys) -> Vec<TokenKey> {
    use crate::config_db::key::FallbackKind;
    std::iter::once(keys.primary.expose())
        .chain(
            keys.fallbacks
                .iter()
                .filter(|(kind, _)| {
                    matches!(kind, FallbackKind::PreviousKey | FallbackKind::KeyFile)
                })
                .map(|(_, k)| k.expose()),
        )
        .map(derive_list_token_key)
        .collect()
}

/// Install the token keys from the config DB keys (startup, once).
pub fn install_list_token_keys(keys: &crate::config_db::ConfigDbKeys) {
    let _ = TOKEN_KEYS.set(token_keys_for(keys));
}

fn token_keys() -> &'static [TokenKey] {
    TOKEN_KEYS.get_or_init(|| {
        use rand::RngCore;
        let mut k = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut k);
        vec![k]
    })
}

/// The listing a sealed token belongs to.
#[derive(Debug, Clone, Copy)]
pub(super) struct TokenBinding<'a> {
    pub bucket: &'a str,
    pub prefix: &'a str,
    pub delimiter: Option<&'a str>,
}

impl TokenBinding<'_> {
    /// The associated data: every field length-prefixed, so two bindings
    /// never give the same bytes. An empty delimiter is no delimiter, as
    /// for the listing.
    fn aad(&self) -> Vec<u8> {
        let mut out = KEY_LABEL.to_vec();
        let delimiter = self.delimiter.filter(|d| !d.is_empty());
        for field in [Some(self.bucket), Some(self.prefix), delimiter] {
            match field {
                Some(f) => {
                    out.push(1);
                    out.extend_from_slice(&(f.len() as u64).to_be_bytes());
                    out.extend_from_slice(f.as_bytes());
                }
                None => out.push(0),
            }
        }
        out
    }
}

/// Pure: seal the scan position `position` with `key` (random nonce).
pub(super) fn seal_with(key: &TokenKey, position: &str, binding: TokenBinding<'_>) -> String {
    use base64::Engine as _;
    use rand::RngCore;
    let mut nonce = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let cipher = Aes256Gcm::new(key.into());
    let aad = binding.aad();
    let sealed = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: position.as_bytes(),
                aad: &aad,
            },
        )
        .expect("AES-GCM seals any message shorter than 64 GiB");
    let mut blob = nonce.to_vec();
    blob.extend_from_slice(&sealed);
    format!(
        "{SEALED_TOKEN_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(blob)
    )
}

/// Why a sealed token did not open. The reason is for logs only: the
/// client gets one answer for all of them.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum OpenError {
    /// Not base64url, or too short to hold a nonce and a tag.
    Malformed,
    /// No key opens it: a forged or changed token, another key, or
    /// another listing.
    Rejected,
}

/// Pure: the envelope bytes of a `dg2.` token, when it has the shape of
/// one (base64url, room for a nonce and a tag).
fn envelope(token: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let blob = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token.strip_prefix(SEALED_TOKEN_PREFIX)?)
        .ok()?;
    (blob.len() >= NONCE_LEN + TAG_LEN).then_some(blob)
}

/// Pure: `true` when `s` has the shape of a sealed token. A V1 `marker`
/// with this shape must open; any other marker is a key.
pub(super) fn looks_sealed(s: &str) -> bool {
    envelope(s).is_some()
}

/// Pure: open a sealed token with the first of `keys` that opens it.
pub(super) fn open_with(
    keys: &[TokenKey],
    token: &str,
    binding: TokenBinding<'_>,
) -> Result<String, OpenError> {
    let blob = envelope(token).ok_or(OpenError::Malformed)?;
    let (nonce, sealed) = blob.split_at(NONCE_LEN);
    let aad = binding.aad();
    keys.iter()
        .find_map(|key| {
            Aes256Gcm::new(key.into())
                .decrypt(
                    Nonce::from_slice(nonce),
                    Payload {
                        msg: sealed,
                        aad: &aad,
                    },
                )
                .ok()
        })
        .and_then(|plain| String::from_utf8(plain).ok())
        .ok_or(OpenError::Rejected)
}

/// Seal a scan position with the installed key.
pub(super) fn seal(position: &str, binding: TokenBinding<'_>) -> String {
    seal_with(&token_keys()[0], position, binding)
}

/// Open a sealed token with the installed keys. A token that does not open
/// is `InvalidArgument`: the listing never restarts somewhere else, because
/// two nodes with different keys would then answer the same page forever.
pub(super) fn open(token: &str, binding: TokenBinding<'_>) -> s3s::S3Result<String> {
    open_with(token_keys(), token, binding).map_err(|e| {
        tracing::info!(
            bucket = binding.bucket,
            reason = ?e,
            "LIST continuation token does not open (forged, changed, from another \
             listing, or sealed with another DGP_CONFIG_DB_KEY)"
        );
        s3s::s3_error!(
            InvalidArgument,
            "The continuation token provided is incorrect"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: TokenBinding<'static> = TokenBinding {
        bucket: "b",
        prefix: "p/",
        delimiter: Some("/"),
    };

    /// RFC 5869, test case 3 (empty salt, empty info): the first 32 bytes
    /// of the OKM.
    #[test]
    fn hkdf_matches_the_rfc_vector() {
        let okm = hkdf_sha256_32(&[], &[0x0b; 22], &[]);
        assert_eq!(
            hex::encode(okm),
            "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d"
        );
    }

    #[test]
    fn the_token_key_is_derived_not_the_db_key() {
        let db_key = "k".repeat(64);
        let key = derive_list_token_key(&db_key);
        assert_ne!(&key[..], &db_key.as_bytes()[..32]);
        assert_eq!(key, derive_list_token_key(&db_key), "stable across boots");
        assert_ne!(key, derive_list_token_key(&"j".repeat(64)));
    }

    #[test]
    fn a_sealed_token_round_trips_and_hides_the_position() {
        let key = derive_list_token_key(&"k".repeat(64));
        for position in ["h/10", "", "h/é<&>\u{1}", &"x".repeat(1024)] {
            let t = seal_with(&key, position, B);
            assert!(t.starts_with(SEALED_TOKEN_PREFIX));
            assert!(
                t.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
                "{t}"
            );
            assert!(looks_sealed(&t));
            if position.len() >= 3 {
                assert!(!t.contains(position), "the position shows in clear");
            }
            assert_eq!(open_with(&[key], &t, B).as_deref(), Ok(position));
        }
        // Random nonce: one position seals to two different tokens.
        assert_ne!(seal_with(&key, "h/10", B), seal_with(&key, "h/10", B));
    }

    #[test]
    fn a_changed_token_does_not_open() {
        let key = derive_list_token_key(&"k".repeat(64));
        let t = seal_with(&key, "h/10", B);
        let body = t.strip_prefix(SEALED_TOKEN_PREFIX).unwrap();
        for i in 0..body.len() {
            let mut bytes = body.as_bytes().to_vec();
            bytes[i] = if bytes[i] == b'A' { b'B' } else { b'A' };
            let changed = format!("{SEALED_TOKEN_PREFIX}{}", String::from_utf8(bytes).unwrap());
            assert!(open_with(&[key], &changed, B).is_err(), "byte {i}");
        }
        assert_eq!(
            open_with(&[key], "dg2.", B),
            Err(OpenError::Malformed),
            "too short"
        );
        assert_eq!(open_with(&[key], "dg2.!!", B), Err(OpenError::Malformed));
    }

    #[test]
    fn a_token_of_another_key_or_listing_does_not_open() {
        let key = derive_list_token_key(&"k".repeat(64));
        let other = derive_list_token_key(&"j".repeat(64));
        let t = seal_with(&key, "h/10", B);
        assert_eq!(open_with(&[other], &t, B), Err(OpenError::Rejected));
        // After a rotation the previous key still opens it.
        assert_eq!(open_with(&[other, key], &t, B).as_deref(), Ok("h/10"));
        for binding in [
            TokenBinding { bucket: "c", ..B },
            TokenBinding { prefix: "p", ..B },
            TokenBinding { prefix: "", ..B },
            TokenBinding {
                delimiter: None,
                ..B
            },
            TokenBinding {
                delimiter: Some("-"),
                ..B
            },
        ] {
            assert_eq!(
                open_with(&[key], &t, binding),
                Err(OpenError::Rejected),
                "{binding:?}"
            );
        }
        // An empty delimiter is no delimiter.
        let none = TokenBinding {
            delimiter: None,
            ..B
        };
        let t = seal_with(&key, "h/10", none);
        let empty = TokenBinding {
            delimiter: Some(""),
            ..B
        };
        assert_eq!(open_with(&[key], &t, empty).as_deref(), Ok("h/10"));
    }

    /// The installed keys: the primary seals; a rotation's previous key and
    /// a replaced key file still open; the legacy bootstrap hash is not one.
    #[test]
    fn the_token_keys_follow_the_config_db_keys() {
        use crate::config_db::key::FallbackKind;
        let keys = crate::config_db::ConfigDbKeys::primary_only(&"p".repeat(40))
            .with_fallback(FallbackKind::PreviousKey, &"q".repeat(40))
            .with_fallback(FallbackKind::LegacyBootstrapHash, "hash")
            .with_fallback(FallbackKind::KeyFile, &"f".repeat(40));
        assert_eq!(
            token_keys_for(&keys),
            vec![
                derive_list_token_key(&"p".repeat(40)),
                derive_list_token_key(&"q".repeat(40)),
                derive_list_token_key(&"f".repeat(40)),
            ]
        );
    }
}
