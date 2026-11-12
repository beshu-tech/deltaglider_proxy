// SPDX-License-Identifier: BUSL-1.1

//! THE test rig of the ingest tests (`engine/ingest_tests.rs`,
//! `s3_adapter_s3s/put_ingest_tests.rs`, `api/handlers/form_post_tests.rs`):
//! an engine over the filesystem backend with a small spool threshold, its
//! own memory budget and spool dir, and the checks that every budget is
//! whole again after a body.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::deltaglider::memory_budget::MemoryBudget;
use crate::deltaglider::{DeltaGliderEngine, DynEngine};
use crate::metrics::Metrics;
use crate::storage::{DynStorageBackend, FilesystemBackend};

/// What an [`IngestRig`] engine looks like.
pub(crate) struct RigConfig {
    pub threshold: u64,
    pub spool_bytes: u64,
    pub memory: MemoryBudget,
    pub max_object_size: Option<u64>,
    /// The backend is wrapped by the proxy's encryption, so a stored body
    /// needs a ciphertext file in the spool too.
    pub encrypted: bool,
    /// `quota_bytes` per bucket (`0` freezes it).
    pub quotas: Vec<(&'static str, u64)>,
    /// The buckets to create.
    pub buckets: Vec<&'static str>,
}

/// The temp dir, spool dir and metrics of an engine that [`IngestRig::build`]
/// made; the engine itself goes to the caller (often into an `AppState`).
pub(crate) struct IngestRig {
    _dir: tempfile::TempDir,
    pub spool_dir: PathBuf,
    pub metrics: Arc<Metrics>,
    spool_bytes: u64,
}

impl IngestRig {
    pub(crate) async fn build(cfg: RigConfig) -> (Self, DynEngine) {
        let dir = tempfile::tempdir().unwrap();
        let fs = FilesystemBackend::new(dir.path().join("data"))
            .await
            .unwrap();
        let backend: Box<DynStorageBackend<'static>> = if cfg.encrypted {
            let key = crate::storage::EncryptionKey::from_hex(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            )
            .unwrap();
            let enc = Arc::new(arc_swap::ArcSwap::new(Arc::new(
                crate::storage::EncryptionConfig {
                    key: Some(key),
                    key_id: Some("kid-1".to_string()),
                    ..Default::default()
                },
            )));
            DynStorageBackend::new_box(crate::storage::EncryptingBackend::new(fs, enc))
        } else {
            DynStorageBackend::new_box(fs)
        };
        let mut config = crate::config::Config::default();
        if let Some(max) = cfg.max_object_size {
            config.max_object_size = max.into();
        }
        config.tuning.spool_threshold_bytes = Some(cfg.threshold);
        config.tuning.spool_acquire_timeout_secs = 1;
        for (bucket, quota) in &cfg.quotas {
            config.buckets.insert(
                (*bucket).into(),
                crate::bucket_policy::BucketPolicyConfig {
                    quota_bytes: Some(*quota),
                    ..Default::default()
                },
            );
        }
        let metrics = Arc::new(Metrics::new());
        let spool_dir = dir.path().join("spool");
        let engine: DynEngine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &config, Some(metrics.clone()))
                .with_memory_budget(cfg.memory)
                .with_spool_dir(
                    crate::deltaglider::spool::SpoolDir::new(spool_dir.clone(), cfg.spool_bytes)
                        .unwrap(),
                );
        for bucket in &cfg.buckets {
            engine.create_bucket(bucket).await.unwrap();
        }
        let rig = Self {
            _dir: dir,
            spool_dir,
            metrics,
            spool_bytes: cfg.spool_bytes,
        };
        (rig, engine)
    }

    /// The bodies the ingest counted with `medium` and `reason`.
    pub(crate) fn bodies(&self, medium: &str, reason: &str) -> u64 {
        self.metrics
            .ingest_bodies_total
            .with_label_values(&[medium, reason])
            .get()
    }

    /// Nothing held: the RAM budget, the spool budget and the spool dir.
    pub(crate) fn assert_restored(&self, engine: &DynEngine, when: &str) {
        assert_eq!(engine.memory_budget().held_bytes(), 0, "{when}: RAM held");
        assert_eq!(
            engine.spool_dir().free_mib() as u64,
            self.spool_bytes / (1024 * 1024),
            "{when}: spool budget held"
        );
        let files = files_under(&self.spool_dir);
        assert!(
            files.is_empty(),
            "{when}: spool files left behind: {files:?}"
        );
    }
}

/// The regular files under `dir`, at any depth (the relay parts are in
/// subdirectories of the spool dir).
pub(crate) fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(files_under(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// A body of `n` bytes that no two offsets of a short period share.
pub(crate) fn body(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 % 251) as u8).collect()
}
