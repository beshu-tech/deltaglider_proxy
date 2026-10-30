// SPDX-License-Identifier: BUSL-1.1

//! Per-bucket write policy and keyring of the encrypting wrapper (issue
//! #116): one wrapper per raw backend, a write key per REAL bucket, and
//! reads that pick the key by the id stamped on the object.

use super::*;
use crate::storage::filesystem::FilesystemBackend;
use crate::storage::{ObjectAt, ObjectVariant};
use std::collections::HashMap;

fn key(byte: u8) -> EncryptionKey {
    EncryptionKey::from_hex(&format!("{byte:02x}").repeat(32)).unwrap()
}

/// Passthrough metadata as the engine writes it (a seal needs the hashes;
/// the wrapper does not check them against the body).
fn meta(name: &str, size: u64) -> FileMetadata {
    FileMetadata::new_passthrough(name.into(), "ab".repeat(32), "cd".repeat(16), size, None)
}

/// The backend key `a` (id `local-a`), and `db-archive` with its own key
/// `b` (id `db-archive-b`).
fn two_keys() -> EncryptionConfig {
    EncryptionConfig {
        key: Some(key(0xaa)),
        key_id: Some("local-a".into()),
        buckets: HashMap::from([(
            "db-archive".to_string(),
            BucketKeys {
                key: Some(key(0xbb)),
                key_id: Some("db-archive-b".into()),
                ..Default::default()
            },
        )]),
        keyring: HashMap::from([("db-archive-b".to_string(), key(0xbb))]),
        ..Default::default()
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    wrapper: EncryptingBackend<FilesystemBackend>,
    cfg: Arc<ArcSwap<EncryptionConfig>>,
}

async fn rig(cfg: EncryptionConfig) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let fs = FilesystemBackend::new(dir.path().to_path_buf())
        .await
        .unwrap();
    let cfg = Arc::new(ArcSwap::new(Arc::new(cfg)));
    let wrapper = EncryptingBackend::new(fs, cfg.clone());
    for b in ["releases", "db-archive"] {
        wrapper.create_bucket(b).await.unwrap();
    }
    Rig {
        _dir: dir,
        wrapper,
        cfg,
    }
}

impl Rig {
    async fn put(&self, bucket: &str, name: &str, body: &'static [u8]) -> Result<(), StorageError> {
        self.wrapper
            .put_passthrough(
                bucket,
                "",
                name,
                bytes::Bytes::from_static(body),
                &meta(name, body.len() as u64),
            )
            .await
    }

    async fn get(&self, bucket: &str, name: &str) -> Vec<u8> {
        self.wrapper
            .get_passthrough(bucket, "", name)
            .await
            .unwrap()
    }

    /// The id of the key that encrypted the object, as the wrapper
    /// presents it (`None`: plaintext).
    async fn stamp(&self, bucket: &str, name: &str) -> Option<String> {
        let shown = self
            .wrapper
            .get_passthrough_metadata(bucket, "", name)
            .await
            .unwrap();
        (shown
            .user_metadata
            .get(ENCRYPTION_MARKER_KEY)
            .map(String::as_str)
            == Some(PRESENTED_MARKER_VALUE))
        .then(|| shown.user_metadata[ENCRYPTION_KEY_ID_KEY].clone())
    }
}

#[tokio::test]
async fn each_bucket_writes_with_its_own_key() {
    let r = rig(two_keys()).await;
    r.put("releases", "x", b"backend key").await.unwrap();
    r.put("db-archive", "x", b"bucket key").await.unwrap();
    assert_eq!(r.stamp("releases", "x").await.as_deref(), Some("local-a"));
    assert_eq!(
        r.stamp("db-archive", "x").await.as_deref(),
        Some("db-archive-b")
    );
    assert_eq!(r.get("releases", "x").await, b"backend key");
    assert_eq!(r.get("db-archive", "x").await, b"bucket key");
}

/// The override arrives after the bucket's history: the old objects read
/// under the backend key; the new ones use the bucket key.
#[tokio::test]
async fn a_bucket_reads_its_history_under_the_backend_key() {
    let mut before = two_keys();
    before.buckets.clear();
    before.keyring.clear();
    let r = rig(before).await;
    r.put("db-archive", "old", b"history").await.unwrap();
    assert_eq!(
        r.stamp("db-archive", "old").await.as_deref(),
        Some("local-a")
    );
    r.cfg.store(Arc::new(two_keys()));
    assert_eq!(r.get("db-archive", "old").await, b"history");
    r.put("db-archive", "new", b"after").await.unwrap();
    assert_eq!(
        r.stamp("db-archive", "new").await.as_deref(),
        Some("db-archive-b")
    );
}

/// The override is gone from the write policy (mode none), its key stays
/// in the keyring: the ciphertext still reads, new writes are plaintext.
#[tokio::test]
async fn a_retired_bucket_key_still_reads_from_the_keyring() {
    let r = rig(two_keys()).await;
    r.put("db-archive", "old", b"under b").await.unwrap();
    let mut after = two_keys();
    after.key = None;
    after.key_id = None;
    after.buckets.insert(
        "db-archive".into(),
        BucketKeys {
            legacy_key_id: Some("db-archive-b".into()),
            ..Default::default()
        },
    );
    r.cfg.store(Arc::new(after));
    assert_eq!(r.get("db-archive", "old").await, b"under b");
    r.put("db-archive", "new", b"plain").await.unwrap();
    assert_eq!(r.stamp("db-archive", "new").await, None);
    // A range read takes the keyed path for this bucket.
    let crate::storage::RangeRead { stream, len, .. } = r
        .wrapper
        .open_passthrough_range("db-archive", "", "old", 0, 2)
        .await
        .unwrap();
    use futures::StreamExt;
    let body: Vec<u8> = stream.map(|c| c.unwrap().to_vec()).concat().await;
    assert_eq!((len, body.as_slice()), (3, &b"und"[..]));
}

/// A bucket with its own override decides with its own
/// `plaintext_reads`, not the backend's: an unsealed object is served only
/// where the bucket's rule says `serve`.
#[tokio::test]
async fn a_bucket_override_decides_whether_plaintext_is_served() {
    for (backend_serves, bucket_serves) in [(false, false), (true, false), (false, true)] {
        let mut cfg = two_keys();
        cfg.serve_plaintext = backend_serves;
        cfg.buckets.get_mut("db-archive").unwrap().serve_plaintext = bucket_serves;
        let r = rig(cfg).await;
        for b in ["releases", "db-archive"] {
            r.wrapper
                .inner
                .put_passthrough(
                    b,
                    "",
                    "plain",
                    bytes::Bytes::from_static(b"p"),
                    &meta("plain", 1),
                )
                .await
                .unwrap();
        }
        let label = format!("backend {backend_serves} bucket {bucket_serves}");
        let w = &r.wrapper;
        let read = |b| async move {
            (
                w.get_passthrough(b, "", "plain").await.is_ok(),
                w.get_passthrough_metadata(b, "", "plain").await.is_ok(),
                w.open_passthrough_range(b, "", "plain", 0, 0).await.is_ok(),
            )
        };
        let s = backend_serves;
        assert_eq!(read("releases").await, (s, s, s), "{label}");
        let s = bucket_serves;
        assert_eq!(read("db-archive").await, (s, s, s), "{label}");
    }
}

#[tokio::test]
async fn a_keyless_bucket_refuses_writes_and_names_itself() {
    let mut cfg = two_keys();
    cfg.buckets.insert(
        "db-archive".into(),
        BucketKeys {
            write_mode: WriteMode::Refuse,
            ..Default::default()
        },
    );
    let r = rig(cfg).await;
    let err = r.put("db-archive", "x", b"no").await.unwrap_err();
    assert!(err.to_string().contains("bucket 'db-archive'"), "{err}");
    r.put("releases", "x", b"yes").await.unwrap();
}

/// A server-side copy keeps the stored bytes, so it is valid only when the
/// DESTINATION bucket would store them the same way.
#[tokio::test]
async fn copy_within_follows_the_destination_buckets_policy() {
    let r = rig(two_keys()).await;
    r.put("releases", "x", b"under a").await.unwrap();
    let source = r
        .wrapper
        .get_passthrough_metadata("releases", "", "x")
        .await
        .unwrap();
    let at = |bucket| ObjectAt {
        bucket,
        prefix: "",
        filename: "x",
    };
    let into_override = r
        .wrapper
        .copy_within(
            ObjectVariant::Passthrough,
            at("releases"),
            ObjectAt {
                filename: "y",
                ..at("db-archive")
            },
            &source,
            &meta("y", 7),
        )
        .await
        .unwrap();
    assert_eq!(
        into_override,
        crate::storage::CopyOutcome::Unsupported,
        "db-archive writes under its own key: the bytes under 'local-a' must be re-encrypted"
    );
}

/// The write policy at COMPLETE decides: buffered parts into a bucket that
/// encrypts are encrypted; native parts (already stored in plaintext) are
/// refused.
#[tokio::test]
async fn multipart_complete_follows_the_buckets_policy() {
    let mut before = two_keys();
    before.key = None;
    before.key_id = None;
    before.buckets.clear();
    let r = rig(before).await;
    let upload = r
        .wrapper
        .create_multipart_upload("db-archive", "", "big", &meta("big", 6))
        .await
        .unwrap();
    assert!(!upload.native);
    // A config apply gives the bucket a key during the upload.
    let mut after = two_keys();
    after.key = None;
    after.key_id = None;
    r.cfg.store(Arc::new(after));
    r.wrapper
        .complete_multipart_upload(
            &upload,
            "",
            "big",
            &[],
            &[
                bytes::Bytes::from_static(b"abc"),
                bytes::Bytes::from_static(b"def"),
            ],
            &meta("big", 6),
        )
        .await
        .unwrap();
    assert_eq!(
        r.stamp("db-archive", "big").await.as_deref(),
        Some("db-archive-b")
    );
    assert_eq!(r.get("db-archive", "big").await, b"abcdef");

    let native = MultipartUpload {
        native: true,
        ..upload
    };
    let err = r
        .wrapper
        .complete_multipart_upload(&native, "", "big", &[], &[], &meta("big", 6))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("changed during the upload"),
        "{err}"
    );
}

/// Review of P3: the witness gate lived in `write_key`, so the writes that
/// do not encrypt (a folder marker, a metadata rewrite: on S3 a self-copy
/// with this node's SSE headers) went through on a refused bucket.
#[tokio::test]
async fn a_witness_refusal_stops_every_write_also_a_folder_marker() {
    let dir = tempfile::tempdir().unwrap();
    let fs = FilesystemBackend::new(dir.path().to_path_buf())
        .await
        .unwrap();
    let gate = Arc::new(WitnessGate::default());
    let wrapper = EncryptingBackend::new(fs, Arc::new(ArcSwap::new(Arc::new(two_keys()))))
        .with_witness("local-disk", gate.clone());
    wrapper.create_bucket("releases").await.unwrap();
    gate.activate(std::time::Duration::from_secs(60));
    gate.synced();
    gate.set(WitnessGateState {
        refused: HashMap::from([(
            ("local-disk".to_string(), "releases".to_string()),
            "mismatch".to_string(),
        )]),
        witnessed: Default::default(),
    });
    let marker = wrapper.put_directory_marker("releases", "dir/").await;
    assert!(
        matches!(marker, Err(StorageError::WritesRefused(ref m)) if m == "mismatch"),
        "{marker:?}"
    );
    let put = wrapper
        .put_passthrough(
            "releases",
            "",
            "x.txt",
            bytes::Bytes::from_static(b"x"),
            &meta("x.txt", 1),
        )
        .await;
    assert!(
        matches!(put, Err(StorageError::WritesRefused(_))),
        "{put:?}"
    );
}

/// Source guard: every write method of `StorageBackend` is overridden by
/// the encrypting wrapper and calls the witness check first. A new write
/// method fails here until it does.
#[test]
fn every_write_method_checks_the_witness() {
    let traits = std::fs::read_to_string("src/storage/traits.rs").unwrap();
    let wrapper = std::fs::read_to_string("src/storage/encrypting/mod.rs").unwrap();
    // Trait defaults that only delegate to a checked method,
    // `write_refusal`, the witness query itself (it writes nothing), and
    // `write_key_claim`, which the wrapper's own check sends to the raw
    // backend below it (nothing above the wrapper writes a claim).
    let delegating = [
        "put_reference_from_file",
        "write_refusal",
        "write_key_claim",
    ];
    let prefixes = [
        "put_",
        "write_",
        "copy_",
        "create_multipart",
        "upload_part",
        "complete_multipart",
    ];
    let mut missing = Vec::new();
    for line in traits.lines() {
        let t = line.trim_start();
        let Some(rest) = t
            .strip_prefix("fn ")
            .or_else(|| t.strip_prefix("async fn "))
        else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !prefixes.iter().any(|p| name.starts_with(p)) || delegating.contains(&name.as_str()) {
            continue;
        }
        let Some(at) = wrapper.find(&format!("    async fn {name}(")) else {
            missing.push(format!("{name}: not overridden"));
            continue;
        };
        let body_start = wrapper[at..].find("{\n").map(|i| at + i).unwrap();
        let first = wrapper[body_start + 2..].lines().next().unwrap_or("");
        if !first.contains("self.check_write(") && !first.contains("self.check_write_unless(") {
            missing.push(format!("{name}: first line is `{}`", first.trim()));
        }
    }
    assert!(missing.is_empty(), "{missing:?}");
}
