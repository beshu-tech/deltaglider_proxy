// SPDX-License-Identifier: BUSL-1.1

//! One-off bucket maintenance jobs.
//!
//! Three kinds:
//!
//! - **`reencrypt`** — rewrite every object in a bucket through the engine
//!   so its at-rest encryption state matches the backend's CURRENT config.
//!   One uniform, canned procedure covers all three transitions: enable
//!   (encrypt plaintext objects), rotate (re-encrypt under the new key),
//!   disable (decrypt back to plaintext). Objects already in the desired
//!   state are skipped, which makes the job idempotent and resumable.
//! - **`migrate`** (see [`migrate`]) — move a bucket to another backend.
//! - **`backfill-metadata`** (see [`backfill`]) — stamp the canonical DG
//!   metadata onto passthrough objects that lack it, in place (S3
//!   self-copy / xattr write), reading bytes only to compute the hashes.
//!
//! Architecture (mirrors `src/replication/` / `src/lifecycle/`):
//!
//! - [`store`] — durable job rows in the SQLCipher JobsDb: phase +
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
//! The JobsDb is per-instance and the write gate is in-process memory.
//! In a multi-instance deployment, other instances do not gate the
//! bucket — the same single-runner posture replication and lifecycle
//! already have. The leader lease prevents double-running on one
//! instance's DB, nothing more.

pub mod backfill;
pub mod conversion;
pub mod delete;
pub mod gate;
pub mod live;
pub mod migrate;
mod paged;
pub mod store;
pub mod worker;

/// Wake the maintenance worker after a job row changes. Every lane waits
/// on the same `Notify`: `notify_waiters` wakes each lane that waits now,
/// and `notify_one` keeps a permit for a lane that is between its drain and
/// its wait. A bare `notify_one` woke one lane only, often the wrong one.
pub fn wake_workers(notify: &tokio::sync::Notify) {
    notify.notify_waiters();
    notify.notify_one();
}

use std::collections::HashMap;

use crate::config::{BackendEncryptionConfig, Config};
use crate::storage::encrypting::{
    ENCRYPTION_KEY_ID_KEY, ENCRYPTION_MARKER_KEY, PRESENTED_MARKER_VALUE,
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
/// Reads the bucket's encryption through `Config::encryption_for_bucket`
/// (the routing the engine uses; the singleton's synthetic name `default`
/// is the name the engine mixes into the derived key id, so the ids match
/// what the wrapper stamps).
///
/// Returns `Err(reason)` for configurations the v1 job cannot verify or
/// normalize: SSE modes (encryption happens AWS-side, no per-object
/// proxy marker to check) and proxy mode without a key (writes would be
/// plaintext — a misconfiguration to fix before running a job).
pub fn resolve_desired(config: &Config, bucket: &str) -> Result<DesiredEncryption, String> {
    let resolved = config
        .encryption_for_bucket(bucket)
        .ok_or_else(|| format!("bucket '{bucket}' routes to an unknown backend"))?;
    let enc = resolved.config.clone();
    // Who owns the encryption, for the messages: a bucket override (issue
    // #116; its key id is always explicit) or the backend.
    let owner = match &resolved.owner {
        Some(b) => format!("bucket '{b}'"),
        None => format!("backend '{}'", resolved.backend),
    };

    match enc {
        BackendEncryptionConfig::None { .. } => Ok(DesiredEncryption::Plain),
        BackendEncryptionConfig::Aes256GcmProxy { .. } => {
            // THE key resolution (config::resolve_site_keys): the id the
            // wrapper stamps.
            match resolved.site_keys().primary {
                None => Err(format!(
                    "{owner} is in aes256-gcm-proxy mode but has no key configured — \
                     writes are refused; fix the key before re-encrypting"
                )),
                Some(Err(e)) => Err(e),
                Some(Ok(k)) => Ok(DesiredEncryption::Proxy { key_id: k.key_id }),
            }
        }
        BackendEncryptionConfig::SseKms { .. } | BackendEncryptionConfig::SseS3 { .. } => {
            Err(format!(
                "{owner} uses {} — AWS-side encryption leaves no per-object \
                 proxy marker, so the re-encrypt job cannot verify object state (unsupported in v1)",
                enc.mode_tag()
            ))
        }
    }
}

/// Does this object's at-rest state differ from the desired one?
///
/// Truth table (marker = `dg-encrypted: v2`, as the wrapper presents an
/// encrypted object; one key has one id on a backend, so the id names the key):
///
/// | marker | desired       | rewrite? |
/// |--------|---------------|----------|
/// | absent | Plain         | no       |
/// | absent | Proxy         | yes      |
/// | present| Plain         | yes (decrypt) |
/// | present| Proxy(k)      | iff stamped key-id ≠ k (or absent) |
pub fn needs_rewrite(user_metadata: &HashMap<String, String>, desired: &DesiredEncryption) -> bool {
    match desired {
        DesiredEncryption::Plain => matches!(
            user_metadata.get(ENCRYPTION_MARKER_KEY).map(String::as_str),
            Some(PRESENTED_MARKER_VALUE)
        ),
        DesiredEncryption::Proxy { key_id } => !stamped_with_key_id(user_metadata, key_id),
    }
}

/// Is this object encrypted under the key `key_id`? Its presented
/// `dg-encryption-key-id` is the id of the key that the seal's fingerprint
/// names, so the object counts when that id equals `key_id`. A plaintext
/// object never counts. This answers "does anything still need the legacy
/// key".
pub fn stamped_with_key_id(user_metadata: &HashMap<String, String>, key_id: &str) -> bool {
    let encrypted = matches!(
        user_metadata.get(ENCRYPTION_MARKER_KEY).map(String::as_str),
        Some(PRESENTED_MARKER_VALUE)
    );
    encrypted && user_metadata.get(ENCRYPTION_KEY_ID_KEY).map(String::as_str) == Some(key_id)
}

// `strip_encryption_markers` lives in `storage::encrypting` (where the marker
// consts do) — the single home shared by the re-encrypt job here AND the
// delta-passthrough ship in `transfer`. Re-exported so `super::` callers in this
// module are unaffected.
pub(crate) use crate::storage::encrypting::strip_encryption_markers;

/// Pure: the terminal status of a job that ran to its end. A job with
/// per-object failures is never plain `completed`: `completed_with_errors`
/// when some objects went through, `failed` when none did. Returns the
/// status and a summary for `last_error`.
pub fn settle_status(done: i64, skipped: i64, failed: i64) -> (&'static str, Option<String>) {
    if failed <= 0 {
        return ("completed", None);
    }
    let summary = format!("{failed} object(s) failed — see the job's Failures tab");
    if done <= 0 && skipped <= 0 {
        ("failed", Some(summary))
    } else {
        ("completed_with_errors", Some(summary))
    }
}

/// Display percent for a job ROW: 100 once completed, otherwise
/// [`progress_percent`]. The one home for the rule both admin views
/// (jobs.rs + the session-light bucket status) share.
pub fn display_percent(job: &store::MaintenanceJob) -> Option<u8> {
    match job.status.as_str() {
        "completed" | "completed_with_errors" => Some(100),
        _ if job.kind == "migrate" => migrate_percent(
            &job.phase,
            job.objects_total,
            job.objects_done + job.objects_skipped,
            live::verified(job),
        ),
        _ => progress_percent(
            &job.phase,
            job.objects_total,
            job.objects_done,
            job.objects_skipped,
        ),
    }
}

/// Percent of a migrate: the copy is the first half (0-49), the verify the
/// second (50-98), the flip and cleanup 99. `total` is the source's object
/// count (counted next to the copy, exact after it), `copied` the objects
/// copied or skipped, `verified` the objects that this run of the verify
/// phase checked (`None`: not running here). `None` while the total is
/// unknown (stage, and the copy until the count ends).
pub fn migrate_percent(
    phase: &str,
    total: Option<i64>,
    copied: i64,
    verified: Option<i64>,
) -> Option<u8> {
    let share = |n: i64, t: i64, width: i64| (n.clamp(0, t) * width / t) as u8;
    match phase {
        "copy" => {
            let t = total?;
            Some(if t <= 0 { 49 } else { share(copied, t, 49) })
        }
        "verify" => {
            let t = total?;
            Some(if t <= 0 {
                98
            } else {
                50 + share(verified.unwrap_or(0), t, 48)
            })
        }
        "flip" | "cleanup" => Some(99),
        _ => None,
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
        // Migrate phases: see `migrate_percent`.
        "stage" | "copy" | "verify" => None,
        "references" | "purge-versions" | "flip" | "cleanup" => Some(99),
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

    /// Every lane of the worker waits on one `Notify`; a bare `notify_one`
    /// wakes one lane only. Every wake goes through `wake_workers`.
    #[test]
    fn every_worker_wake_goes_through_wake_workers() {
        let mut offenders = Vec::new();
        let mut dirs = vec![std::path::PathBuf::from("src")];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "rs")
                    && path != std::path::Path::new("src/maintenance/mod.rs")
                {
                    let text = std::fs::read_to_string(&path).unwrap();
                    if text.contains("maintenance_notify.notify_") {
                        offenders.push(path.display().to_string());
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "wake the workers with wake_workers: {offenders:?}"
        );
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

        // an encrypted object, as the wrapper presents it
        let enc = meta(&[("dg-encrypted", "v2"), ("dg-encryption-key-id", "kid-1")]);
        assert!(needs_rewrite(&enc, &plain), "encrypted + Plain = decrypt");
        assert!(!needs_rewrite(&enc, &proxy), "matching key id = no-op");

        // key rotation: another key
        let old_key = meta(&[("dg-encrypted", "v2"), ("dg-encryption-key-id", "kid-0")]);
        assert!(
            needs_rewrite(&old_key, &proxy),
            "key-id mismatch = re-encrypt"
        );

        // the stored marker of the previous format is never presented: if
        // it shows, it is not the presented v2 marker
        let stored = meta(&[("dg-encrypted", "aes-256-gcm-chunked-v1")]);
        assert!(needs_rewrite(&stored, &proxy));

        // unknown marker value is NOT treated as encrypted
        let bogus = meta(&[("dg-encrypted", "something-else")]);
        assert!(!needs_rewrite(&bogus, &plain));
        assert!(needs_rewrite(&bogus, &proxy));
    }

    #[test]
    fn stamped_with_key_id_truth_table() {
        let old = meta(&[("dg-encrypted", "v2"), ("dg-encryption-key-id", "kid-old")]);
        assert!(stamped_with_key_id(&old, "kid-old"));
        assert!(!stamped_with_key_id(&old, "kid-new"));
        // A key id without the marker is not an encrypted body.
        assert!(!stamped_with_key_id(
            &meta(&[("dg-encryption-key-id", "kid-old")]),
            "kid-old"
        ));
        assert!(!stamped_with_key_id(
            &meta(&[("dg-encrypted", "aes-256-gcm-chunked-v1")]),
            "kid-old"
        ));
        assert!(!stamped_with_key_id(&meta(&[]), "kid-old"));
    }

    #[test]
    fn strip_removes_both_markers_only() {
        let mut m = meta(&[
            ("dg-encrypted", "v2"),
            ("dg-encryption-key-id", "kid"),
            ("dg-sealed", "2.x"),
            ("custom", "keep-me"),
        ]);
        strip_encryption_markers(&mut m);
        assert!(!m.contains_key("dg-encrypted"));
        assert!(!m.contains_key("dg-encryption-key-id"));
        assert!(!m.contains_key("dg-sealed"));
        assert_eq!(m.get("custom").map(String::as_str), Some("keep-me"));
    }

    #[test]
    fn settle_status_never_calls_a_job_with_failures_completed() {
        assert_eq!(settle_status(10, 5, 0), ("completed", None));
        assert_eq!(settle_status(0, 0, 0), ("completed", None));
        let (s, e) = settle_status(10, 0, 2);
        assert_eq!(s, "completed_with_errors");
        assert!(e.unwrap().starts_with("2 object(s) failed"));
        assert_eq!(settle_status(0, 3, 1).0, "completed_with_errors");
        assert_eq!(settle_status(0, 0, 4).0, "failed", "all objects failed");
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

    /// Maint M1: a migrate row stayed indeterminate for the whole job.
    #[test]
    fn migrate_percent_is_determinate_once_the_total_is_known() {
        assert_eq!(migrate_percent("stage", None, 0, None), None);
        assert_eq!(
            migrate_percent("copy", None, 500, None),
            None,
            "count running"
        );
        assert_eq!(migrate_percent("copy", Some(1000), 0, None), Some(0));
        assert_eq!(migrate_percent("copy", Some(1000), 500, None), Some(24));
        assert_eq!(migrate_percent("copy", Some(1000), 1000, None), Some(49));
        assert_eq!(
            migrate_percent("copy", Some(10), 50, None),
            Some(49),
            "capped"
        );
        assert_eq!(migrate_percent("verify", Some(1000), 1000, None), Some(50));
        assert_eq!(
            migrate_percent("verify", Some(1000), 1000, Some(500)),
            Some(74)
        );
        assert_eq!(
            migrate_percent("verify", Some(1000), 1000, Some(1000)),
            Some(98)
        );
        assert_eq!(migrate_percent("verify", Some(0), 0, None), Some(98));
        assert_eq!(migrate_percent("flip", Some(1), 1, None), Some(99));
        assert_eq!(migrate_percent("cleanup", None, 0, None), Some(99));
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
            plaintext_reads: Default::default(),
        };
        assert_eq!(
            resolve_desired(&cfg, "b").unwrap(),
            DesiredEncryption::Proxy {
                key_id: "explicit-id".into()
            }
        );

        // A derived key id is `fp-` and the key's fingerprint.
        cfg.backend_encryption = BackendEncryptionConfig::Aes256GcmProxy {
            key: Some("00".repeat(32)),
            key_id: None,
            legacy_key: None,
            legacy_key_id: None,
            plaintext_reads: Default::default(),
        };
        let derived = match resolve_desired(&cfg, "b").unwrap() {
            DesiredEncryption::Proxy { key_id } => key_id,
            other => panic!("expected proxy, got {other:?}"),
        };
        assert!(
            derived.starts_with("fp-") && derived.len() == 19,
            "{derived}"
        );

        // Proxy without key = reject.
        cfg.backend_encryption = BackendEncryptionConfig::Aes256GcmProxy {
            key: None,
            key_id: None,
            legacy_key: None,
            legacy_key_id: None,
            plaintext_reads: Default::default(),
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
