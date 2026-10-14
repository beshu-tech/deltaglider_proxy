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

fn meta(name: &str, size: u64) -> FileMetadata {
    FileMetadata::fallback(
        name.into(),
        size,
        "md5".into(),
        chrono::Utc::now(),
        None,
        crate::types::StorageInfo::Passthrough,
    )
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

    /// The key id stamped on the stored object (`None`: plaintext).
    async fn stamp(&self, bucket: &str, name: &str) -> Option<String> {
        let raw = self
            .wrapper
            .get_passthrough_metadata(bucket, "", name)
            .await
            .unwrap();
        is_encrypted(&raw).then(|| stamped_key_id(&raw).unwrap().to_string())
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
    let (stream, len) = r
        .wrapper
        .get_passthrough_stream_range("db-archive", "", "old", 0, 2)
        .await
        .unwrap();
    use futures::StreamExt;
    let body: Vec<u8> = stream.map(|c| c.unwrap().to_vec()).concat().await;
    assert_eq!((len, body.as_slice()), (3, &b"und"[..]));
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
