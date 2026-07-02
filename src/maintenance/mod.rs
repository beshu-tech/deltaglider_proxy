// SPDX-License-Identifier: GPL-3.0-only

//! One-off bucket maintenance jobs.
//!
//! v1 ships a single kind: **`reencrypt`** — rewrite every object in a
//! bucket through the engine so its at-rest encryption state matches the
//! backend's CURRENT config. One uniform, canned procedure covers all
//! three transitions: enable (encrypt plaintext objects), rotate
//! (re-encrypt under the new key), disable (decrypt back to plaintext).
//! Objects already in the desired state are skipped, which makes the job
//! idempotent and resumable.
//!
//! Architecture (mirrors `src/replication/` / `src/lifecycle/`):
//!
//! - [`store`] — durable job rows in the SQLCipher ConfigDb: phase +
//!   continuation-token cursor, progress counters, leader lease,
//!   boot-time reconcile (running → queued, cursor preserved).
//! - [`gate`] — the per-bucket WRITE gate. While a bucket has an active
//!   job, S3 write requests (PUT/POST/DELETE) get `503 SlowDown`; reads
//!   stay up (the read path handles mixed encrypted/plaintext state
//!   transparently). Write-blocking is a CORRECTNESS requirement, not
//!   UX: a client PUT racing the job's retrieve→store would be silently
//!   overwritten with stale bytes.
//! - [`worker`] — the single sequential background runner.
//!
//! This module hosts the PURE decision logic so the truth tables are
//! unit-tested without I/O (`needs_rewrite`, `resolve_desired`,
//! `progress_percent`).
//!
//! ## Multi-instance caveat
//!
//! The ConfigDb is per-instance and the write gate is in-process memory.
//! In a multi-instance deployment, other instances do not gate the
//! bucket — the same single-runner posture replication and lifecycle
//! already have. The leader lease prevents double-running on one
//! instance's DB, nothing more.

pub mod gate;
pub mod migrate;
pub mod store;
pub mod worker;

use std::collections::HashMap;

use crate::config::{BackendEncryptionConfig, Config};
use crate::storage::encrypting::{
    CHUNK_MARKER_VALUE, ENCRYPTION_KEY_ID_KEY, ENCRYPTION_MARKER_KEY, ENCRYPTION_MARKER_VALUE,
};

/// The at-rest state every object in a bucket SHOULD be in, derived from
/// the bucket's backend encryption config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesiredEncryption {
    /// Backend mode `none` — objects should be plaintext, no markers.
    Plain,
    /// Backend mode `aes256-gcm-proxy` — objects should carry the
    /// encryption marker stamped with exactly this key id.
    Proxy { key_id: String },
}

/// Resolve the bucket's desired at-rest state from the config.
///
/// Follows the same routing the engine uses: explicit bucket-policy
/// `backend`, else the default backend, else the first named backend,
/// else the legacy singleton (synthetic name `default` — the same name
/// the engine mixes into the derived key id, so the ids match what the
/// wrapper stamps).
///
/// Returns `Err(reason)` for configurations the v1 job cannot verify or
/// normalize: SSE modes (encryption happens AWS-side, no per-object
/// proxy marker to check) and proxy mode without a key (writes would be
/// plaintext — a misconfiguration to fix before running a job).
pub fn resolve_desired(config: &Config, bucket: &str) -> Result<DesiredEncryption, String> {
    let bucket_key = bucket.to_ascii_lowercase();
    let (backend_name, enc) = if config.backends.is_empty() {
        ("default".to_string(), config.backend_encryption.clone())
    } else {
        let explicit = config
            .buckets
            .get(&bucket_key)
            .and_then(|p| p.backend.clone());
        let name = explicit
            .or_else(|| config.default_backend.clone())
            .unwrap_or_else(|| config.backends[0].name.clone());
        let named = config
            .backends
            .iter()
            .find(|b| b.name == name)
            .ok_or_else(|| format!("bucket '{bucket}' routes to unknown backend '{name}'"))?;
        (name, named.encryption.clone())
    };

    match enc {
        BackendEncryptionConfig::None { .. } => Ok(DesiredEncryption::Plain),
        BackendEncryptionConfig::Aes256GcmProxy { key, key_id, .. } => {
            let Some(hex) = key else {
                return Err(format!(
                    "backend '{backend_name}' is in aes256-gcm-proxy mode but has no key configured — \
                     new writes would be plaintext; fix the key before re-encrypting"
                ));
            };
            let key_id = match key_id {
                Some(explicit) => explicit,
                None => {
                    let parsed = crate::storage::EncryptionKey::from_hex(&hex)
                        .map_err(|e| format!("backend '{backend_name}' key: {e}"))?;
                    crate::deltaglider::derive_key_id(&backend_name, &parsed.0)
                }
            };
            Ok(DesiredEncryption::Proxy { key_id })
        }
        BackendEncryptionConfig::SseKms { .. } | BackendEncryptionConfig::SseS3 { .. } => {
            Err(format!(
                "backend '{backend_name}' uses {} — AWS-side encryption leaves no per-object \
                 proxy marker, so the re-encrypt job cannot verify object state (unsupported in v1)",
                enc.mode_tag()
            ))
        }
    }
}

/// Does this object's at-rest state differ from the desired one?
///
/// Truth table (marker = `dg-encrypted` ∈ {aes-256-gcm-v1, aes-256-gcm-chunked-v1}):
///
/// | marker | desired       | rewrite? |
/// |--------|---------------|----------|
/// | absent | Plain         | no       |
/// | absent | Proxy         | yes      |
/// | present| Plain         | yes (decrypt) |
/// | present| Proxy(k)      | iff stamped key-id ≠ k (or absent) |
pub fn needs_rewrite(user_metadata: &HashMap<String, String>, desired: &DesiredEncryption) -> bool {
    let marker = user_metadata.get(ENCRYPTION_MARKER_KEY).map(String::as_str);
    let encrypted = matches!(
        marker,
        Some(ENCRYPTION_MARKER_VALUE) | Some(CHUNK_MARKER_VALUE)
    );
    match desired {
        DesiredEncryption::Plain => encrypted,
        DesiredEncryption::Proxy { key_id } => {
            if !encrypted {
                return true;
            }
            user_metadata.get(ENCRYPTION_KEY_ID_KEY).map(String::as_str) != Some(key_id.as_str())
        }
    }
}

// `strip_encryption_markers` lives in `storage::encrypting` (where the marker
// consts do) — the single home shared by the re-encrypt job here AND the
// delta-passthrough ship in `transfer`. Re-exported so `super::` callers in this
// module are unaffected.
pub(crate) use crate::storage::encrypting::strip_encryption_markers;

/// Display percent for a job ROW: 100 once completed, otherwise
/// [`progress_percent`]. The one home for the rule both admin views
/// (jobs.rs + the session-light bucket status) share.
pub fn display_percent(job: &store::MaintenanceJob) -> Option<u8> {
    match job.status.as_str() {
        "completed" => Some(100),
        _ => progress_percent(
            &job.phase,
            job.objects_total,
            job.objects_done,
            job.objects_skipped,
        ),
    }
}

/// Overall progress percent for the UI, or `None` while it cannot be
/// estimated (counting phase). Capped at 99 until the caller marks the
/// job completed — the references phase rides in the final percent.
pub fn progress_percent(
    phase: &str,
    objects_total: Option<i64>,
    objects_done: i64,
    objects_skipped: i64,
) -> Option<u8> {
    match phase {
        "counting" => None,
        // Migrate phases: no total is ever counted (D9 — the copy must be
        // failure-free to flip anyway), so the bar stays indeterminate
        // until the post-flip endgame.
        "stage" | "copy" | "verify" => None,
        "references" | "flip" | "cleanup" => Some(99),
        _ => {
            let total = objects_total?;
            if total <= 0 {
                return Some(99);
            }
            let processed = (objects_done + objects_skipped).clamp(0, total);
            Some(((processed * 99) / total).clamp(0, 99) as u8)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn needs_rewrite_truth_table() {
        let plain = DesiredEncryption::Plain;
        let proxy = DesiredEncryption::Proxy {
            key_id: "kid-1".into(),
        };

        // absent marker
        assert!(
            !needs_rewrite(&meta(&[]), &plain),
            "plaintext + Plain = no-op"
        );
        assert!(
            needs_rewrite(&meta(&[]), &proxy),
            "plaintext + Proxy = encrypt"
        );

        // single-shot marker
        let enc = meta(&[
            ("dg-encrypted", "aes-256-gcm-v1"),
            ("dg-encryption-key-id", "kid-1"),
        ]);
        assert!(needs_rewrite(&enc, &plain), "encrypted + Plain = decrypt");
        assert!(!needs_rewrite(&enc, &proxy), "matching key id = no-op");

        // chunked marker counts as encrypted too
        let chunked = meta(&[
            ("dg-encrypted", "aes-256-gcm-chunked-v1"),
            ("dg-encryption-key-id", "kid-1"),
        ]);
        assert!(!needs_rewrite(&chunked, &proxy));
        assert!(needs_rewrite(&chunked, &plain));

        // key rotation: stamped id differs
        let old_key = meta(&[
            ("dg-encrypted", "aes-256-gcm-v1"),
            ("dg-encryption-key-id", "kid-0"),
        ]);
        assert!(
            needs_rewrite(&old_key, &proxy),
            "key-id mismatch = re-encrypt"
        );

        // legacy object: marker without key id → conservative rewrite
        let no_kid = meta(&[("dg-encrypted", "aes-256-gcm-v1")]);
        assert!(
            needs_rewrite(&no_kid, &proxy),
            "missing key id = re-encrypt"
        );

        // unknown marker value is NOT treated as encrypted
        let bogus = meta(&[("dg-encrypted", "something-else")]);
        assert!(!needs_rewrite(&bogus, &plain));
        assert!(needs_rewrite(&bogus, &proxy));
    }

    #[test]
    fn strip_removes_both_markers_only() {
        let mut m = meta(&[
            ("dg-encrypted", "aes-256-gcm-v1"),
            ("dg-encryption-key-id", "kid"),
            ("custom", "keep-me"),
        ]);
        strip_encryption_markers(&mut m);
        assert!(!m.contains_key("dg-encrypted"));
        assert!(!m.contains_key("dg-encryption-key-id"));
        assert_eq!(m.get("custom").map(String::as_str), Some("keep-me"));
    }

    #[test]
    fn progress_percent_phases() {
        assert_eq!(progress_percent("counting", None, 0, 0), None);
        assert_eq!(progress_percent("objects", None, 5, 0), None);
        assert_eq!(progress_percent("objects", Some(0), 0, 0), Some(99));
        assert_eq!(progress_percent("objects", Some(100), 0, 0), Some(0));
        assert_eq!(progress_percent("objects", Some(100), 40, 10), Some(49));
        assert_eq!(
            progress_percent("objects", Some(100), 100, 0),
            Some(99),
            "capped at 99"
        );
        assert_eq!(progress_percent("references", Some(100), 100, 0), Some(99));
    }

    #[test]
    fn resolve_desired_modes() {
        let mut cfg = Config::default();
        // Singleton plaintext.
        assert_eq!(
            resolve_desired(&cfg, "b").unwrap(),
            DesiredEncryption::Plain
        );

        // Singleton proxy with explicit key_id.
        cfg.backend_encryption = BackendEncryptionConfig::Aes256GcmProxy {
            key: Some("00".repeat(32)),
            key_id: Some("explicit-id".into()),
            legacy_key: None,
            legacy_key_id: None,
        };
        assert_eq!(
            resolve_desired(&cfg, "b").unwrap(),
            DesiredEncryption::Proxy {
                key_id: "explicit-id".into()
            }
        );

        // Derived key id is stable and mixes the backend name ("default").
        cfg.backend_encryption = BackendEncryptionConfig::Aes256GcmProxy {
            key: Some("00".repeat(32)),
            key_id: None,
            legacy_key: None,
            legacy_key_id: None,
        };
        let derived = match resolve_desired(&cfg, "b").unwrap() {
            DesiredEncryption::Proxy { key_id } => key_id,
            other => panic!("expected proxy, got {other:?}"),
        };
        assert_eq!(derived.len(), 16, "16 hex chars of sha256");

        // Proxy without key = reject.
        cfg.backend_encryption = BackendEncryptionConfig::Aes256GcmProxy {
            key: None,
            key_id: None,
            legacy_key: None,
            legacy_key_id: None,
        };
        assert!(resolve_desired(&cfg, "b").unwrap_err().contains("no key"));

        // SSE = unsupported.
        cfg.backend_encryption = BackendEncryptionConfig::SseS3 {
            legacy_key: None,
            legacy_key_id: None,
        };
        assert!(resolve_desired(&cfg, "b").unwrap_err().contains("sse-s3"));
    }
}
