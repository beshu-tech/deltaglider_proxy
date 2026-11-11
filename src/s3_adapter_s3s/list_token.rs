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
//!
//! A page that cannot finish the visibility proof of a folded name also
//! seals the proof's progress (the name and its inner cursor, both hidden
//! data), so the next page resumes the proof. The inner cursor is sealed
//! with the position and bound to the same listing, so a client cannot
//! forge or move it.
//!
//! Both kinds of token start with `dg2.`. The kind is part of the
//! authenticated associated data (`+proof`), not of the prefix. An earlier
//! release knows only `dg2.` tokens without a proof: it opens a proof token
//! with the plain associated data, fails, and answers `InvalidArgument`. A
//! new prefix would be read there as a raw start key (a V2 token) or a plain
//! marker (V1), and the listing would go on from a meaningless key.
//!
//! The plaintext of a proof token has a fixed size: the position, the name
//! and the inner cursor can each name a hidden key, and the length of a
//! sealed message shows the length of its plaintext. Each field is padded to
//! the longest S3 key ([`PROOF_FIELD_LEN`]).

use crate::iam::listing::ProofResume;
use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Marks a sealed token, with or without a proof. A `dg1.` token carries a
/// visible key in clear.
pub(super) const SEALED_TOKEN_PREFIX: &str = "dg2.";

/// The size of each secret field of a proof token: the longest S3 key, in
/// bytes. A longer field (not possible for a key) is kept whole, not cut.
const PROOF_FIELD_LEN: usize = 1024;

/// What a sealed token holds: a scan position, and the proof to go on with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SealedCursor {
    pub position: String,
    pub proof: Option<ProofResume>,
}

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
    fn aad(&self, proof: bool) -> Vec<u8> {
        let mut out = KEY_LABEL.to_vec();
        if proof {
            // A proof payload never opens as a position, nor the reverse,
            // and an earlier release (plain associated data only) refuses it.
            out.extend_from_slice(b"+proof");
        }
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

/// Pure: the plaintext of a proof token, of one size for every input: a
/// flag byte (inner cursor present), then the position, the name and the
/// inner cursor, each as a length, the bytes and zeros up to
/// [`PROOF_FIELD_LEN`].
fn proof_payload(position: &str, proof: &ProofResume) -> Vec<u8> {
    let mut out = vec![u8::from(proof.inner.is_some())];
    for field in [
        position,
        proof.name.as_str(),
        proof.inner.as_deref().unwrap_or(""),
    ] {
        out.extend_from_slice(&(field.len() as u32).to_be_bytes());
        out.extend_from_slice(field.as_bytes());
        out.resize(out.len() + PROOF_FIELD_LEN.saturating_sub(field.len()), 0);
    }
    out
}

/// Pure: read what [`proof_payload`] wrote; `None` for any other shape.
fn parse_proof_payload(bytes: &[u8]) -> Option<SealedCursor> {
    let (&flag, mut rest) = bytes.split_first()?;
    let mut take = || -> Option<String> {
        let len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let field = rest.get(4..4 + len)?;
        let padding = rest.get(4 + len..4 + len + PROOF_FIELD_LEN.saturating_sub(len))?;
        if padding.iter().any(|b| *b != 0) {
            return None;
        }
        rest = &rest[4 + len + padding.len()..];
        String::from_utf8(field.to_vec()).ok()
    };
    let position = take()?;
    let name = take()?;
    let inner = take()?;
    let inner = match flag {
        1 => Some(inner),
        0 if inner.is_empty() => None,
        _ => return None,
    };
    rest.is_empty().then_some(SealedCursor {
        position,
        proof: Some(ProofResume { name, inner }),
    })
}

/// Pure: seal the scan position `position` with `key` (random nonce).
#[cfg(test)]
pub(super) fn seal_with(key: &TokenKey, position: &str, binding: TokenBinding<'_>) -> String {
    seal_cursor_with(
        key,
        &SealedCursor {
            position: position.to_string(),
            proof: None,
        },
        binding,
    )
}

/// Pure: seal a scan position and the proof progress that goes with it.
pub(super) fn seal_cursor_with(
    key: &TokenKey,
    cursor: &SealedCursor,
    binding: TokenBinding<'_>,
) -> String {
    use base64::Engine as _;
    use rand::RngCore;
    let mut nonce = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let cipher = Aes256Gcm::new(key.into());
    let aad = binding.aad(cursor.proof.is_some());
    let plain = match &cursor.proof {
        Some(proof) => proof_payload(&cursor.position, proof),
        None => cursor.position.as_bytes().to_vec(),
    };
    let sealed = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &plain,
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

/// Pure: the envelope bytes of a sealed token, when it has the shape of
/// one (`dg2.`, base64url, room for a nonce and a tag).
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

/// Pure: open a sealed token with the first of `keys` that opens it. The
/// kind is the associated data that opens it: a position, else a proof.
pub(super) fn open_with(
    keys: &[TokenKey],
    token: &str,
    binding: TokenBinding<'_>,
) -> Result<SealedCursor, OpenError> {
    let blob = envelope(token).ok_or(OpenError::Malformed)?;
    let (nonce, sealed) = blob.split_at(NONCE_LEN);
    let decrypt = |proof: bool| {
        let aad = binding.aad(proof);
        keys.iter().find_map(|key| {
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
    };
    if let Some(plain) = decrypt(false) {
        return String::from_utf8(plain)
            .map(|position| SealedCursor {
                position,
                proof: None,
            })
            .map_err(|_| OpenError::Rejected);
    }
    let plain = decrypt(true).ok_or(OpenError::Rejected)?;
    parse_proof_payload(&plain).ok_or(OpenError::Rejected)
}

/// Seal a scan position, and the proof that goes with it, with the
/// installed key.
pub(super) fn seal(
    position: &str,
    proof: Option<&ProofResume>,
    binding: TokenBinding<'_>,
) -> String {
    seal_cursor_with(
        &token_keys()[0],
        &SealedCursor {
            position: position.to_string(),
            proof: proof.cloned(),
        },
        binding,
    )
}

/// Open a sealed token with the installed keys. A token that does not open
/// is `InvalidArgument`: the listing never restarts somewhere else, because
/// two nodes with different keys would then answer the same page forever.
pub(super) fn open(token: &str, binding: TokenBinding<'_>) -> s3s::S3Result<SealedCursor> {
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
            assert_eq!(
                open_with(&[key], &t, B).map(|c| c.position).as_deref(),
                Ok(position)
            );
        }
        // Random nonce: one position seals to two different tokens.
        assert_ne!(seal_with(&key, "h/10", B), seal_with(&key, "h/10", B));
    }

    #[test]
    fn a_proof_payload_round_trips_and_refuses_other_shapes() {
        let key = derive_list_token_key(&"k".repeat(64));
        for (position, name, inner) in [
            ("e.secret", "h/", Some("h/0999.pem")),
            ("", "a/..", None),
            ("é<&>\u{1}", "ü/", Some("")),
        ] {
            let cursor = SealedCursor {
                position: position.into(),
                proof: Some(ProofResume {
                    name: name.into(),
                    inner: inner.map(String::from),
                }),
            };
            let t = seal_cursor_with(&key, &cursor, B);
            assert!(t.starts_with(SEALED_TOKEN_PREFIX));
            assert_eq!(open_with(&[key], &t, B), Ok(cursor));
        }
        for bad in [
            &b""[..],
            &[0, 0, 0, 9, 1][..],
            &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2][..],
        ] {
            assert_eq!(parse_proof_payload(bad), None);
        }
        let mut trailing = proof_payload(
            "p",
            &ProofResume {
                name: "n".into(),
                inner: None,
            },
        );
        trailing.push(0);
        assert_eq!(parse_proof_payload(&trailing), None);
    }

    /// The length of a proof token does not depend on the folded name, the
    /// inner cursor or the position (all can name hidden keys), nor on
    /// whether the inner cursor is set.
    #[test]
    fn a_proof_token_has_one_length() {
        let key = derive_list_token_key(&"k".repeat(64));
        let token = |position: &str, name: &str, inner: Option<&str>| {
            seal_cursor_with(
                &key,
                &SealedCursor {
                    position: position.into(),
                    proof: Some(ProofResume {
                        name: name.into(),
                        inner: inner.map(String::from),
                    }),
                },
                B,
            )
        };
        let reference = token("e", "h/", Some("h/0"));
        // About 4.2 KB: far under the 8 KB request-line limit of common proxies.
        assert!(reference.len() < 4300, "{}", reference.len());
        for (position, name, inner) in [
            ("e.secret", "hidden-folder/", Some("hidden-folder/0999.pem")),
            ("", "a/..", None),
            ("é<&>", "ü/", Some("")),
            (
                &"p".repeat(1024),
                &"n".repeat(1024),
                Some(&"i".repeat(1024)[..]),
            ),
        ] {
            assert_eq!(
                token(position, name, inner).len(),
                reference.len(),
                "{position:?} {name:?} {inner:?}"
            );
        }
    }

    /// The kind of a token is authenticated data: a proof payload sealed as
    /// a position never reads as a proof, and a position sealed as a proof
    /// is refused. An older release, which opens with the plain data only,
    /// therefore refuses every proof token.
    #[test]
    fn the_kind_is_part_of_the_authenticated_data() {
        use base64::Engine as _;
        let key = derive_list_token_key(&"k".repeat(64));
        let raw = |plain: &[u8], proof: bool| {
            let nonce = [9u8; NONCE_LEN];
            let aad = B.aad(proof);
            let sealed = Aes256Gcm::new((&key).into())
                .encrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: plain,
                        aad: &aad,
                    },
                )
                .unwrap();
            let mut blob = nonce.to_vec();
            blob.extend_from_slice(&sealed);
            format!(
                "{SEALED_TOKEN_PREFIX}{}",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(blob)
            )
        };
        let payload = proof_payload(
            "p",
            &ProofResume {
                name: "n".into(),
                inner: None,
            },
        );
        // Under the proof data a payload is a proof; under the plain data it
        // is only a position string.
        let as_proof = open_with(&[key], &raw(&payload, true), B).unwrap();
        assert!(as_proof.proof.is_some());
        let as_position = open_with(&[key], &raw(&payload, false), B).unwrap();
        assert!(as_position.proof.is_none());
        // A position under the proof data is not a proof payload.
        assert_eq!(
            open_with(&[key], &raw(b"h/10", true), B),
            Err(OpenError::Rejected)
        );
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
        assert_eq!(
            open_with(&[other, key], &t, B)
                .map(|c| c.position)
                .as_deref(),
            Ok("h/10")
        );
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
        assert_eq!(
            open_with(&[key], &t, empty).map(|c| c.position).as_deref(),
            Ok("h/10")
        );
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
