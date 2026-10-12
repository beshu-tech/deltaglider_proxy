// SPDX-License-Identifier: BUSL-1.1

//! What the read verbs send upstream, pinned per backend shape (plain S3,
//! S3 behind the encrypting wrapper, S3 behind the routing backend) and
//! per cache state (a cold engine, as after a restart or on another node;
//! a warm one). Each count is exact: a test that allows "at most" hides a
//! regression of one request.

use super::multipart::adapter_test_support::{request, service};
use super::object::get_object;
use crate::api::handlers::AppState;
use crate::deltaglider::{DeltaGliderEngine, DynEngine};
use crate::storage::{DynStorageBackend, FakeS3, StorageBackend};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Plain,
    Encrypted,
    Routed,
}

const KINDS: [Kind; 3] = [Kind::Plain, Kind::Encrypted, Kind::Routed];

type Backend = Arc<Box<DynStorageBackend<'static>>>;

fn backend_on(endpoint: &str, kind: Kind) -> Backend {
    let s3 = crate::storage::s3_test_support::for_test_endpoint(endpoint);
    Arc::new(match kind {
        Kind::Plain => DynStorageBackend::new_box(s3),
        Kind::Encrypted => {
            let cfg = Arc::new(arc_swap::ArcSwap::new(Arc::new(
                crate::storage::EncryptionConfig {
                    key: Some(crate::storage::EncryptionKey::from_hex(&"ab".repeat(32)).unwrap()),
                    key_id: Some("kid".into()),
                    ..Default::default()
                },
            )));
            DynStorageBackend::new_box(crate::storage::EncryptingBackend::new(s3, cfg))
        }
        Kind::Routed => {
            let mut backends = HashMap::new();
            backends.insert(
                "primary".to_string(),
                Arc::new(DynStorageBackend::new_box(s3)),
            );
            DynStorageBackend::new_box(
                crate::storage::RoutingBackend::new(backends, HashMap::new(), "primary".into())
                    .unwrap(),
            )
        }
    })
}

/// A fresh engine (cold metadata, reference and spool caches) on `backend`.
fn engine_on(backend: &Backend, spool_threshold: Option<u64>) -> DynEngine {
    let mut cfg = crate::config::Config::default();
    cfg.tuning.spool_threshold_bytes = spool_threshold;
    DeltaGliderEngine::new_with_backend(backend.clone(), &cfg, None)
}

fn rnd(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// A bucket `b` with a passthrough photo `pics/a.jpg` and two close
/// versions `dir/v0.zip`, `dir/v1.zip` stored as deltas against
/// `dir/.dg/reference.bin`. Returns the backend, the fake and the bodies.
struct Fixture {
    endpoint: String,
    backend: Backend,
    fake: Arc<FakeS3>,
    photo: Vec<u8>,
    versions: Vec<Vec<u8>>,
}

async fn fixture(kind: Kind) -> Fixture {
    let (endpoint, fake) = crate::storage::fake_s3::start().await;
    let backend = backend_on(&endpoint, kind);
    let writer = engine_on(&backend, None);
    writer.create_bucket("b").await.unwrap();
    let photo = rnd(200 * 1024, 7);
    writer
        .store("b", "pics/a.jpg", &photo, None, Default::default())
        .await
        .unwrap();
    let base = rnd(64 * 1024, 0x9E37_79B9_7F4A_7C15);
    let mut versions = Vec::new();
    for i in 0..2usize {
        let mut data = base.clone();
        data[1000 + i * 16..1016 + i * 16].fill(i as u8 + 1);
        let stored = writer
            .store(
                "b",
                &format!("dir/v{i}.zip"),
                &data,
                None,
                Default::default(),
            )
            .await
            .unwrap();
        assert_eq!(stored.metadata.storage_info.label(), "delta", "{kind:?}");
        versions.push(data);
    }
    Fixture {
        endpoint,
        backend,
        fake,
        photo,
        versions,
    }
}

/// An engine whose large-object threshold is 1 KiB, so the 64 KiB deltas
/// take the spooled (streaming) path.
fn large_engine_on(backend: &Backend) -> DynEngine {
    engine_on(backend, Some(1024))
}

/// The object requests since the last `clear`: not the background
/// listing-facts upkeep under `.dg/facts/`.
fn object_requests(fake: &FakeS3) -> Vec<String> {
    fake.requests()
        .into_iter()
        .filter(|r| !r.contains("/.dg/facts/") && !r.contains("prefix=.dg"))
        .map(|r| r.replace("?x-id=GetObject", ""))
        .collect()
}

/// P3.2: a cold reference load is ONE GET. The reference's recorded
/// SHA-256 came from a separate HEAD sent beside the GET.
#[tokio::test]
async fn a_cold_reference_load_is_one_get() {
    for kind in KINDS {
        let fx = fixture(kind).await;
        let engine = engine_on(&fx.backend, None);
        // Warm the object metadata, so only the reference load is cold.
        engine.head("b", "dir/v1.zip").await.unwrap();
        fx.fake.clear();
        let (data, _) = engine.retrieve("b", "dir/v1.zip").await.unwrap();
        assert_eq!(data, fx.versions[1], "{kind:?}");
        let reqs = object_requests(&fx.fake);
        let reference: Vec<&String> = reqs
            .iter()
            .filter(|r| r.ends_with("/dir/reference.bin"))
            .collect();
        assert_eq!(
            reference,
            ["GET /b/dir/reference.bin"],
            "{kind:?}: {reqs:?}"
        );
    }
}

/// A reference whose bytes do not match its own recorded SHA-256 is
/// refused (and not cached) when it is read with one GET too.
#[tokio::test]
async fn a_corrupt_reference_is_refused_on_the_one_get() {
    let fx = fixture(Kind::Plain).await;
    // Overwrite the reference bytes, keep its metadata: read it raw, then
    // write corrupt bytes with the same headers through the backend.
    let meta = fx.backend.get_reference_metadata("b", "dir").await.unwrap();
    let mut bytes = fx.backend.get_reference("b", "dir").await.unwrap();
    bytes[10] ^= 0xff;
    let proof = crate::deltaglider::RefWriteProof::for_tests();
    fx.backend
        .put_reference("b", "dir", &bytes, &meta, proof)
        .await
        .unwrap();
    let engine = engine_on(&fx.backend, None);
    let err = engine.retrieve("b", "dir/v1.zip").await.unwrap_err();
    assert!(
        matches!(
            err,
            crate::deltaglider::EngineError::ChecksumMismatch { .. }
        ),
        "{err}"
    );
}

fn count(reqs: &[String], method: &str) -> usize {
    reqs.iter().filter(|r| r.starts_with(method)).count()
}

/// Requests that name an object (not a bucket-level HeadBucket).
fn key_requests(fake: &FakeS3) -> Vec<String> {
    object_requests(fake)
        .into_iter()
        .filter(|r| {
            let path = r.split(' ').nth(1).unwrap_or("");
            path.trim_start_matches('/')
                .split_once('/')
                .is_some_and(|(_, k)| !k.is_empty())
        })
        .collect()
}

fn get(key: &str) -> s3s::dto::GetObjectInput {
    s3s::dto::GetObjectInput::builder()
        .bucket("b".into())
        .key(key.into())
        .build()
        .unwrap()
}

/// GET through the adapter; the body and the response ETag.
async fn run_get(
    svc: &super::DeltaGliderS3Service,
    input: s3s::dto::GetObjectInput,
) -> (Vec<u8>, String) {
    use futures::StreamExt;
    let resp = get_object(svc, request(input)).await.unwrap();
    let etag = resp
        .output
        .e_tag
        .as_ref()
        .map(|e| e.value().to_string())
        .unwrap_or_default();
    let mut out = Vec::new();
    if let Some(mut body) = resp.output.body {
        while let Some(c) = body.next().await {
            out.extend_from_slice(&c.unwrap());
        }
    }
    (out, etag)
}

fn svc_on(engine: DynEngine) -> super::DeltaGliderS3Service {
    service(AppState::for_tests(engine))
}

/// The HEAD-first answer of `key` on a fresh engine: what a GET returned
/// before the one-round-trip read, for the layouts that fall back to it.
async fn headed_answer(backend: &Backend, key: &str) -> (Vec<u8>, String) {
    let engine = engine_on(backend, None);
    let head = engine.head("b", key).await.unwrap();
    let (data, meta) = engine.retrieve("b", key).await.unwrap();
    assert_eq!(head.etag(), meta.etag());
    (data, meta.etag().trim_matches('"').to_string())
}

/// P3.1: a plain GET reads the metadata and the bytes in one round trip.
/// Cold passthrough: the GET of the object and the HEAD of its `.delta`
/// form, in parallel (was HEAD x2 + GET). Cold small delta: GET of the
/// delta, HEAD of the passthrough form, GET of the reference (was HEAD x2
/// + GET delta + GET reference). Warm: the cached form only, one GET.
#[tokio::test]
async fn a_plain_get_is_one_round_trip() {
    for kind in KINDS {
        let fx = fixture(kind).await;
        let svc = svc_on(engine_on(&fx.backend, None));
        let label = format!("{kind:?}");

        fx.fake.clear();
        let (body, etag) = run_get(&svc, get("pics/a.jpg")).await;
        assert_eq!(body, fx.photo, "{label}");
        assert!(!etag.is_empty(), "{label}");
        let reqs = key_requests(&fx.fake);
        assert_eq!(count(&reqs, "GET"), 1, "{label} cold passthrough: {reqs:?}");
        assert_eq!(
            count(&reqs, "HEAD"),
            1,
            "{label} cold passthrough: {reqs:?}"
        );
        assert!(
            reqs.contains(&"HEAD /b/pics/a.jpg.delta".to_string()),
            "{reqs:?}"
        );

        fx.fake.clear();
        let (again, etag2) = run_get(&svc, get("pics/a.jpg")).await;
        assert_eq!((again, etag2), (fx.photo.clone(), etag.clone()), "{label}");
        let reqs = key_requests(&fx.fake);
        assert_eq!(reqs, ["GET /b/pics/a.jpg"], "{label} warm passthrough");

        fx.fake.clear();
        let (body, _) = run_get(&svc, get("dir/v1.zip")).await;
        assert_eq!(body, fx.versions[1], "{label}");
        let reqs = key_requests(&fx.fake);
        assert_eq!(count(&reqs, "GET"), 2, "{label} cold delta: {reqs:?}");
        assert_eq!(count(&reqs, "HEAD"), 1, "{label} cold delta: {reqs:?}");
        assert!(reqs.contains(&"HEAD /b/dir/v1.zip".to_string()), "{reqs:?}");

        fx.fake.clear();
        let (body, _) = run_get(&svc, get("dir/v1.zip")).await;
        assert_eq!(body, fx.versions[1], "{label}");
        let reqs = key_requests(&fx.fake);
        assert_eq!(reqs, ["GET /b/dir/v1.zip.delta"], "{label} warm delta");
    }
}

/// The response matches the HEAD-first answer: same body, same ETag.
#[tokio::test]
async fn the_one_read_answers_as_the_head_first_read() {
    for kind in KINDS {
        let fx = fixture(kind).await;
        for key in ["pics/a.jpg", "dir/v0.zip", "dir/v1.zip"] {
            let svc = svc_on(engine_on(&fx.backend, None));
            let got = run_get(&svc, get(key)).await;
            assert_eq!(got, headed_answer(&fx.backend, key).await, "{kind:?} {key}");
        }
    }
}

/// A missing key answers NoSuchKey after the two reads.
#[tokio::test]
async fn a_missing_key_is_no_such_key() {
    let fx = fixture(Kind::Plain).await;
    let svc = svc_on(engine_on(&fx.backend, None));
    fx.fake.clear();
    let err = get_object(&svc, request(get("pics/missing.jpg")))
        .await
        .unwrap_err();
    assert_eq!(err.code(), &s3s::S3ErrorCode::NoSuchKey);
    assert_eq!(key_requests(&fx.fake).len(), 2, "{:?}", fx.fake.requests());
}

/// Wrong guess: a delta-eligible name stored passthrough (its delta was
/// poor). The opened `.delta` is absent, the HEAD finds the object, one
/// more GET reads it: the cost of the HEAD-first path, not more.
#[tokio::test]
async fn a_wrong_guess_costs_the_head_first_read() {
    let fx = fixture(Kind::Plain).await;
    let writer = engine_on(&fx.backend, None);
    let other = rnd(64 * 1024, 99);
    let stored = writer
        .store("b", "dir/other.zip", &other, None, Default::default())
        .await
        .unwrap();
    assert_eq!(stored.metadata.storage_info.label(), "passthrough");
    let svc = svc_on(engine_on(&fx.backend, None));
    fx.fake.clear();
    let (body, _) = run_get(&svc, get("dir/other.zip")).await;
    assert_eq!(body, other);
    let mut reqs = key_requests(&fx.fake);
    reqs.sort();
    assert_eq!(
        reqs,
        [
            "GET /b/dir/other.zip",
            "GET /b/dir/other.zip.delta",
            "HEAD /b/dir/other.zip"
        ]
    );
}

/// Both forms exist (a crash between a write and its `drop_other_form`):
/// the newest wins, whichever form was opened.
#[tokio::test]
async fn both_forms_newest_wins() {
    for newer_passthrough in [true, false] {
        let fx = fixture(Kind::Plain).await;
        let delta_meta = fx
            .backend
            .get_delta_metadata("b", "dir", "v1.zip")
            .await
            .unwrap();
        let leftover = b"a passthrough leftover".to_vec();
        let mut meta = crate::types::FileMetadata::new_passthrough(
            "v1.zip".into(),
            hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&leftover)),
            hex::encode(<md5::Md5 as md5::Digest>::digest(&leftover)),
            leftover.len() as u64,
            None,
        );
        let shift = chrono::Duration::seconds(60);
        meta.created_at = if newer_passthrough {
            delta_meta.created_at + shift
        } else {
            delta_meta.created_at - shift
        };
        fx.backend
            .put_passthrough("b", "dir", "v1.zip", &leftover, &meta)
            .await
            .unwrap();
        let svc = svc_on(engine_on(&fx.backend, None));
        let got = run_get(&svc, get("dir/v1.zip")).await;
        let want = if newer_passthrough {
            leftover.clone()
        } else {
            fx.versions[1].clone()
        };
        assert_eq!(got.0, want, "newer passthrough: {newer_passthrough}");
        assert_eq!(got, headed_answer(&fx.backend, "dir/v1.zip").await);
    }
}

/// The opened form's metadata names another layout: a `.delta` copied in
/// without its DG metadata (rclone between deployments) reads as a
/// passthrough stored under `name.delta`. The GET falls back to the
/// HEAD-first read and answers as it did.
#[tokio::test]
async fn a_layout_mismatch_falls_back_to_the_head_first_read() {
    let fx = fixture(Kind::Plain).await;
    let raw = rnd(5000, 5);
    reqwest::Client::new()
        .put(format!("{}/b/raw/x.zip.delta", fx.endpoint))
        .body(raw.clone())
        .send()
        .await
        .unwrap();
    let svc = svc_on(engine_on(&fx.backend, None));
    let got = run_get(&svc, get("raw/x.zip")).await;
    assert_eq!(got.0, raw);
    assert_eq!(got, headed_answer(&fx.backend, "raw/x.zip").await);
}

/// A conditional GET keeps HEAD-first: a 304 opens no body.
#[tokio::test]
async fn a_conditional_get_opens_no_body_for_a_304() {
    let fx = fixture(Kind::Plain).await;
    let svc = svc_on(engine_on(&fx.backend, None));
    let (_, etag) = run_get(&svc, get("pics/a.jpg")).await;
    fx.fake.clear();
    let mut g = get("pics/a.jpg");
    g.if_none_match = Some(s3s::dto::ETagCondition::ETag(s3s::dto::ETag::Strong(etag)));
    let err = get_object(&svc, request(g)).await.unwrap_err();
    assert_eq!(
        err.status_code(),
        Some(axum::http::StatusCode::NOT_MODIFIED)
    );
    assert_eq!(count(&key_requests(&fx.fake), "GET"), 0);
}

/// Large delta (above the spool threshold): the delta body comes from the
/// one read and is not read again; a second GET whose reconstruction is in
/// the range-spool cache reads only the delta's GET (its metadata).
#[tokio::test]
async fn a_large_delta_reads_its_delta_once() {
    for kind in KINDS {
        let fx = fixture(kind).await;
        let svc = svc_on(large_engine_on(&fx.backend));
        fx.fake.clear();
        let (body, _) = run_get(&svc, get("dir/v1.zip")).await;
        assert_eq!(body, fx.versions[1], "{kind:?}");
        let reqs = key_requests(&fx.fake);
        assert_eq!(
            reqs.iter()
                .filter(|r| *r == "GET /b/dir/v1.zip.delta")
                .count(),
            1,
            "{kind:?}: {reqs:?}"
        );
        fx.fake.clear();
        let (body, _) = run_get(&svc, get("dir/v1.zip")).await;
        assert_eq!(body, fx.versions[1], "{kind:?}");
        assert_eq!(
            key_requests(&fx.fake),
            ["GET /b/dir/v1.zip.delta"],
            "{kind:?}"
        );
    }
}

/// A client that drops a large-delta body half-way leaks nothing: with ONE
/// codec slot, the next decode still runs.
#[tokio::test]
async fn a_dropped_body_leaks_no_permit() {
    use futures::StreamExt;
    let fx = fixture(Kind::Plain).await;
    let mut cfg = crate::config::Config::default();
    cfg.tuning.spool_threshold_bytes = Some(1024);
    cfg.codec_concurrency = Some(1);
    cfg.range_spool_ttl_secs = 0;
    let svc = svc_on(DeltaGliderEngine::new_with_backend(
        fx.backend.clone(),
        &cfg,
        None,
    ));
    for _ in 0..3 {
        for key in ["dir/v0.zip", "dir/v1.zip", "pics/a.jpg"] {
            let resp = get_object(&svc, request(get(key))).await.unwrap();
            let mut body = resp.output.body.unwrap();
            body.next().await.unwrap().unwrap();
            drop(body);
        }
    }
    let done = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_get(&svc, get("dir/v1.zip")),
    )
    .await
    .expect("a codec slot or spool budget leaked");
    assert_eq!(done.0, fx.versions[1]);
}

/// Encrypting wrapper over the filesystem: the opened form decrypts, and
/// its metadata (the raw stored one) maps as on HEAD.
#[tokio::test]
async fn encrypted_filesystem_reads_in_one_open() {
    let dir = tempfile::tempdir().unwrap();
    let fs = crate::storage::FilesystemBackend::new(dir.path().to_path_buf())
        .await
        .unwrap();
    let cfg = Arc::new(arc_swap::ArcSwap::new(Arc::new(
        crate::storage::EncryptionConfig {
            key: Some(crate::storage::EncryptionKey::from_hex(&"cd".repeat(32)).unwrap()),
            key_id: Some("kid".into()),
            ..Default::default()
        },
    )));
    let backend: Backend = Arc::new(DynStorageBackend::new_box(
        crate::storage::EncryptingBackend::new(fs, cfg),
    ));
    let writer = engine_on(&backend, None);
    writer.create_bucket("b").await.unwrap();
    let photo = rnd(300 * 1024, 3);
    writer
        .store("b", "pics/a.jpg", &photo, None, Default::default())
        .await
        .unwrap();
    let base = rnd(64 * 1024, 11);
    let mut v1 = base.clone();
    v1[7] ^= 1;
    for (k, d) in [("dir/v0.zip", &base), ("dir/v1.zip", &v1)] {
        writer
            .store("b", k, d, None, Default::default())
            .await
            .unwrap();
    }
    for threshold in [None, Some(1024)] {
        let svc = svc_on(engine_on(&backend, threshold));
        for (key, want) in [
            ("pics/a.jpg", &photo),
            ("dir/v1.zip", &v1),
            ("dir/v0.zip", &base),
        ] {
            for _ in 0..2 {
                let got = run_get(&svc, get(key)).await;
                assert_eq!(&got.0, want, "{key} {threshold:?}");
                assert_eq!(got, headed_answer(&backend, key).await, "{key}");
            }
        }
    }
}

/// THE rule: a GET's full read goes through `retrieve_stream_fresh`
/// (the metadata and the bytes from one response). The GET verb does not
/// HEAD and then call `retrieve_stream`, which read the bytes in a second
/// round trip and could pair one version's ETag with another's body.
/// (CopyObject reads its source in `copy.rs`; the copy plan moves it.)
#[test]
fn the_get_verb_opens_the_object_with_its_metadata() {
    let text = std::fs::read_to_string("src/s3_adapter_s3s/object.rs").unwrap();
    let sites: Vec<usize> = crate::source_scan::prod_lines(&text)
        .into_iter()
        .filter(|(_, line)| line.contains(".retrieve_stream("))
        .map(|(n, _)| n)
        .collect();
    assert!(
        sites.is_empty(),
        "object.rs lines {sites:?}: use retrieve_stream_fresh"
    );
}
