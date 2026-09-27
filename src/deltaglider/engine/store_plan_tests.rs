// SPDX-License-Identifier: BUSL-1.1

//! The store decision, pinned for BOTH delta-eligible PUT paths: the buffered
//! `store` and the streaming `store_spooled_delta`. Every case runs through
//! both and expects the same outcome (strategy, baseline, stored variants,
//! metadata, usage accounting).

use super::*;
use crate::bucket_usage::{BucketUsage, BucketUsageRow};
use crate::config::Config;
use md5::{Digest, Md5};
use sha2::Sha256;

#[derive(Clone, Copy, Debug)]
enum Path {
    Buffered,
    Spooled,
}

const PATHS: [Path; 2] = [Path::Buffered, Path::Spooled];

struct Harness {
    _tmp: tempfile::TempDir,
    usage: Arc<BucketUsage>,
    engine: DynEngine,
}

/// Bucket `b` compresses; bucket `nocomp` has `compression: false`.
async fn harness() -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let yaml = format!(
        r#"
storage:
  backends:
    - name: local-disk
      type: filesystem
      path: {}
  buckets:
    b:
      backend: local-disk
    nocomp:
      backend: local-disk
      compression: false
"#,
        tmp.path().display()
    );
    let cfg = Config::from_yaml_str(&yaml).unwrap();
    let usage = Arc::new(BucketUsage::in_memory().unwrap());
    let engine = DeltaGliderEngine::new(&cfg, None)
        .await
        .unwrap()
        .with_bucket_usage(Some(usage.clone()));
    engine.create_bucket("b").await.unwrap();
    engine.create_bucket("nocomp").await.unwrap();
    Harness {
        _tmp: tmp,
        usage,
        engine,
    }
}

fn noise(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed;
    (0..n)
        .map(|_| {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
        })
        .collect()
}

fn near(v: &[u8]) -> Vec<u8> {
    let mut v = v.to_vec();
    v[1000..1100].fill(0xAB);
    v
}

fn user_meta() -> HashMap<String, String> {
    HashMap::from([("owner".to_string(), "ci-uploader".to_string())])
}

async fn put_as(
    h: &Harness,
    path: Path,
    bucket: &str,
    key: &str,
    data: &[u8],
    multipart_etag: Option<&str>,
) -> Result<StoreResult, EngineError> {
    let ct = Some("application/zip".to_string());
    match path {
        Path::Buffered => match multipart_etag {
            None => h.engine.store(bucket, key, data, ct, user_meta()).await,
            Some(e) => {
                h.engine
                    .store_with_multipart_etag(bucket, key, data, ct, user_meta(), e.to_string())
                    .await
            }
        },
        Path::Spooled => {
            let spool = h.engine.spool_acquire(data.len() as u64).await?;
            tokio::fs::write(spool.path(), data).await.unwrap();
            h.engine
                .store_spooled_delta(
                    bucket,
                    key,
                    &spool,
                    data.len() as u64,
                    ct,
                    user_meta(),
                    multipart_etag.map(str::to_string),
                )
                .await
        }
    }
}

async fn put(h: &Harness, path: Path, key: &str, data: &[u8]) -> StoreResult {
    put_as(h, path, "b", key, data, None)
        .await
        .unwrap_or_else(|e| panic!("{path:?} {key}: {e}"))
}

async fn has_reference(h: &Harness, bucket: &str, dsid: &str) -> bool {
    h.engine
        .storage()
        .has_reference(bucket, dsid)
        .await
        .unwrap()
}

/// Which stored variants exist at a key: (delta, passthrough).
async fn variants(h: &Harness, dsid: &str, filename: &str) -> (bool, bool) {
    let s = h.engine.storage();
    (
        s.get_delta_metadata("b", dsid, filename).await.is_ok(),
        s.get_passthrough_metadata("b", dsid, filename)
            .await
            .is_ok(),
    )
}

async fn read_back(h: &Harness, bucket: &str, key: &str) -> Vec<u8> {
    h.engine.retrieve(bucket, key).await.unwrap().0
}

fn row(h: &Harness, bucket: &str) -> (u64, u64, u64) {
    h.usage.flush_pending();
    let r = h.usage.read(bucket).unwrap().unwrap_or(BucketUsageRow {
        object_count: 0,
        logical_bytes: 0,
        stored_bytes: 0,
        last_scan_at: None,
    });
    (r.object_count, r.logical_bytes, r.stored_bytes)
}

#[tokio::test]
async fn a_key_that_is_not_delta_eligible_is_passthrough_without_a_baseline() {
    for path in PATHS {
        let h = harness().await;
        let v = noise(1, 200_000);
        let r = put(&h, path, "img/a.jpg", &v).await;
        assert_eq!(r.metadata.storage_info.label(), "passthrough", "{path:?}");
        assert_eq!(r.stored_size, v.len() as u64, "{path:?}");
        assert_eq!(r.reference_created_bytes, 0, "{path:?}");
        assert!(!has_reference(&h, "b", "img").await, "{path:?}");
        assert_eq!(read_back(&h, "b", "img/a.jpg").await, v, "{path:?}");
    }
}

#[tokio::test]
async fn a_bucket_with_compression_off_is_passthrough_without_a_baseline() {
    for path in PATHS {
        let h = harness().await;
        let v = noise(1, 200_000);
        let r = put_as(&h, path, "nocomp", "rel/a.zip", &v, None)
            .await
            .unwrap();
        assert_eq!(r.metadata.storage_info.label(), "passthrough", "{path:?}");
        assert!(!has_reference(&h, "nocomp", "rel").await, "{path:?}");
        assert_eq!(read_back(&h, "nocomp", "rel/a.zip").await, v, "{path:?}");
    }
}

/// The first member creates the baseline and stores a self-delta; a close
/// sibling is a delta; an unrelated sibling loses the ratio and is
/// passthrough, and the baseline stays for the deltas that need it.
#[tokio::test]
async fn baseline_then_delta_then_passthrough_sibling() {
    for path in PATHS {
        let h = harness().await;
        let v1 = noise(1, 200_000);
        let r1 = put(&h, path, "rel/a.zip", &v1).await;
        assert_eq!(r1.metadata.storage_info.label(), "delta", "{path:?}");
        assert_eq!(r1.reference_created_bytes, v1.len() as u64, "{path:?}");
        assert!(has_reference(&h, "b", "rel").await, "{path:?}");

        let v2 = near(&v1);
        let r2 = put(&h, path, "rel/b.zip", &v2).await;
        assert_eq!(r2.metadata.storage_info.label(), "delta", "{path:?}");
        assert!(r2.stored_size < 10_000, "{path:?}: {}", r2.stored_size);
        assert_eq!(r2.reference_created_bytes, 0, "{path:?}");

        let v3 = noise(3, 200_000);
        let r3 = put(&h, path, "rel/c.zip", &v3).await;
        assert_eq!(r3.metadata.storage_info.label(), "passthrough", "{path:?}");
        assert!(has_reference(&h, "b", "rel").await, "{path:?}");

        for (key, v) in [("rel/a.zip", &v1), ("rel/b.zip", &v2), ("rel/c.zip", &v3)] {
            assert_eq!(&read_back(&h, "b", key).await, v, "{path:?} {key}");
        }
    }
}

/// An overwrite that changes the strategy removes the other variant.
#[tokio::test]
async fn an_overwrite_that_changes_strategy_removes_the_old_variant() {
    for path in PATHS {
        let h = harness().await;
        let v1 = noise(1, 200_000);
        put(&h, path, "rel/a.zip", &v1).await;
        put(&h, path, "rel/b.zip", &near(&v1)).await;
        assert_eq!(
            variants(&h, "rel", "b.zip").await,
            (true, false),
            "{path:?}"
        );
        let other = noise(9, 200_000);
        put(&h, path, "rel/b.zip", &other).await;
        assert_eq!(
            variants(&h, "rel", "b.zip").await,
            (false, true),
            "{path:?}"
        );
        put(&h, path, "rel/b.zip", &near(&v1)).await;
        assert_eq!(
            variants(&h, "rel", "b.zip").await,
            (true, false),
            "{path:?}"
        );
        assert_eq!(read_back(&h, "b", "rel/b.zip").await, near(&v1), "{path:?}");
    }
}

/// Content type, user metadata, hashes and the multipart ETag reach the
/// stored object, whatever the strategy.
#[tokio::test]
async fn metadata_is_stored_on_every_strategy() {
    for path in PATHS {
        let h = harness().await;
        let v1 = noise(1, 200_000);
        let mpe = "\"0123456789abcdef0123456789abcdef-2\"";
        for (key, data, label) in [
            ("rel/a.zip", v1.clone(), "delta"),
            ("rel/c.zip", noise(3, 200_000), "passthrough"),
            ("img/a.jpg", noise(4, 1000), "passthrough"),
        ] {
            let r = put_as(&h, path, "b", key, &data, Some(mpe)).await.unwrap();
            let head = h.engine.head("b", key).await.unwrap();
            assert_eq!(head.storage_info.label(), label, "{path:?} {key}");
            assert_eq!(r.metadata.storage_info.label(), label, "{path:?} {key}");
            assert_eq!(head.file_size, data.len() as u64, "{path:?} {key}");
            assert_eq!(head.md5, hex::encode(Md5::digest(&data)), "{path:?} {key}");
            assert_eq!(
                head.file_sha256,
                hex::encode(Sha256::digest(&data)),
                "{path:?} {key}"
            );
            assert_eq!(head.content_type.as_deref(), Some("application/zip"));
            assert_eq!(
                head.user_metadata.get("owner").map(String::as_str),
                Some("ci-uploader"),
                "{path:?} {key}"
            );
            assert_eq!(head.etag(), mpe, "{path:?} {key}");
        }
    }
}

/// Without a multipart ETag the ETag is the body MD5, on every strategy.
#[tokio::test]
async fn a_single_put_etag_is_the_body_md5() {
    for path in PATHS {
        let h = harness().await;
        let v1 = noise(1, 200_000);
        for (key, data) in [
            ("rel/a.zip", v1.clone()),
            ("rel/c.zip", noise(3, 200_000)),
            ("img/a.jpg", noise(4, 1000)),
        ] {
            put(&h, path, key, &data).await;
            let head = h.engine.head("b", key).await.unwrap();
            let md5 = hex::encode(Md5::digest(&data));
            assert_eq!(head.etag(), format!("\"{md5}\""), "{path:?} {key}");
        }
    }
}

/// The usage counter after a sequence of PUTs is the same on both paths.
#[tokio::test]
async fn usage_accounting_is_the_same_on_both_paths() {
    let (a, b) = (harness().await, harness().await);
    let v1 = noise(1, 200_000);
    let seq = [
        ("rel/a.zip", v1.clone()),
        ("rel/b.zip", near(&v1)),
        ("rel/c.zip", noise(3, 200_000)),
        ("rel/b.zip", noise(5, 200_000)),
        ("img/a.jpg", noise(4, 1000)),
    ];
    for (key, data) in &seq {
        put(&a, Path::Buffered, key, data).await;
        put(&b, Path::Spooled, key, data).await;
    }
    assert_eq!(row(&a, "b").0, 4);
    assert_eq!(row(&a, "b"), row(&b, "b"));
}
