// SPDX-License-Identifier: BUSL-1.1

//! Per-bucket encryption at rest (issue #116): the config side.
//!
//! * [`EncryptionSite`] and the ONE visitor over every encryption config in
//!   a [`Config`] (the singleton backend, each named backend, each bucket
//!   override). Every secrets surface (redaction, preservation, backup,
//!   the apply diff) walks the config through it, so a site cannot be
//!   skipped (source guard `every_encryption_site_goes_through_the_visitor`).
//! * Where a bucket's writes land ([`Config::bucket_storage`]): the real
//!   bucket on a raw backend. The override belongs to that storage, so the
//!   engine looks it up by the real bucket name.
//! * The fatal rules of an override ([`Config::bucket_encryption_errors`])
//!   and the apply-time rules ([`Config::dropped_bucket_keys`],
//!   [`Config::bucket_encryption_notes`]).

use super::*;

/// Where one encryption config lives in a [`Config`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EncryptionSite {
    /// `storage.backend_encryption`: the singleton backend (`default`).
    Singleton,
    /// `storage.backends[name].encryption`.
    Backend(String),
    /// `storage.buckets.<name>.encryption` (issue #116).
    Bucket(String),
}

impl EncryptionSite {
    /// Operator-facing name, for messages.
    pub fn label(&self) -> String {
        match self {
            Self::Singleton => "backend 'default'".to_string(),
            Self::Backend(name) => format!("backend '{name}'"),
            Self::Bucket(name) => format!("bucket '{name}'"),
        }
    }
}

/// The `encryption.mode` values that a bucket override accepts in this
/// release. The admin UI offers exactly these on the bucket card (the
/// response field `effective_encryption.bucket_modes`), so a mode that the
/// server learns to accept appears in the UI with no client change. The
/// test `bucket_modes_match_the_fatal_rules` ties this list to
/// [`Config::bucket_encryption_errors`].
pub const BUCKET_ENCRYPTION_MODES: &[&str] = &["none", "aes256-gcm-proxy", "sse-kms", "sse-s3"];

/// The native S3 modes: a filesystem backend refuses them on a bucket.
const NATIVE_MODES: &[&str] = &["sse-kms", "sse-s3"];

/// One bucket override with the storage it applies to.
#[derive(Debug, Clone)]
pub struct BucketOverride<'a> {
    /// The configured (virtual) bucket name.
    pub bucket: &'a str,
    /// The raw backend that stores the bucket.
    pub backend: String,
    /// The bucket name on that backend (the alias, else the name).
    pub real_bucket: String,
    pub config: &'a BackendEncryptionConfig,
}

impl Config {
    /// THE list of every encryption config in this config: the singleton,
    /// every named backend, every bucket override. Read-only visitor.
    pub fn encryption_sites(&self) -> Vec<(EncryptionSite, &BackendEncryptionConfig)> {
        let mut out = vec![(EncryptionSite::Singleton, &self.backend_encryption)];
        out.extend(
            self.backends
                .iter()
                .map(|b| (EncryptionSite::Backend(b.name.clone()), &b.encryption)),
        );
        out.extend(self.buckets.iter().filter_map(|(name, p)| {
            p.encryption
                .as_ref()
                .map(|e| (EncryptionSite::Bucket(name.clone()), e))
        }));
        out
    }

    /// [`Self::encryption_sites`], mutable.
    pub fn encryption_sites_mut(&mut self) -> Vec<(EncryptionSite, &mut BackendEncryptionConfig)> {
        let mut out = vec![(EncryptionSite::Singleton, &mut self.backend_encryption)];
        out.extend(
            self.backends
                .iter_mut()
                .map(|b| (EncryptionSite::Backend(b.name.clone()), &mut b.encryption)),
        );
        out.extend(self.buckets.iter_mut().filter_map(|(name, p)| {
            p.encryption
                .as_mut()
                .map(|e| (EncryptionSite::Bucket(name.clone()), e))
        }));
        out
    }

    /// The bucket policies with every override's key material removed: the
    /// form an API response carries. The running config holds EXPANDED
    /// values, so a key that came from `${env:NAME}` is removed too (no
    /// reference is put back here; the export does that).
    /// `GET /config` hands the policies out on their own, outside the
    /// whole-config redactors.
    pub fn redacted_bucket_policies(
        &self,
    ) -> std::collections::BTreeMap<String, crate::bucket_policy::BucketPolicyConfig> {
        let mut out = self.buckets.clone();
        for enc in out.values_mut().filter_map(|p| p.encryption.as_mut()) {
            enc.redact_secrets();
        }
        out
    }

    /// The encryption config at `site`, if this config has that site.
    pub fn encryption_at(&self, site: &EncryptionSite) -> Option<&BackendEncryptionConfig> {
        match site {
            EncryptionSite::Singleton => Some(&self.backend_encryption),
            EncryptionSite::Backend(name) => self
                .backends
                .iter()
                .find(|b| &b.name == name)
                .map(|b| &b.encryption),
            EncryptionSite::Bucket(name) => {
                self.buckets.get(name).and_then(|p| p.encryption.as_ref())
            }
        }
    }

    /// The `(raw backend, real bucket)` that a CONFIGURED bucket's writes
    /// land on, when the config fixes it. Single backend: always the
    /// singleton `default`, under the bucket's own name (an alias needs a
    /// route, which a single backend does not build). Named backends: only
    /// with an explicit `backend:`, under the alias or the name; an
    /// unrouted bucket can live on any backend (the router probes).
    pub fn bucket_storage(&self, bucket: &str) -> Option<(String, String)> {
        let name = bucket.to_ascii_lowercase();
        let policy = self.buckets.get(&name);
        if self.backends.is_empty() {
            return Some(("default".to_string(), name));
        }
        let policy = policy?;
        let backend = policy.backend.clone()?;
        let real = policy.alias.clone().unwrap_or(name);
        Some((backend, real))
    }

    /// Every bucket override whose storage the config fixes (see
    /// [`Self::bucket_storage`]). An override without one is a fatal error
    /// (`Config::check_fatal`).
    pub fn bucket_encryption_overrides(&self) -> Vec<BucketOverride<'_>> {
        self.buckets
            .iter()
            .filter_map(|(name, p)| {
                let config = p.encryption.as_ref()?;
                let (backend, real_bucket) = self.bucket_storage(name)?;
                Some(BucketOverride {
                    bucket: name,
                    backend,
                    real_bucket,
                    config,
                })
            })
            .collect()
    }

    /// FATAL: an SSE-KMS `kms_key_id` that no KMS accepts, at every
    /// encryption site (backends and buckets; part of [`Self::check_fatal`]).
    pub(super) fn kms_key_id_errors(&self) -> Vec<String> {
        self.encryption_sites()
            .into_iter()
            .filter_map(|(site, enc)| match enc {
                BackendEncryptionConfig::SseKms { kms_key_id, .. } => {
                    kms_key_id_problem(kms_key_id).map(|p| format!("{}: {p}", site.label()))
                }
                _ => None,
            })
            .collect()
    }

    /// FATAL errors of the bucket overrides (part of [`Self::check_fatal`]):
    /// a config the proxy must never run with.
    pub(super) fn bucket_encryption_errors(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let overrides: Vec<(&String, &BackendEncryptionConfig)> = self
            .buckets
            .iter()
            .filter_map(|(n, p)| p.encryption.as_ref().map(|e| (n, e)))
            .collect();
        if overrides.is_empty() {
            return errors;
        }
        // Multi-instance (config_sync_bucket) is allowed: the encryption
        // witness (`coordination::encryption_witness`) makes a node that
        // disagrees about a bucket refuse writes to it (issue #116 P3).
        for (bucket, enc) in &overrides {
            let at = format!("bucket '{bucket}'");
            match enc {
                // Native SSE needs S3: a filesystem backend would store the
                // bytes as they are (issue #116 P3).
                BackendEncryptionConfig::SseKms { .. } | BackendEncryptionConfig::SseS3 { .. } => {
                    let on_filesystem = self
                        .bucket_storage(bucket)
                        .and_then(|(backend, _)| self.backend_by_name(&backend))
                        .is_some_and(|def| matches!(def, BackendConfig::Filesystem { .. }));
                    if on_filesystem {
                        errors.push(format!(
                            "{at}: encryption mode '{}' is native S3 encryption, but the \
                             bucket is stored on a filesystem backend, which would store \
                             the bytes as they are. Use 'aes256-gcm-proxy' for this bucket, \
                             or store it on an S3 backend",
                            enc.mode_tag()
                        ));
                    }
                }
                BackendEncryptionConfig::Aes256GcmProxy { key_id, .. } => match key_id {
                    None => errors.push(format!(
                        "{at}: encryption.key_id is required on a bucket override. The id \
                             is stamped on every object, so it must not depend on a name; \
                             choose a stable id such as '{bucket}-2026'"
                    )),
                    Some(id) if !is_valid_key_id(id) => errors.push(format!(
                        "{at}: encryption.key_id='{id}' must match [A-Za-z0-9_.-]{{1,64}}"
                    )),
                    Some(_) => {}
                },
                BackendEncryptionConfig::None { .. } => {}
            }
            // A key that does not parse: THE key resolution's error (a
            // missing id is reported above and below, once).
            let site = resolve_site_keys(KeyScope::Bucket(bucket), enc);
            for (slot, has_id) in [
                (&site.primary, enc_has_key_id(enc)),
                (&site.legacy, enc.legacy_key_id().is_some()),
            ] {
                if let (Some(Err(e)), true) = (slot, has_id) {
                    errors.push(e.clone());
                }
            }
            if enc.legacy_key().is_some() {
                match enc.legacy_key_id() {
                    None => errors.push(format!(
                        "{at}: encryption.legacy_key needs an explicit legacy_key_id on a \
                         bucket override: the id that the objects written under that key carry"
                    )),
                    Some(id) if !is_valid_key_id(id) => errors.push(format!(
                        "{at}: encryption.legacy_key_id='{id}' must match [A-Za-z0-9_.-]{{1,64}}"
                    )),
                    Some(_) => {}
                }
            }
            if self.bucket_storage(bucket).is_none() {
                errors.push(format!(
                    "{at} sets its own encryption but has no `backend:`. With named backends, \
                     a bucket without a route can live on any backend, so the proxy cannot \
                     tell which storage the override applies to. Add `backend: <name>` to the \
                     bucket"
                ));
            }
        }
        errors.extend(self.alias_encryption_disagreements());
        errors.extend(self.bucket_key_id_collisions());
        errors
    }

    /// Two configured names on one `(backend, real bucket)` whose effective
    /// encryption differs: one deltaspace would be written in two forms.
    fn alias_encryption_disagreements(&self) -> Vec<String> {
        let mut by_storage: std::collections::BTreeMap<(String, String), Vec<&String>> =
            Default::default();
        for name in self.buckets.keys() {
            if let Some(storage) = self.bucket_storage(name) {
                by_storage.entry(storage).or_default().push(name);
            }
        }
        let mut errors = Vec::new();
        for ((backend, real), names) in by_storage {
            if names.len() < 2 {
                continue;
            }
            // Each name's own block (the resolver answers by storage, so
            // it would hide a name that carries no block or another one).
            let own = |n: &str| self.buckets.get(n).and_then(|p| p.encryption.clone());
            let first = own(names[0]);
            if names[1..].iter().any(|n| own(n) != first) {
                errors.push(format!(
                    "buckets {names:?} all store into bucket '{real}' on backend '{backend}' but \
                     do not agree on its encryption. The encryption belongs to the storage, so \
                     give every one of these names the same `encryption` block (or none)"
                ));
            }
        }
        errors
    }

    /// A key id that two sites give DIFFERENT key bytes, when one of them is
    /// a bucket override. (Two backends with that conflict are refused when
    /// the engine is built and warned about by `check`.)
    fn bucket_key_id_collisions(&self) -> Vec<String> {
        let keys = self.resolved_keys().keys;
        let mut errors = Vec::new();
        for (i, a) in keys.iter().enumerate() {
            for b in &keys[i + 1..] {
                let involves_bucket = matches!(a.site, EncryptionSite::Bucket(_))
                    || matches!(b.site, EncryptionSite::Bucket(_));
                if involves_bucket && a.key_id == b.key_id && a.key.0 != b.key.0 {
                    errors.push(format!(
                        "{} and {} use key id '{}' with DIFFERENT keys. A key id names one \
                         key everywhere (objects carry it to pick their key on read); give \
                         one of them another key id",
                        a.site.label(),
                        b.site.label(),
                        a.key_id
                    ));
                }
            }
        }
        errors.dedup();
        errors
    }

    /// Advisory warnings of the bucket overrides (part of `check`).
    pub(super) fn bucket_encryption_warnings(&self) -> Vec<String> {
        let mut warnings: Vec<String> = self
            .keyless_proxy_buckets()
            .into_iter()
            .map(|b| {
                format!(
                    "bucket '{b}' uses aes256-gcm-proxy but no key is configured: the proxy \
                     refuses every write to it (it never stores plaintext there). Set the \
                     bucket's encryption.key"
                )
            })
            .collect();
        // Owner decision 4: replication from an encrypted bucket into a
        // plaintext one is allowed, with a warning.
        let encrypts = |bucket: &str| {
            self.encryption_for_bucket(bucket)
                .is_some_and(|e| e.config.mode_tag() != "none")
        };
        let plaintext = |bucket: &str| {
            self.encryption_for_bucket(bucket)
                .is_some_and(|e| e.config.mode_tag() == "none")
        };
        for rule in &self.replication.rules {
            if encrypts(&rule.source.bucket) && plaintext(&rule.destination.bucket) {
                warnings.push(format!(
                    "replication rule '{}' copies the encrypted bucket '{}' into bucket '{}', \
                     which stores plaintext: the copies are not encrypted at rest. Give the \
                     destination an encryption (on the bucket or its backend) if that is not \
                     what you want",
                    rule.name, rule.source.bucket, rule.destination.bucket
                ));
            }
        }
        warnings
    }

    /// The modes that the own `encryption` block of a bucket stored on
    /// `backend` accepts (see `BUCKET_ENCRYPTION_MODES`).
    pub fn bucket_encryption_modes(&self, backend: &str) -> Vec<&'static str> {
        let on_filesystem = self
            .backend_by_name(backend)
            .is_some_and(|def| matches!(def, BackendConfig::Filesystem { .. }));
        BUCKET_ENCRYPTION_MODES
            .iter()
            .copied()
            .filter(|m| !(on_filesystem && NATIVE_MODES.contains(m)))
            .collect()
    }

    /// Bucket overrides in proxy mode with no key (the engine refuses every
    /// write to those buckets).
    pub fn keyless_proxy_buckets(&self) -> Vec<String> {
        self.buckets
            .iter()
            .filter(|(_, p)| {
                matches!(
                    p.encryption,
                    Some(BackendEncryptionConfig::Aes256GcmProxy { key: None, .. })
                )
            })
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// The key ids that `bucket` can still decrypt with under this config:
    /// its own block's slots, and the slots of the backend it is stored on.
    /// NOT the keys of other buckets on that backend: a bucket must never
    /// depend on another bucket's block to read its own objects (review A:
    /// two buckets that shared a key id could drop it one after the other,
    /// each step looking safe because the other still held it).
    pub(crate) fn key_ids_readable_by(&self, bucket: &str) -> std::collections::HashSet<String> {
        let mut ids = std::collections::HashSet::new();
        if let Some(enc) = self.buckets.get(bucket).and_then(|p| p.encryption.as_ref()) {
            let own = resolve_site_keys(KeyScope::Bucket(bucket), enc);
            ids.extend(own.primary_id().map(str::to_string));
            ids.extend(own.legacy_id().map(str::to_string));
        }
        let backend = self
            .bucket_storage(bucket)
            .map(|(b, _)| b)
            .or_else(|| self.effective_backend_for_bucket(bucket).map(|(b, _)| b));
        if let Some(backend) = backend {
            let site = if self.backends.is_empty() {
                EncryptionSite::Singleton
            } else {
                EncryptionSite::Backend(backend.clone())
            };
            if let Some(enc) = self.encryption_at(&site) {
                let keys = resolve_site_keys(KeyScope::Backend(&backend), enc);
                ids.extend(keys.primary_id().map(str::to_string));
                ids.extend(keys.legacy_id().map(str::to_string));
            }
        }
        ids
    }

    /// The apply gate of every key a bucket override held in `old`
    /// (review A). For each bucket, a key id of its own block (primary or
    /// legacy) must stay readable BY THAT BUCKET in `self`: in its own
    /// block, or in the slots of the backend it is stored on.
    ///
    /// * An ACTIVE key never leaves in one step: keep it as the bucket's
    ///   `legacy_key` (the admin API does that when the body leaves
    ///   `legacy_key` out). A migrate flip keeps the bucket's block, so it
    ///   passes.
    /// * A LEGACY key leaves only when the request confirms its id
    ///   (`confirmed`, from the `X-DGP-Confirm-Key-Removal` header or
    ///   `config apply --confirm-key-removal`), after the operator checked
    ///   `GET /backends/<backend>/legacy-key-usage?bucket=<bucket>`, which
    ///   scans every bucket stored on that backend.
    ///
    /// Migrate's transient routes (`__dgmigrate_*`) are internal and skipped.
    ///
    /// `absent` names configured buckets that do not exist on their storage
    /// (the caller lists the storage). Such a bucket holds no object, so
    /// its keys leave with no gate: the rollback of a create that failed,
    /// and the removal of settings for a bucket that was never created.
    pub fn dropped_bucket_keys(
        &self,
        old: &Config,
        confirmed: &[String],
        absent: &[String],
    ) -> Vec<String> {
        let mut errors = Vec::new();
        for (name, policy) in &old.buckets {
            let Some(enc) = policy.encryption.as_ref() else {
                continue;
            };
            if name.starts_with(crate::maintenance::migrate::TRANSIENT_PREFIX) {
                continue;
            }
            if absent.iter().any(|a| a == name) {
                continue;
            }
            let keys = resolve_site_keys(KeyScope::Bucket(name), enc);
            let readable = self.key_ids_readable_by(name);
            let backend = old.bucket_storage(name).map(|(b, _)| b).unwrap_or_default();
            if let Some(id) = keys.primary_id().filter(|id| !readable.contains(*id)) {
                errors.push(format!(
                    "bucket '{name}': this change removes key id '{id}', which the bucket \
                     writes with, from the keys the bucket can read. Objects written under \
                     it would become unreadable. Keep it as the bucket's `legacy_key` / \
                     `legacy_key_id` (set `mode: none` or a new key first, and leave \
                     `legacy_key` out: the old key moves there), run the re-encrypt job for \
                     the bucket, and remove the legacy key later"
                ));
            }
            if let Some(id) = keys
                .legacy_id()
                .filter(|id| !readable.contains(*id) && !confirmed.iter().any(|c| c == id))
            {
                errors.push(format!(
                    "bucket '{name}': this change removes the legacy (decrypt-only) key id \
                     '{id}' from the keys the bucket can read. Objects that still carry it \
                     would become unreadable. Check GET \
                     /_/api/admin/backends/{backend}/legacy-key-usage?bucket={name} (it scans \
                     every bucket on the backend); when it answers safe_to_clear, send the \
                     change again with the header `X-DGP-Confirm-Key-Removal: {id}` \
                     (`config apply --confirm-key-removal {id}`)"
                ));
            }
        }
        errors
    }

    /// For the apply response: one note per configured bucket whose
    /// effective write encryption (mode and key id) changes from `old`.
    /// Existing objects keep the form they were written in; the
    /// re-encrypt job rewrites them. Migrate's transient routes are skipped,
    /// and so are the buckets in `converting` (the apply started their
    /// conversion and queued the job itself).
    pub fn bucket_encryption_notes(&self, old: &Config, converting: &[&str]) -> Vec<String> {
        let names: std::collections::BTreeSet<&String> =
            self.buckets.keys().chain(old.buckets.keys()).collect();
        let mut notes = Vec::new();
        for name in names {
            if name.starts_with(crate::maintenance::migrate::TRANSIENT_PREFIX)
                || converting.iter().any(|c| c.eq_ignore_ascii_case(name))
            {
                continue;
            }
            let (Some(before), Some(after)) = (
                old.encryption_for_bucket(name),
                self.encryption_for_bucket(name),
            ) else {
                continue;
            };
            let id = |e: &BucketEncryption<'_>| {
                let kid = e.site_keys().primary_id().map(str::to_string);
                (e.config.mode_tag(), kid)
            };
            let (b, a) = (id(&before), id(&after));
            if b == a {
                continue;
            }
            let describe = |(mode, kid): &(&str, Option<String>)| match kid {
                Some(kid) => format!("{mode} (key id '{kid}')"),
                None => mode.to_string(),
            };
            notes.push(format!(
                "bucket '{name}': new writes use {} instead of {}. Objects stored before keep \
                 their old form and stay readable. To rewrite them, start the re-encrypt job: \
                 POST /_/api/admin/jobs/reencrypt with {{\"buckets\": [\"{name}\"]}}",
                describe(&a),
                describe(&b)
            ));
        }
        notes
    }
}

/// Does a proxy-AES config name its primary key id?
fn enc_has_key_id(enc: &BackendEncryptionConfig) -> bool {
    matches!(
        enc,
        BackendEncryptionConfig::Aes256GcmProxy {
            key_id: Some(_),
            ..
        }
    )
}
