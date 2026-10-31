// SPDX-License-Identifier: BUSL-1.1

//! The `effective_encryption` of one bucket, as the admin bucket list
//! (`GET /_/api/admin/buckets`) reports it (issue #116, P2).
//!
//! The admin UI never decides a bucket's encryption itself: it shows this
//! answer. The answer comes from [`Config::encryption_for_listed_bucket`],
//! the same resolver that the re-encrypt job, the legacy-key scan and the
//! apply notes use, so the bucket card cannot disagree with the engine.

use crate::config::{resolve_site_keys, BackendEncryptionConfig, Config, EncryptionSite, KeyScope};
use serde::Serialize;

/// How one bucket's new writes are encrypted, and where that comes from.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EffectiveEncryption {
    /// `none`, `aes256-gcm-proxy`, `sse-kms` or `sse-s3`.
    pub mode: String,
    /// `bucket`: the bucket's own `encryption` block. `backend`: the
    /// bucket inherits the encryption of `backend`.
    pub source: &'static str,
    /// The raw backend that stores the bucket.
    pub backend: String,
    /// Proxy mode: a key is configured. Native modes: always true.
    pub has_key: bool,
    /// The key id that new writes carry (proxy mode).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    /// The decrypt-only legacy key id of the site (bucket or backend).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legacy_key_id: Option<String>,
    /// SSE-KMS: the KMS key (an identifier, not a secret).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kms_key_id: Option<String>,
    /// The bucket's own key comes from this environment variable
    /// (`key: ${env:NAME}` in the file): the UI shows it read-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
    /// New writes are stored encrypted. Proxy mode without a key is
    /// false: the proxy refuses those writes.
    pub encrypts: bool,
    /// The bucket's conversion to encryption (a marker in the bucket), or
    /// its deprecated `plaintext_reads: serve`; `None` when neither.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversion: Option<ConversionView>,
    /// A re-encrypt job can change objects of this bucket: it encrypts
    /// with a proxy key, or it is plaintext and can still read an older key.
    pub reencrypt: bool,
    /// The config fixes the bucket's backend (`backend:`, or a single
    /// backend). A bucket's own `encryption` block needs that, so an edit
    /// that adds one also sets `backend:` when this is false.
    pub pinned: bool,
    /// The modes that the bucket's own block accepts.
    pub bucket_modes: Vec<&'static str>,
    /// What the bucket's backend does on its own: the encryption that
    /// "Inherit" gives the bucket (equal to the top-level fields when
    /// `source` is `backend`).
    pub inherited: InheritedEncryption,
}

/// The conversion state of a bucket, as the bucket card shows it
/// (`docs/plan/encryption-conversion-marker.md` section 6).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ConversionView {
    /// A marker of this bucket whose deadline is ahead: clients read the
    /// plaintext history.
    pub open: bool,
    /// The marker's times (unix seconds, backend clock) and run id; `None`
    /// without a marker. A marker with `open: false` passed its deadline:
    /// the conversion stopped before it finished.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cutoff: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// Objects that the latest re-encrypt job on this node converted
    /// although clients could not read them (stored after the cutoff).
    pub adopted_after_cutoff: i64,
    /// The latest re-encrypt job of the bucket on this node (job ids are
    /// node-local).
    pub job_on_this_node: Option<ConversionJob>,
    /// The bucket's own block, or its backend's, sets the deprecated
    /// `plaintext_reads: serve`.
    pub serves_plaintext_deprecated: bool,
}

/// A re-encrypt job, as the conversion row shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConversionJob {
    pub id: i64,
    pub status: String,
    pub percent: Option<u8>,
}

/// The backend's own encryption, as a bucket without its own block gets it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InheritedEncryption {
    pub mode: String,
    pub encrypts: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
}

/// The encryption of `backend` itself, through the site visitor.
fn backend_own(cfg: &Config, backend: &str) -> Option<InheritedEncryption> {
    let site = if cfg.backends.is_empty() {
        EncryptionSite::Singleton
    } else {
        EncryptionSite::Backend(backend.to_string())
    };
    let enc = cfg.encryption_at(&site)?;
    let keys = resolve_site_keys(KeyScope::Backend(backend), enc);
    let encrypts = match enc {
        BackendEncryptionConfig::None { .. } => false,
        BackendEncryptionConfig::Aes256GcmProxy { .. } => matches!(keys.primary, Some(Ok(_))),
        BackendEncryptionConfig::SseKms { .. } | BackendEncryptionConfig::SseS3 { .. } => true,
    };
    Some(InheritedEncryption {
        mode: enc.mode_tag().to_string(),
        encrypts,
        key_id: keys.primary_id().map(str::to_string),
    })
}

/// Pure: the effective encryption of `bucket`, which the storage listing
/// found on `held_by`. `None` when the bucket routes to an undefined
/// backend.
pub fn effective_encryption(
    cfg: &Config,
    bucket: &str,
    held_by: Option<&str>,
) -> Option<EffectiveEncryption> {
    let name = bucket.to_ascii_lowercase();
    let e = cfg.encryption_for_listed_bucket(&name, held_by)?;
    // The mode and key id as the engine writes them (WriteEncryption: the
    // proxy key id, or the KMS key of SSE-KMS).
    let write = e.write_encryption();
    let keys = e.site_keys();
    let primary_ok = matches!(keys.primary, Some(Ok(_)));
    let (has_key, encrypts) = match e.config {
        BackendEncryptionConfig::None { .. } => (false, false),
        BackendEncryptionConfig::Aes256GcmProxy { key, .. } => (key.is_some(), primary_ok),
        BackendEncryptionConfig::SseKms { .. } | BackendEncryptionConfig::SseS3 { .. } => {
            (true, true)
        }
    };
    let is_kms = matches!(e.config, BackendEncryptionConfig::SseKms { .. });
    let kms_key_id = write.key_id.clone().filter(|_| is_kms);
    // The proxy key id of the writes. With no key, an explicit id still
    // shows (what the writes would carry once a key is set).
    let key_id = match e.config {
        BackendEncryptionConfig::Aes256GcmProxy { key_id, .. } => {
            write.key_id.clone().or_else(|| key_id.clone())
        }
        _ => None,
    };
    let legacy_key_id = keys
        .legacy_id()
        .map(str::to_string)
        .or_else(|| e.config.legacy_key_id().map(str::to_string));
    let key_env = if e.from_bucket {
        e.config.primary_key().and_then(|k| {
            cfg.env_refs
                .iter()
                .find(|(_, v)| v.as_str() == k)
                .map(|(n, _)| n.clone())
        })
    } else {
        None
    };
    let reencrypt = match e.config {
        BackendEncryptionConfig::Aes256GcmProxy { .. } => primary_ok,
        BackendEncryptionConfig::None { .. } => {
            legacy_key_id.is_some() || !cfg.key_ids_readable_by(&name).is_empty()
        }
        // The job cannot verify an AWS-side mode (maintenance::resolve_desired).
        BackendEncryptionConfig::SseKms { .. } | BackendEncryptionConfig::SseS3 { .. } => false,
    };
    let inherited = backend_own(cfg, &e.backend)?;
    Some(EffectiveEncryption {
        inherited,
        mode: write.mode.clone(),
        source: if e.from_bucket { "bucket" } else { "backend" },
        backend: e.backend.clone(),
        has_key,
        key_id,
        legacy_key_id,
        kms_key_id,
        key_env,
        encrypts,
        conversion: e.config.serves_plaintext().then(|| ConversionView {
            serves_plaintext_deprecated: true,
            ..Default::default()
        }),
        reencrypt,
        pinned: cfg.bucket_storage(&name).is_some(),
        bucket_modes: cfg.bucket_encryption_modes(&e.backend),
    })
}

/// Fill in the conversion state of each bucket in `rows` that encrypts
/// with a proxy key: its marker from the encrypting wrapper's window
/// cache (one marker read per bucket per minute, 8 at once; this node's
/// apply, job and Resume write through it, so their result shows at
/// once) and its latest re-encrypt job on this node.
pub async fn attach_conversions(
    engine: &crate::deltaglider::DynEngine,
    jobs_db: Option<&std::sync::Arc<tokio::sync::Mutex<crate::config_db::JobsDb>>>,
    rows: &mut [(&str, &mut Option<EffectiveEncryption>)],
) {
    let wanted: Vec<String> = rows
        .iter()
        .filter(|(_, e)| {
            e.as_ref()
                .is_some_and(|e| e.mode == "aes256-gcm-proxy" && e.encrypts)
        })
        .map(|(b, _)| b.to_ascii_lowercase())
        .collect();
    let mut markers = std::collections::HashMap::new();
    for chunk in wanted.chunks(8) {
        let states = futures::future::join_all(chunk.iter().map(|b| marker_state(engine, b))).await;
        markers.extend(chunk.iter().cloned().zip(states));
    }
    let db = match jobs_db {
        Some(db) if !markers.is_empty() => Some(db.lock().await),
        _ => None,
    };
    for (bucket, eff) in rows.iter_mut() {
        let Some(eff) = eff.as_mut() else { continue };
        let key = bucket.to_ascii_lowercase();
        let Some(Some((fields, server_now))) = markers.get(&key) else {
            continue;
        };
        let job = db
            .as_ref()
            .and_then(|db| db.maintenance_latest_job_for_bucket(&key, "reencrypt").ok())
            .flatten();
        let adopted = match (&db, &job) {
            (Some(db), Some(j)) => db
                .maintenance_count_failures_with_prefix(
                    j.id,
                    crate::maintenance::conversion::ADOPTED_AFTER_CUTOFF,
                )
                .unwrap_or(0),
            _ => 0,
        };
        let view = eff.conversion.get_or_insert_with(Default::default);
        view.open = *server_now < fields.deadline;
        view.cutoff = Some(fields.cutoff);
        view.deadline = Some(fields.deadline);
        view.run = Some(crate::storage::encrypting::conversion::run_label(
            &fields.run,
        ));
        view.adopted_after_cutoff = adopted;
        view.job_on_this_node = job.map(|j| ConversionJob {
            percent: crate::maintenance::display_percent(&j),
            id: j.id,
            status: j.status,
        });
    }
}

/// The marker of `bucket` with the backend's time, when it is this
/// bucket's (a foreign or unreadable marker shows nothing here).
async fn marker_state(
    engine: &crate::deltaglider::DynEngine,
    bucket: &str,
) -> Option<(crate::storage::encrypting::conversion::Fields, i64)> {
    use crate::storage::encrypting::conversion::{ConversionOp, ConversionReply, MarkerState};
    use crate::storage::StorageBackend;
    match engine
        .storage()
        .conversion(bucket, ConversionOp::CachedStatus)
        .await
    {
        Ok(ConversionReply::Status(MarkerState::Ours { fields, server_now })) => {
            Some((fields, server_now))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KA: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const KB: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    /// hetzner-fsn1 (plaintext, default) and local-disk (proxy AES).
    /// db-archive has its own key on hetzner-fsn1; downloads inherits.
    fn cfg(db_archive: &str) -> Config {
        Config::from_yaml_str(&format!(
            r#"
storage:
  backends:
    - name: hetzner-fsn1
      type: filesystem
      path: /tmp/dgp-eff-enc-a
    - name: local-disk
      type: filesystem
      path: /tmp/dgp-eff-enc-b
      encryption:
        mode: aes256-gcm-proxy
        key: "{KA}"
        key_id: local-2026
  buckets:
    downloads: {{ backend: hetzner-fsn1 }}
    db-archive: {db_archive}
"#
        ))
        .unwrap()
    }

    #[test]
    fn a_bucket_key_is_reported_with_its_source() {
        let c = cfg(&format!(
            "{{ backend: hetzner-fsn1, encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: db-archive-2026-10 }} }}"
        ));
        let e = effective_encryption(&c, "db-archive", Some("hetzner-fsn1")).unwrap();
        assert_eq!(e.mode, "aes256-gcm-proxy");
        assert_eq!(e.source, "bucket");
        assert_eq!(e.backend, "hetzner-fsn1");
        assert_eq!(e.key_id.as_deref(), Some("db-archive-2026-10"));
        assert!(e.has_key && e.encrypts && e.reencrypt && e.pinned);
        assert_eq!(e.legacy_key_id, None);
        assert_eq!(e.bucket_modes, vec!["none", "aes256-gcm-proxy"]);
        // The neighbour on the same backend stays plaintext.
        // Inherit would give the backend's plaintext.
        assert_eq!(e.inherited.mode, "none");
        assert!(!e.inherited.encrypts);
        let d = effective_encryption(&c, "downloads", Some("hetzner-fsn1")).unwrap();
        assert_eq!((d.mode.as_str(), d.source), ("none", "backend"));
        assert!(!d.encrypts && !d.reencrypt);
    }

    #[test]
    fn an_inherited_mode_names_the_backend_that_holds_the_bucket() {
        let c = cfg("{ backend: hetzner-fsn1 }");
        // `stray` has no route; the listing found it on local-disk.
        let e = effective_encryption(&c, "stray", Some("local-disk")).unwrap();
        assert_eq!((e.source, e.backend.as_str()), ("backend", "local-disk"));
        assert_eq!(e.key_id.as_deref(), Some("local-2026"));
        assert!(e.encrypts && !e.pinned);
        assert_eq!(e.inherited.key_id.as_deref(), Some("local-2026"));
    }

    #[test]
    fn off_for_this_bucket_keeps_the_legacy_key_and_can_be_reencrypted() {
        let c = cfg(&format!(
            "{{ backend: hetzner-fsn1, encryption: {{ mode: none, legacy_key: \"{KB}\", legacy_key_id: db-archive-2026-10 }} }}"
        ));
        let e = effective_encryption(&c, "db-archive", None).unwrap();
        assert_eq!((e.mode.as_str(), e.source), ("none", "bucket"));
        assert_eq!(e.legacy_key_id.as_deref(), Some("db-archive-2026-10"));
        assert!(!e.encrypts && !e.has_key && e.reencrypt);
    }

    #[test]
    fn a_key_from_an_env_reference_names_its_variable() {
        let mut c = cfg(&format!(
            "{{ backend: hetzner-fsn1, encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: db-archive-2026-10 }} }}"
        ));
        c.env_refs.insert("DB_ARCHIVE_KEY".into(), KB.into());
        let e = effective_encryption(&c, "db-archive", None).unwrap();
        assert_eq!(e.key_env.as_deref(), Some("DB_ARCHIVE_KEY"));
        // The JSON never carries a key.
        let json = serde_json::to_string(&e).unwrap();
        assert!(!json.contains(KB), "{json}");
    }

    /// Native SSE on a bucket of an S3 backend (P3): the KMS key is the
    /// key id of the write, and the bucket offers every mode.
    #[test]
    fn a_native_bucket_mode_reports_its_kms_key() {
        let arn = "arn:aws:kms:eu-central-1:123456789012:key/1234abcd-12ab-34cd-56ef-1234567890ab";
        let c = Config::from_yaml_str(&format!(
            r#"
storage:
  backends:
    - name: aws-dr
      type: s3
      region: eu-central-1
      access_key_id: AKIAEXAMPLE
      secret_access_key: example-secret
  buckets:
    db-archive: {{ backend: aws-dr, encryption: {{ mode: sse-kms, kms_key_id: "{arn}" }} }}
"#
        ))
        .unwrap();
        let e = effective_encryption(&c, "db-archive", None).unwrap();
        assert_eq!((e.mode.as_str(), e.source), ("sse-kms", "bucket"));
        assert_eq!(e.kms_key_id.as_deref(), Some(arn));
        assert_eq!(e.key_id, None);
        assert!(e.encrypts && !e.reencrypt);
        assert_eq!(
            e.bucket_modes,
            vec!["none", "aes256-gcm-proxy", "sse-kms", "sse-s3"]
        );
    }

    #[test]
    fn a_route_to_an_undefined_backend_has_no_answer() {
        let c = cfg("{ backend: gone }");
        assert!(effective_encryption(&c, "db-archive", None).is_none());
    }
}
