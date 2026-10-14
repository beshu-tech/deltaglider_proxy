// SPDX-License-Identifier: BUSL-1.1

//! THE key resolution of an encryption config: parse its key and its
//! legacy key, and give each the id that the engine stamps on the objects
//! it writes. The engine wrapper, the config rules (collisions, apply
//! gates, notes), the re-encrypt target, the preserve-on-apply step and
//! the backend summaries all call [`resolve_site_keys`], so they cannot
//! disagree on an id or on which keys exist (issue #116 review G).

use super::*;
use crate::storage::EncryptionKey;

/// Whose id rules apply to a config. A backend derives a missing id from
/// its name (`derive_key_id`); a bucket never derives one: its ids are
/// explicit, because a name can change (rename, alias).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyScope<'a> {
    Backend(&'a str),
    Bucket(&'a str),
}

impl KeyScope<'_> {
    fn label(self) -> String {
        match self {
            Self::Backend(n) => format!("backend '{n}'"),
            Self::Bucket(n) => format!("bucket '{n}'"),
        }
    }
}

/// One parsed key with its id. No `Debug`: the key must never reach a
/// log line (EncryptionKey zeroizes on drop).
#[derive(Clone)]
pub(crate) struct SiteKey {
    pub key_id: String,
    pub key: EncryptionKey,
}

/// The two key slots of one encryption config. `None`: the slot is
/// empty (for the primary: not `aes256-gcm-proxy`, or no key). `Err`: the
/// key does not parse, or a bucket key has no explicit id.
pub(crate) struct SiteKeys {
    pub primary: Option<Result<SiteKey, String>>,
    pub legacy: Option<Result<SiteKey, String>>,
}

impl SiteKeys {
    pub(crate) fn primary_id(&self) -> Option<&str> {
        match &self.primary {
            Some(Ok(k)) => Some(&k.key_id),
            _ => None,
        }
    }

    pub(crate) fn legacy_id(&self) -> Option<&str> {
        match &self.legacy {
            Some(Ok(k)) => Some(&k.key_id),
            _ => None,
        }
    }
}

/// THE resolver. See the module doc.
pub(crate) fn resolve_site_keys(scope: KeyScope<'_>, enc: &BackendEncryptionConfig) -> SiteKeys {
    let slot = |hex: &str, explicit: Option<&str>, legacy: bool| -> Result<SiteKey, String> {
        let field = if legacy { "legacy_key" } else { "key" };
        let key = EncryptionKey::from_hex(hex)
            .map_err(|e| format!("{} encryption.{field}: {e}", scope.label()))?;
        let key_id = match (explicit, scope) {
            (Some(id), _) => id.to_string(),
            (None, KeyScope::Backend(name)) if legacy => {
                crate::deltaglider::derive_key_id(&format!("{name}::legacy"), &key.0)
            }
            (None, KeyScope::Backend(name)) => crate::deltaglider::derive_key_id(name, &key.0),
            (None, KeyScope::Bucket(_)) => {
                return Err(format!(
                    "{}: encryption.{field}_id is required on a bucket",
                    scope.label()
                ))
            }
        };
        Ok(SiteKey { key_id, key })
    };
    let primary = match enc {
        BackendEncryptionConfig::Aes256GcmProxy {
            key: Some(hex),
            key_id,
            ..
        } => Some(slot(hex, key_id.as_deref(), false)),
        _ => None,
    };
    let legacy = enc
        .legacy_key()
        .map(|hex| slot(hex, enc.legacy_key_id(), true));
    SiteKeys { primary, legacy }
}

/// One key that a config site holds, as the engine resolves it. No
/// `Debug` (see [`SiteKey`]).
#[derive(Clone)]
pub(crate) struct ResolvedKey {
    pub site: EncryptionSite,
    pub key_id: String,
    pub key: EncryptionKey,
}

/// Every key of a config, and the slots that do not resolve.
pub(crate) struct KeyResolution {
    pub keys: Vec<ResolvedKey>,
    pub errors: Vec<String>,
}

impl Config {
    /// The key-id scope of an encryption site.
    pub(crate) fn key_scope<'a>(&self, site: &'a EncryptionSite) -> KeyScope<'a> {
        match site {
            EncryptionSite::Singleton => KeyScope::Backend("default"),
            EncryptionSite::Backend(n) => KeyScope::Backend(n),
            EncryptionSite::Bucket(n) => KeyScope::Bucket(n),
        }
    }

    /// Every key the config holds (through [`resolve_site_keys`]). The singleton counts only while it is the engine's
    /// backend (no named backends); a bucket counts when the config fixes
    /// its storage. The engine builds its keyrings and runs its collision
    /// check on exactly these keys.
    pub(crate) fn resolved_keys(&self) -> KeyResolution {
        let mut out = KeyResolution {
            keys: Vec::new(),
            errors: Vec::new(),
        };
        for (site, enc) in self.encryption_sites() {
            if site == EncryptionSite::Singleton && !self.backends.is_empty() {
                continue;
            }
            if let EncryptionSite::Bucket(n) = &site {
                if self.bucket_storage(n).is_none() {
                    continue;
                }
            }
            let keys = resolve_site_keys(self.key_scope(&site), enc);
            for slot in [keys.primary, keys.legacy] {
                match slot {
                    Some(Ok(k)) => out.keys.push(ResolvedKey {
                        site: site.clone(),
                        key_id: k.key_id,
                        key: k.key,
                    }),
                    Some(Err(e)) => out.errors.push(e),
                    None => {}
                }
            }
        }
        out
    }
}
