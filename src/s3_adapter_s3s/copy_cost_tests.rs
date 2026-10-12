// SPDX-License-Identifier: BUSL-1.1

//! What a client CopyObject costs, and that the cheap path gives the same
//! object. A CopyObject of a delta reconstructed the source (a GET of the
//! whole reference, one xdelta3 decode) and stored it again (one xdelta3
//! encode) to write a delta of a few dozen bytes. It now ships the `.delta`
//! verbatim.

use super::super::multipart::adapter_test_support::{request, service};
use super::*;
use crate::deltaglider::{DeltaGliderEngine, DynEngine};
use crate::storage::{DynStorageBackend, EncryptingBackend, EncryptionConfig, EncryptionKey};

/// A REPLACE directive: the content type and the user metadata.
type Replace<'a> = Option<(Option<&'a str>, &'a [(&'a str, &'a str)])>;

fn copy_input(
    source: (&str, &str),
    dest: (&str, &str),
    replace: Replace<'_>,
) -> s3s::dto::CopyObjectInput {
    let mut b = s3s::dto::CopyObjectInput::builder()
        .bucket(dest.0.into())
        .key(dest.1.into())
        .copy_source(s3s::dto::CopySource::Bucket {
            bucket: source.0.into(),
            key: source.1.into(),
            version_id: None,
        });
    if let Some((content_type, meta)) = replace {
        b = b
            .metadata_directive(Some(s3s::dto::MetadataDirective::from_static(
                s3s::dto::MetadataDirective::REPLACE,
            )))
            .content_type(content_type.map(String::from))
            .metadata(Some(
                meta.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ));
    }
    b.build().unwrap()
}

fn engine_on(
    backend: Box<DynStorageBackend<'static>>,
    config: &crate::config::Config,
    metrics: &Arc<crate::metrics::Metrics>,
) -> DynEngine {
    DeltaGliderEngine::new_with_backend(Arc::new(backend), config, Some(metrics.clone()))
}

fn encrypted(
    inner: impl crate::storage::StorageBackend + 'static,
) -> Box<DynStorageBackend<'static>> {
    let cfg = Arc::new(arc_swap::ArcSwap::new(Arc::new(EncryptionConfig {
        key: Some(EncryptionKey::from_hex(&"ab".repeat(32)).unwrap()),
        key_id: Some("kid".into()),
        ..Default::default()
    })));
    DynStorageBackend::new_box(EncryptingBackend::new(inner, cfg))
}

/// xdelta3 runs so far: (encodes, decodes).
fn codec_runs(m: &crate::metrics::Metrics) -> (u64, u64) {
    (
        m.delta_encode_duration_seconds.get_sample_count(),
        m.delta_decode_duration_seconds.get_sample_count(),
    )
}

struct Fixture {
    state: Arc<AppState>,
    svc: DeltaGliderS3Service,
    metrics: Arc<crate::metrics::Metrics>,
    keys: Vec<String>,
}

impl Fixture {
    async fn new(backend: Box<DynStorageBackend<'static>>, config: crate::config::Config) -> Self {
        let metrics = Arc::new(crate::metrics::Metrics::new());
        let engine = engine_on(backend, &config, &metrics);
        for b in ["c1", "c2"] {
            engine.create_bucket(b).await.unwrap();
        }
        engine.create_bucket("b").await.unwrap();
        let keys = crate::deltaglider::store_deltas(&engine, "run", 3).await;
        let state = AppState::for_tests(engine);
        let svc = service(state.clone());
        Self {
            state,
            svc,
            metrics,
            keys,
        }
    }

    fn engine(&self) -> Arc<DynEngine> {
        self.state.engine.load_full()
    }

    async fn copy(
        &self,
        source: (&str, &str),
        dest: (&str, &str),
        replace: Replace<'_>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CopyObjectOutput>> {
        copy::copy_object(&self.svc, request(copy_input(source, dest, replace))).await
    }

    /// The same copy through the store (the path every copy took before).
    async fn copy_through_store(&self, source: (&str, &str), dest: (&str, &str)) {
        let engine = self.state.engine.load();
        let head = engine.head(source.0, source.1).await.unwrap();
        let input = copy_input(source, dest, None);
        copy::copy_through_store(&self.svc, &engine, source, &input, "COPY", &head)
            .await
            .unwrap();
    }
}

/// HEAD fields, created-at aside, as JSON (field by field on mismatch).
fn head_fields(meta: &FileMetadata) -> serde_json::Value {
    let mut v = serde_json::to_value(meta).unwrap();
    v.as_object_mut().unwrap().remove("created_at");
    v
}

/// P5.1: a CopyObject of a delta into its own folder sends no GET of the
/// reference and runs no xdelta3: the `.delta` moves verbatim. It cost
/// 12 requests, a GET of the whole reference and two xdelta3 runs.
#[tokio::test]
async fn a_copy_of_a_delta_ships_the_delta_verbatim() {
    let (s3, fake) = crate::storage::fake_s3_backend().await;
    let f = Fixture::new(
        DynStorageBackend::new_box(s3),
        crate::config::Config::default(),
    )
    .await;
    // Warm the bucket-exists cache.
    f.copy(("b", &f.keys[0]), ("b", "run/warm.zip"), None)
        .await
        .unwrap();
    let before = codec_runs(&f.metrics);
    fake.clear();
    let out = f
        .copy(("b", &f.keys[1]), ("b", "run/copy.zip"), None)
        .await
        .unwrap();
    let r: Vec<String> = fake
        .requests()
        .into_iter()
        .filter(|x| !x.contains("/.dg/") && !x.contains("prefix=.dg"))
        .collect();
    assert_eq!(codec_runs(&f.metrics), before, "no xdelta3: {r:#?}");
    assert!(!r.iter().any(|x| x.contains("reference.bin?")), "{r:#?}");
    let engine = f.engine();
    let source = engine.head("b", &f.keys[1]).await.unwrap();
    let crate::types::StorageInfo::Delta { delta_size, .. } = source.storage_info else {
        panic!("fixture: a delta source");
    };
    assert_eq!(fake.bytes(), (delta_size, delta_size), "{r:#?}");
    // Source HEAD (2), reference HEAD, GET, PUT, DELETE of the other
    // form (no counter read said it is absent), verify HEAD.
    assert_eq!(r.len(), 7, "{r:#?}");
    let etag = out.output.copy_object_result.unwrap().e_tag.unwrap();
    let (copied, meta) = engine.retrieve("b", "run/copy.zip").await.unwrap();
    assert_eq!(copied, engine.retrieve("b", &f.keys[1]).await.unwrap().0);
    assert_eq!(meta.etag(), source.etag());
    assert_eq!(parse_s3s_etag(&source.etag()).unwrap(), etag);
}

/// Into a folder without a reference the copy seeds the source's
/// reference there and ships the delta: still no xdelta3.
#[tokio::test]
async fn a_copy_into_a_new_folder_seeds_the_reference_without_xdelta3() {
    let (s3, _fake) = crate::storage::fake_s3_backend().await;
    let f = Fixture::new(
        DynStorageBackend::new_box(s3),
        crate::config::Config::default(),
    )
    .await;
    let before = codec_runs(&f.metrics);
    f.copy(("b", &f.keys[1]), ("c1", "other/copy.zip"), None)
        .await
        .unwrap();
    assert_eq!(codec_runs(&f.metrics).0, before.0, "no encode");
    let engine = f.engine();
    let head = engine.head("c1", "other/copy.zip").await.unwrap();
    assert!(head.is_delta());
    assert_eq!(
        engine.retrieve("c1", "other/copy.zip").await.unwrap().0,
        engine.retrieve("b", &f.keys[1]).await.unwrap().0
    );
}

/// The golden rule: the object the fast path writes is the object the
/// store path writes, HEAD field by field (created-at aside), and GET
/// returns the same bytes and ETag. On the filesystem, S3, and encrypted
/// S3.
#[tokio::test]
async fn the_fast_copy_and_the_store_copy_write_the_same_object() {
    let dir = tempfile::tempdir().unwrap();
    let (s3, _f1) = crate::storage::fake_s3_backend().await;
    let (s3_enc, _f2) = crate::storage::fake_s3_backend().await;
    let backends: Vec<(&str, Box<DynStorageBackend<'static>>)> = vec![
        (
            "filesystem",
            DynStorageBackend::new_box(
                crate::storage::FilesystemBackend::new(dir.path().to_path_buf())
                    .await
                    .unwrap(),
            ),
        ),
        ("s3", DynStorageBackend::new_box(s3)),
        ("encrypted s3", encrypted(s3_enc)),
    ];
    for (name, backend) in backends {
        let f = Fixture::new(backend, crate::config::Config::default()).await;
        // Both destination folders get the source's reference (a seed).
        for b in ["c1", "c2"] {
            f.copy(("b", &f.keys[0]), (b, "run/seed.zip"), None)
                .await
                .unwrap();
        }
        let before = codec_runs(&f.metrics);
        f.copy(("b", &f.keys[1]), ("c1", "run/copy.zip"), None)
            .await
            .unwrap();
        assert_eq!(codec_runs(&f.metrics), before, "{name}: fast path");
        f.copy_through_store(("b", &f.keys[1]), ("c2", "run/copy.zip"))
            .await;
        assert_ne!(codec_runs(&f.metrics), before, "{name}: store path");
        let engine = f.engine();
        let fast = engine.head("c1", "run/copy.zip").await.unwrap();
        let slow = engine.head("c2", "run/copy.zip").await.unwrap();
        assert_eq!(head_fields(&fast), head_fields(&slow), "{name}");
        let (fast_body, fast_meta) = engine.retrieve("c1", "run/copy.zip").await.unwrap();
        let (slow_body, slow_meta) = engine.retrieve("c2", "run/copy.zip").await.unwrap();
        assert_eq!(fast_body, slow_body, "{name}");
        assert_eq!(fast_meta.etag(), slow_meta.etag(), "{name}");
    }
}

/// The metadata rules of a client copy: COPY keeps the source's content
/// type and user metadata; REPLACE takes the request's; a multipart ETag
/// does not carry over (CopyObject answers a single-part ETag, as the
/// store path does); rule provenance stamps go; created-at is the copy
/// time.
#[tokio::test]
async fn a_client_copy_follows_the_client_metadata_rules() {
    let (s3, _fake) = crate::storage::fake_s3_backend().await;
    let f = Fixture::new(
        DynStorageBackend::new_box(s3),
        crate::config::Config::default(),
    )
    .await;
    let engine = f.engine();
    // A delta source from a multipart upload, stamped by a rule.
    let (data, _) = engine.retrieve("b", &f.keys[2]).await.unwrap();
    let mut data = data;
    data[5000] ^= 0xff;
    let meta = [
        ("team".to_string(), "qa".to_string()),
        (
            crate::transfer::REPLICATION_RULE_METADATA_KEY.to_string(),
            "r1".to_string(),
        ),
    ]
    .into();
    let stored = engine
        .store_with_multipart_etag(
            "b",
            "run/mp.zip",
            &data,
            Some("application/zip".into()),
            meta,
            "\"0123456789abcdef0123456789abcdef-2\"".into(),
        )
        .await
        .unwrap();
    assert!(stored.metadata.is_delta());
    let source = engine.head("b", "run/mp.zip").await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let before = codec_runs(&f.metrics);

    f.copy(("b", "run/mp.zip"), ("b", "run/copy.zip"), None)
        .await
        .unwrap();
    let copied = engine.head("b", "run/copy.zip").await.unwrap();
    assert!(copied.is_delta());
    assert_eq!(copied.content_type.as_deref(), Some("application/zip"));
    assert_eq!(
        copied.user_metadata.get("team").map(String::as_str),
        Some("qa")
    );
    assert!(!copied
        .user_metadata
        .contains_key(crate::transfer::REPLICATION_RULE_METADATA_KEY));
    assert_eq!(copied.multipart_etag, None);
    assert_eq!(copied.etag(), format!("\"{}\"", source.md5));
    assert!(copied.created_at > source.created_at);

    f.copy(
        ("b", "run/mp.zip"),
        ("b", "run/replaced.zip"),
        Some((Some("text/plain"), &[("owner", "dana")])),
    )
    .await
    .unwrap();
    let replaced = engine.head("b", "run/replaced.zip").await.unwrap();
    assert!(replaced.is_delta());
    assert_eq!(replaced.content_type.as_deref(), Some("text/plain"));
    assert_eq!(
        replaced.user_metadata.get("owner").map(String::as_str),
        Some("dana")
    );
    assert!(!replaced.user_metadata.contains_key("team"));
    assert_eq!(codec_runs(&f.metrics), before, "both copies: no xdelta3");
}

/// A metadata-only REPLACE self-copy of a delta rewrites the object with
/// no xdelta3, and the content stays.
#[tokio::test]
async fn a_replace_self_copy_of_a_delta_runs_no_xdelta3() {
    let (s3, _fake) = crate::storage::fake_s3_backend().await;
    let f = Fixture::new(
        DynStorageBackend::new_box(s3),
        crate::config::Config::default(),
    )
    .await;
    let engine = f.engine();
    let (body, _) = engine.retrieve("b", &f.keys[1]).await.unwrap();
    let before = codec_runs(&f.metrics);
    f.copy(
        ("b", &f.keys[1]),
        ("b", &f.keys[1]),
        Some((None, &[("x", "y")])),
    )
    .await
    .unwrap();
    assert_eq!(codec_runs(&f.metrics).0, before.0, "no encode");
    let (after, meta) = engine.retrieve("b", &f.keys[1]).await.unwrap();
    assert_eq!(after, body);
    assert!(meta.is_delta());
    assert_eq!(meta.user_metadata.get("x").map(String::as_str), Some("y"));
}

/// The cases that keep the store path: `dg-no-delta` in a REPLACE, and a
/// destination bucket with compression off. Both store the copy as a
/// passthrough object, as before.
#[tokio::test]
async fn a_copy_that_must_not_be_a_delta_goes_through_the_store() {
    let (s3, _fake) = crate::storage::fake_s3_backend().await;
    let mut config = crate::config::Config::default();
    config.buckets.insert(
        "c2".into(),
        crate::bucket_policy::BucketPolicyConfig {
            compression: Some(false),
            ..Default::default()
        },
    );
    let f = Fixture::new(DynStorageBackend::new_box(s3), config).await;
    let engine = f.engine();
    let body = engine.retrieve("b", &f.keys[1]).await.unwrap().0;
    f.copy(
        ("b", &f.keys[1]),
        ("c1", "run/plain.zip"),
        Some((None, &[("dg-no-delta", "true")])),
    )
    .await
    .unwrap();
    f.copy(("b", &f.keys[1]), ("c2", "run/plain.zip"), None)
        .await
        .unwrap();
    for b in ["c1", "c2"] {
        let (data, meta) = engine.retrieve(b, "run/plain.zip").await.unwrap();
        assert!(!meta.is_delta(), "{b}");
        assert_eq!(data, body, "{b}");
    }
}

/// A destination that tries a delta keeps the delta size limit on the
/// LOGICAL size, though the fast path moves only the small delta: the
/// answer for a too-large source does not change.
#[tokio::test]
async fn the_delta_size_limit_holds_for_a_fast_copy() {
    let (endpoint, _fake) = crate::storage::fake_s3::start().await;
    let on_fake = || async {
        let config = crate::config::BackendConfig::S3 {
            endpoint: Some(endpoint.clone()),
            region: "us-east-1".into(),
            force_path_style: true,
            access_key_id: Some("k".into()),
            secret_access_key: Some("s".into()),
            allow_local: true,
            session_token: None,
        };
        let s3 =
            crate::storage::S3Backend::new(&config, crate::storage::NativeEncryptionConfig::None)
                .await
                .unwrap();
        let b: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(s3);
        b
    };
    let metrics = Arc::new(crate::metrics::Metrics::new());
    // Write the 64 KiB fixture with the default limit.
    let writer = engine_on(on_fake().await, &crate::config::Config::default(), &metrics);
    writer.create_bucket("b").await.unwrap();
    let keys = crate::deltaglider::store_deltas(&writer, "run", 2).await;
    let config = crate::config::Config {
        max_object_size: 32 * 1024,
        ..Default::default()
    };
    let small = engine_on(on_fake().await, &config, &metrics);
    let state = AppState::for_tests(small);
    let svc = service(state);
    let err = copy::copy_object(
        &svc,
        request(copy_input(("b", &keys[1]), ("b", "run/copy.zip"), None)),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), &s3s::S3ErrorCode::EntityTooLarge);
}

/// A source overwritten after the HEAD that the conditionals judged: the
/// generation pin refuses the stale bytes with 503 SlowDown, so the client
/// retries and the retry judges the new object.
#[tokio::test]
async fn a_source_that_changes_under_a_fast_copy_answers_slowdown() {
    let (s3, _fake) = crate::storage::fake_s3_backend().await;
    let f = Fixture::new(
        DynStorageBackend::new_box(s3),
        crate::config::Config::default(),
    )
    .await;
    let engine = f.engine();
    let stale = engine.head("b", &f.keys[1]).await.unwrap();
    let (mut data, _) = engine.retrieve("b", &f.keys[2]).await.unwrap();
    data[7000] ^= 0x55;
    engine
        .store("b", &f.keys[1], &data, None, Default::default())
        .await
        .unwrap();
    let precondition = crate::deltaglider::Precondition::none();
    let err = crate::api::handlers::object_helpers::store_client_delta_copy(
        &f.state,
        crate::api::handlers::object_helpers::ClientDeltaCopy {
            source: ("b", &f.keys[1]),
            bucket: "b",
            key: "run/copy.zip",
            source_head: &stale,
            content_type: None,
            user_metadata: Default::default(),
            precondition: &precondition,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(err, crate::api::S3Error::SlowDown(_)), "{err:?}");
    assert!(engine.head("b", "run/copy.zip").await.is_err());
}
