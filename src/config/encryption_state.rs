// SPDX-License-Identifier: BUSL-1.1

//! The write encryption of a bucket as a value (issue #116 P3): what the
//! audit, the downgrade gate and the multi-instance witness compare.
//!
//! * [`WriteEncryption`]: the mode and the PUBLIC id of the key that new
//!   writes use. Never key material, never a value derived from a key
//!   beyond the key id that every object already carries.
//! * [`Config::encryption_changes`]: every subject (a configured bucket,
//!   or a backend for the buckets the config does not name) whose write
//!   encryption an apply changes. THE one list: the transition audits it
//!   and the downgrade gate judges it.
//! * [`Config::unconfirmed_encryption_downgrades`]: the gate. A change that
//!   makes a subject store plaintext where it encrypted needs the
//!   subject in the request's confirmation.

use super::*;

/// The mode and the public key id of a write encryption.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct WriteEncryption {
    /// The YAML `mode:` tag.
    pub mode: String,
    /// The proxy key id (`aes256-gcm-proxy`) or the `kms_key_id`
    /// (`sse-kms`); `None` for `none`, `sse-s3`, and a proxy mode with no
    /// key (every write is refused then).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    /// The key check value of a proxy key ([`key_check_value`]): two
    /// configs with one key id and different key bytes differ here. Since
    /// format v2 (issue #120) it is the key's v2 fingerprint, so a node of
    /// an older release (whose value differs) disagrees and refuses writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kcv: Option<String>,
}

/// The key check value of a proxy key: its v2 fingerprint in hex (8
/// bytes of an HKDF expansion of the key). It tells two keys apart and
/// reveals nothing practical about a random 256-bit key. Every v2 object
/// carries the same value, so the witness and the objects agree.
pub fn key_check_value(key: &crate::storage::EncryptionKey) -> String {
    hex::encode(crate::storage::encrypting::v2::SiteKey::new(key).fingerprint())
}

impl WriteEncryption {
    /// The write encryption of `enc`, whose keys resolve to `keys` (see
    /// `resolve_site_keys`).
    pub(crate) fn of(enc: &BackendEncryptionConfig, keys: &SiteKeys) -> Self {
        let primary = match &keys.primary {
            Some(Ok(k)) => Some(k),
            _ => None,
        };
        let (key_id, kcv) = match enc {
            BackendEncryptionConfig::Aes256GcmProxy { .. } => (
                primary.map(|k| k.key_id.clone()),
                primary.map(|k| key_check_value(&k.key)),
            ),
            BackendEncryptionConfig::SseKms { kms_key_id, .. } => (Some(kms_key_id.clone()), None),
            BackendEncryptionConfig::None { .. } | BackendEncryptionConfig::SseS3 { .. } => {
                (None, None)
            }
        };
        Self {
            mode: enc.mode_tag().to_string(),
            key_id,
            kcv,
        }
    }

    /// A proxy mode with no usable key: every write is refused.
    pub fn refuses_writes(&self) -> bool {
        self.mode == "aes256-gcm-proxy" && self.key_id.is_none()
    }

    /// False only for plaintext writes (`mode: none`). A proxy mode with no
    /// key counts as encrypting: its writes are refused, never plaintext.
    pub fn encrypts(&self) -> bool {
        self.mode != "none"
    }

    /// `mode (key id 'x')`, for messages and audit targets.
    pub fn describe(&self) -> String {
        match &self.key_id {
            Some(id) => format!("{} (key id '{id}')", self.mode),
            None => self.mode.clone(),
        }
    }
}

/// How a subject's write encryption changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptionChangeKind {
    /// Plaintext → encrypting, or one encrypting mode → another.
    Set,
    /// Encrypting → plaintext: a downgrade.
    Cleared,
    /// Same mode, another key id.
    KeyRotated,
}

impl EncryptionChangeKind {
    /// The audit action of this change.
    pub fn audit_action(self) -> &'static str {
        match self {
            Self::Set => "bucket_encryption_set",
            Self::Cleared => "bucket_encryption_cleared",
            Self::KeyRotated => "bucket_encryption_key_rotated",
        }
    }
}

/// The subject of an encryption change: what a confirmation header names.
/// Its [`Display`](std::fmt::Display) form is the wire string (the
/// confirmation header values, the error and audit texts).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Subject {
    /// Every bucket on this backend that the config does not name:
    /// `backend:<name>` (S3 bucket names never hold `:`, so it never
    /// collides with a bucket).
    Backend(String),
    /// The buckets that no config names and that follow the default route:
    /// [`DEFAULT_ROUTE_SUBJECT`].
    DefaultRoute,
    /// A configured bucket: its name.
    Bucket(String),
}

impl Subject {
    /// The bucket, for a bucket subject.
    pub fn bucket(&self) -> Option<&str> {
        match self {
            Self::Bucket(b) => Some(b),
            _ => None,
        }
    }
}

impl std::fmt::Display for Subject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(name) => write!(f, "backend:{name}"),
            Self::DefaultRoute => f.write_str(DEFAULT_ROUTE_SUBJECT),
            Self::Bucket(name) => f.write_str(name),
        }
    }
}

/// One subject whose write encryption a config change changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptionChange {
    /// The confirmation token of a gated change.
    pub subject: Subject,
    pub before: WriteEncryption,
    pub after: WriteEncryption,
    pub kind: EncryptionChangeKind,
    /// The new config serves the plaintext history
    /// (`plaintext_reads: serve`).
    pub serves_plaintext: bool,
    /// A bucket subject that inherits the encryption of one backend in
    /// both configs: that backend. The backend's own subject covers it.
    pub follows_backend: Option<String>,
}

impl EncryptionChange {
    /// The audit target: `<subject>: <before> -> <after>`. Key ids only.
    pub fn audit_target(&self) -> String {
        format!(
            "{}: {} -> {}",
            self.subject,
            self.before.describe(),
            self.after.describe()
        )
    }

    /// True when `confirmed` names this change's subject.
    fn confirmed_by(&self, confirmed: &[String]) -> bool {
        confirmed.contains(&self.subject.to_string())
    }

    /// True when a backend change in `changes` that `covers` accepts covers
    /// this bucket change (see [`Self::follows_backend`]).
    fn covered_by(
        &self,
        changes: &[EncryptionChange],
        covers: impl Fn(&EncryptionChange) -> bool,
    ) -> bool {
        let Some(backend) = &self.follows_backend else {
            return false;
        };
        changes
            .iter()
            .any(|c| matches!(&c.subject, Subject::Backend(n) if n == backend) && covers(c))
    }
}

/// The subject of the buckets that no config names and that follow the
/// default route (`default_backend`).
pub const DEFAULT_ROUTE_SUBJECT: &str = "route:default";

/// The request header that confirms a downgrade: subjects, comma-separated.
/// `config apply --confirm-encryption-downgrade <subject>` sends it.
pub const CONFIRM_ENCRYPTION_DOWNGRADE_HEADER: &str = "x-dgp-confirm-encryption-downgrade";

/// The request header that confirms that a subject which starts to encrypt
/// may refuse its plaintext history (`plaintext_reads: refuse`): subjects,
/// comma-separated. `config apply --confirm-plaintext-refusal <subject>`.
pub const CONFIRM_PLAINTEXT_REFUSAL_HEADER: &str = "x-dgp-confirm-plaintext-refusal";

/// Pure: the subjects that `headers` confirm in `header`.
pub(crate) fn confirmed_subjects(
    headers: Option<&axum::http::HeaderMap>,
    header: &str,
) -> Vec<String> {
    headers
        .into_iter()
        .flat_map(|h| h.get_all(header))
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Pure: the downgrade subjects that `headers` confirm.
pub(crate) fn confirmed_downgrades(headers: Option<&axum::http::HeaderMap>) -> Vec<String> {
    confirmed_subjects(headers, CONFIRM_ENCRYPTION_DOWNGRADE_HEADER)
}

/// The encryption of one backend's own site.
struct BackendSite {
    write: WriteEncryption,
    serves_plaintext: bool,
}

/// Pure: the kind of a change from `before` to `after`, `None` when equal.
fn change_kind(before: &WriteEncryption, after: &WriteEncryption) -> Option<EncryptionChangeKind> {
    if before == after {
        None
    } else if before.encrypts() && !after.encrypts() {
        Some(EncryptionChangeKind::Cleared)
    } else if before.mode == after.mode {
        Some(EncryptionChangeKind::KeyRotated)
    } else {
        Some(EncryptionChangeKind::Set)
    }
}

impl Config {
    /// The buckets of `listed` whose reads refuse an unencrypted object:
    /// they encrypt with `aes256-gcm-proxy` and do not set
    /// `plaintext_reads: serve`. The boot check samples them.
    pub fn buckets_refusing_plaintext<'a>(&self, listed: &'a [String]) -> Vec<&'a str> {
        listed
            .iter()
            .filter(|b| {
                self.encryption_for_bucket(b).is_some_and(|e| {
                    matches!(e.config, BackendEncryptionConfig::Aes256GcmProxy { .. })
                        && !e.config.serves_plaintext()
                })
            })
            .map(String::as_str)
            .collect()
    }

    /// The write encryption of a configured bucket name, or `None` when it
    /// routes to an undefined backend.
    pub fn write_encryption_for_bucket(&self, bucket: &str) -> Option<WriteEncryption> {
        self.encryption_for_bucket(bucket)
            .map(|e| e.write_encryption())
    }

    /// The encryption of each backend's own site (what a bucket that the
    /// config does not name writes with), by backend name.
    fn backend_sites(&self) -> std::collections::BTreeMap<String, BackendSite> {
        self.encryption_sites()
            .into_iter()
            .filter_map(|(site, enc)| {
                let name = match &site {
                    // The singleton is the storage only without named backends.
                    EncryptionSite::Singleton if self.backends.is_empty() => "default".to_string(),
                    EncryptionSite::Backend(name) => name.clone(),
                    _ => return None,
                };
                let keys = resolve_site_keys(KeyScope::Backend(&name), enc);
                let site = BackendSite {
                    write: WriteEncryption::of(enc, &keys),
                    serves_plaintext: enc.serves_plaintext(),
                };
                Some((name, site))
            })
            .collect()
    }

    /// Every subject whose write encryption changes from `old` to `self`:
    /// each backend that both configs define (for the buckets the config
    /// does not name), then each bucket that either config names (its
    /// override, its route and its backend all count). Migrate's transient
    /// routes are internal and skipped. THE list that the transition
    /// audits and the gates judge.
    pub fn encryption_changes(&self, old: &Config) -> Vec<EncryptionChange> {
        let mut out = Vec::new();
        let (before, after) = (old.backend_sites(), self.backend_sites());
        for (name, b) in &before {
            let Some(a) = after.get(name) else { continue };
            if let Some(kind) = change_kind(&b.write, &a.write) {
                out.push(EncryptionChange {
                    subject: Subject::Backend(name.clone()),
                    before: b.write.clone(),
                    after: a.write.clone(),
                    kind,
                    serves_plaintext: a.serves_plaintext,
                    follows_backend: None,
                });
            }
        }
        // The buckets that no config names follow the default route: a new
        // default backend (or the move from the singleton to named
        // backends) changes their encryption with no backend subject.
        let (od, nd) = (old.default_backend_name(), self.default_backend_name());
        if od != nd {
            if let (Some(b), Some(a)) = (before.get(&od), after.get(&nd)) {
                if let Some(kind) = change_kind(&b.write, &a.write) {
                    out.push(EncryptionChange {
                        subject: Subject::DefaultRoute,
                        before: b.write.clone(),
                        after: a.write.clone(),
                        kind,
                        serves_plaintext: a.serves_plaintext,
                        follows_backend: None,
                    });
                }
            }
        }
        let names: std::collections::BTreeSet<&String> =
            self.buckets.keys().chain(old.buckets.keys()).collect();
        for name in names {
            if name.starts_with(crate::maintenance::migrate::TRANSIENT_PREFIX) {
                continue;
            }
            let (Some(b), Some(a)) = (
                old.encryption_for_bucket(name),
                self.encryption_for_bucket(name),
            ) else {
                continue;
            };
            let (wb, wa) = (b.write_encryption(), a.write_encryption());
            if let Some(kind) = change_kind(&wb, &wa) {
                let follows_backend = (!b.from_bucket && !a.from_bucket && a.backend == b.backend)
                    .then(|| a.backend.clone());
                out.push(EncryptionChange {
                    subject: Subject::Bucket(name.clone()),
                    before: wb,
                    after: wa,
                    kind,
                    serves_plaintext: a.config.serves_plaintext(),
                    follows_backend,
                });
            }
        }
        out
    }

    /// The plaintext-refusal gate (issue #120): one error per subject that
    /// starts to encrypt with proxy AES and `plaintext_reads: refuse`. Its
    /// objects written before are plaintext, and the read rule then refuses
    /// them. The change needs `plaintext_reads: serve` (and a re-encrypt job
    /// to convert the history), or the subject in `confirmed` (the bucket
    /// holds no plaintext that anyone needs). A bucket in `absent` (no
    /// backend lists it yet) has no history. A bucket that follows one
    /// backend in both configs is covered by the backend's subject; a
    /// bucket that starts to inherit an encrypting backend is its own
    /// subject. `None` = not gated.
    pub fn unconfirmed_plaintext_refusals(
        &self,
        old: &Config,
        confirmed: Option<&[String]>,
        absent: &[String],
    ) -> Vec<String> {
        let Some(confirmed) = confirmed else {
            return Vec::new();
        };
        let starts_to_refuse = |c: &EncryptionChange| {
            !c.before.encrypts()
                && c.after.mode == "aes256-gcm-proxy"
                && !c.after.refuses_writes()
                && !c.serves_plaintext
        };
        let changes = self.encryption_changes(old);
        let mut errors = Vec::new();
        for c in changes.iter().filter(|c| starts_to_refuse(c)) {
            if c.confirmed_by(confirmed) || c.covered_by(&changes, starts_to_refuse) {
                continue;
            }
            let what = match &c.subject {
                Subject::Bucket(b) => {
                    if absent.iter().any(|a| a.eq_ignore_ascii_case(b)) {
                        continue;
                    }
                    format!("bucket '{b}'")
                }
                Subject::DefaultRoute => "every bucket that the config does not name".to_string(),
                Subject::Backend(name) => {
                    format!("every bucket on backend '{name}' without its own encryption block")
                }
            };
            errors.push(format!(
                "this change makes {what} encrypt, but the objects stored there before are \
                 plaintext, and with `plaintext_reads: refuse` (the default) the proxy refuses \
                 to read them. Set encryption.plaintext_reads: serve and run a re-encrypt job \
                 (POST /_/api/admin/jobs/reencrypt) to convert them, then set it back. If the \
                 subject holds no plaintext that anyone needs, confirm with the header \
                 `X-DGP-Confirm-Plaintext-Refusal: {}` (`config apply \
                 --confirm-plaintext-refusal {}`)",
                c.subject, c.subject
            ));
        }
        errors
    }

    /// The downgrade gate: one error per change from `old` that makes a
    /// subject store plaintext where it encrypted, unless `confirmed` names
    /// the subject. A bucket that inherits its backend's encryption in both
    /// configs, on the same backend, is covered by that backend's subject.
    /// `None` = not gated (a background change: the migrate start
    /// confirmed its own downgrade).
    pub fn unconfirmed_encryption_downgrades(
        &self,
        old: &Config,
        confirmed: Option<&[String]>,
    ) -> Vec<String> {
        let Some(confirmed) = confirmed else {
            return Vec::new();
        };
        let cleared = |c: &EncryptionChange| c.kind == EncryptionChangeKind::Cleared;
        let changes = self.encryption_changes(old);
        let mut errors = Vec::new();
        for c in changes.iter().filter(|c| cleared(c)) {
            if c.confirmed_by(confirmed) || c.covered_by(&changes, cleared) {
                continue;
            }
            let what = match &c.subject {
                Subject::Bucket(b) => format!("bucket '{b}'"),
                Subject::DefaultRoute => {
                    "every bucket that the config does not name (they follow the default \
                     backend)"
                        .to_string()
                }
                Subject::Backend(name) => {
                    format!("every bucket on backend '{name}' without its own encryption block")
                }
            };
            errors.push(format!(
                "this change makes {what} store new objects in plaintext (it encrypted with \
                 {} before). A downgrade needs an explicit confirmation: send the change again \
                 with the header `X-DGP-Confirm-Encryption-Downgrade: {}` (`config apply \
                 --confirm-encryption-downgrade {}`). Objects stored before keep their \
                 encryption",
                c.before.describe(),
                c.subject,
                c.subject
            ));
        }
        errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KA: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const KB: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    fn cfg(local_enc: &str, buckets: &str) -> Config {
        Config::from_yaml_str(&format!(
            "storage:\n  backends:\n    - name: local-disk\n      type: filesystem\n      path: /tmp/dgp-enc-state-a\n{local_enc}    - name: hetzner-fsn1\n      type: filesystem\n      path: /tmp/dgp-enc-state-b\n  default_backend: local-disk\n  buckets:\n{buckets}"
        ))
        .unwrap()
    }

    trait WithDefault {
        fn with_default(self, name: &str) -> Config;
    }
    impl WithDefault for Config {
        fn with_default(mut self, name: &str) -> Config {
            self.default_backend = Some(name.to_string());
            self
        }
    }

    fn aes(key: &str, id: &str) -> String {
        format!("{{ mode: aes256-gcm-proxy, key: \"{key}\", key_id: {id} }}")
    }

    fn local_aes(key: &str, id: &str) -> String {
        format!("      encryption: {}\n", aes(key, id))
    }

    /// The boot check samples exactly the buckets that refuse plaintext:
    /// proxy AES without `serve`, inherited or their own.
    #[test]
    fn buckets_refusing_plaintext_are_the_proxy_aes_ones_without_serve() {
        let c = cfg(
            &local_aes(KA, "ka"),
            &format!(
                "    releases: {{ backend: local-disk }}\n    db-archive: {{ backend: local-disk, encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: kb, plaintext_reads: serve }} }}\n    downloads: {{ backend: hetzner-fsn1 }}\n"
            ),
        );
        let listed: Vec<String> = ["releases", "db-archive", "downloads", "unnamed"]
            .map(String::from)
            .to_vec();
        // `unnamed` is on the default backend, local-disk, which encrypts.
        assert_eq!(
            c.buckets_refusing_plaintext(&listed),
            ["releases", "unnamed"]
        );
    }

    #[test]
    fn changes_are_set_cleared_or_rotated() {
        let none = cfg("", "    releases: { backend: local-disk }\n");
        let a = cfg(
            "",
            &format!(
                "    releases: {{ backend: local-disk, encryption: {} }}\n",
                aes(KA, "rel-a")
            ),
        );
        let b = cfg(
            "",
            &format!(
                "    releases: {{ backend: local-disk, encryption: {} }}\n",
                aes(KB, "rel-b")
            ),
        );
        let kinds = |new: &Config, old: &Config| {
            new.encryption_changes(old)
                .into_iter()
                .map(|c| (c.subject.to_string(), c.kind))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            kinds(&a, &none),
            vec![("releases".into(), EncryptionChangeKind::Set)]
        );
        assert_eq!(
            kinds(&b, &a),
            vec![("releases".into(), EncryptionChangeKind::KeyRotated)]
        );
        assert_eq!(
            kinds(&none, &a),
            vec![("releases".into(), EncryptionChangeKind::Cleared)]
        );
        assert!(kinds(&a, &a).is_empty());
        let c = &a.encryption_changes(&none)[0];
        assert_eq!(
            c.audit_target(),
            "releases: none -> aes256-gcm-proxy (key id 'rel-a')"
        );
        assert!(!c.audit_target().contains(KA));
    }

    /// Every downgrade path: `mode: none`, the override removed onto a
    /// plaintext backend, a route to a plaintext backend, and the backend's
    /// own downgrade (one confirmation covers its inheriting buckets).
    #[test]
    fn every_downgrade_needs_its_confirmation() {
        let over = format!(
            "    releases: {{ backend: local-disk, encryption: {} }}\n",
            aes(KA, "rel-a")
        );
        let old = cfg("", &over);
        let gate = |new: &Config, old: &Config, ok: &[&str]| {
            let ok: Vec<String> = ok.iter().map(|s| s.to_string()).collect();
            new.unconfirmed_encryption_downgrades(old, Some(&ok))
        };
        // mode: none.
        let none = cfg(
            "",
            "    releases: { backend: local-disk, encryption: { mode: none, legacy_key: \"1111111111111111111111111111111111111111111111111111111111111111\", legacy_key_id: rel-a } }\n",
        );
        let errs = gate(&none, &old, &[]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("X-DGP-Confirm-Encryption-Downgrade: releases"));
        assert!(gate(&none, &old, &["releases"]).is_empty());
        assert!(gate(&none, &old, &["downloads"]).len() == 1);
        // Override removed: the bucket falls back to plaintext local-disk.
        let removed = cfg("", "    releases: { backend: local-disk }\n");
        assert_eq!(gate(&removed, &old, &[]).len(), 1);
        // A route to a plaintext backend, from an encrypting one.
        let enc_backend = cfg(
            &local_aes(KA, "local-a"),
            "    releases: { backend: local-disk }\n",
        );
        let moved = cfg(
            &local_aes(KA, "local-a"),
            "    releases: { backend: hetzner-fsn1 }\n",
        );
        assert_eq!(gate(&moved, &enc_backend, &[]).len(), 1);
        // The backend's own downgrade: `backend:local-disk` covers releases.
        let plain_backend = cfg("", "    releases: { backend: local-disk }\n");
        let errs = gate(&plain_backend, &enc_backend, &[]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("backend:local-disk"), "{errs:?}");
        assert!(gate(&plain_backend, &enc_backend, &["backend:local-disk"]).is_empty());
        // An upgrade and a rotation need nothing.
        assert!(gate(&old, &removed, &[]).is_empty());
        // A background change is not gated here.
        assert!(none
            .unconfirmed_encryption_downgrades(&old, None)
            .is_empty());
    }

    /// Review of P3: the buckets that no config names had no subject, so a
    /// new default backend or the move from an encrypted singleton to
    /// plaintext named backends passed with no change and no gate.
    #[test]
    fn the_default_route_is_a_subject() {
        let enc_default = cfg(
            &local_aes(KA, "local-a"),
            "    releases: { backend: local-disk }\n",
        )
        .with_default("local-disk");
        let plain_default = enc_default.clone().with_default("hetzner-fsn1");
        let changes = plain_default.encryption_changes(&enc_default);
        assert!(
            changes
                .iter()
                .any(|c| c.subject == Subject::DefaultRoute
                    && c.kind == EncryptionChangeKind::Cleared),
            "{changes:?}"
        );
        let errs = plain_default.unconfirmed_encryption_downgrades(&enc_default, Some(&[]));
        assert!(errs.iter().any(|e| e.contains("route:default")), "{errs:?}");
        assert!(plain_default
            .unconfirmed_encryption_downgrades(&enc_default, Some(&["route:default".into()]))
            .is_empty());
        // Singleton `backend_encryption: aes` → named plaintext backends.
        let singleton = Config::from_yaml_str(&format!(
            "storage:\n  filesystem: /tmp/dgp-enc-state-s\n  backend_encryption: {}\n",
            aes(KA, "single-a")
        ))
        .unwrap();
        let named = cfg("", "    releases: { backend: local-disk }\n");
        let errs = named.unconfirmed_encryption_downgrades(&singleton, Some(&[]));
        assert!(errs.iter().any(|e| e.contains("route:default")), "{errs:?}");
    }

    /// Two configs with one key id and different key bytes differ (KCV).
    #[test]
    fn the_key_check_value_tells_keys_apart() {
        let a = cfg(
            "",
            &format!(
                "    releases: {{ backend: local-disk, encryption: {} }}\n",
                aes(KA, "rel")
            ),
        );
        let b = cfg(
            "",
            &format!(
                "    releases: {{ backend: local-disk, encryption: {} }}\n",
                aes(KB, "rel")
            ),
        );
        let (wa, wb) = (
            a.write_encryption_for_bucket("releases").unwrap(),
            b.write_encryption_for_bucket("releases").unwrap(),
        );
        assert_eq!(wa.key_id, wb.key_id);
        assert_ne!(wa, wb);
        assert_eq!(wa.kcv.as_deref().map(str::len), Some(16));
        assert!(!wa.kcv.as_deref().unwrap().contains(&KA[..16]));
    }

    #[test]
    fn the_confirmation_header_parses_a_list() {
        let mut h = axum::http::HeaderMap::new();
        h.append(
            CONFIRM_ENCRYPTION_DOWNGRADE_HEADER,
            "releases, backend:local-disk".parse().unwrap(),
        );
        h.append(
            CONFIRM_ENCRYPTION_DOWNGRADE_HEADER,
            "db-archive".parse().unwrap(),
        );
        assert_eq!(
            confirmed_downgrades(Some(&h)),
            vec!["releases", "backend:local-disk", "db-archive"]
        );
        assert!(confirmed_downgrades(None).is_empty());
    }
}
