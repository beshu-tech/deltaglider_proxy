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
        .put_reference("b", "dir", bytes::Bytes::from(bytes.clone()), &meta, proof)
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
            .put_passthrough(
                "b",
                "dir",
                "v1.zip",
                bytes::Bytes::from(leftover.clone()),
                &meta,
            )
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

/// Large delta (above the spool threshold): the one read gives the
/// metadata and its body is dropped unread (bounded memory: no delta body
/// in RAM before the spool admission); the decode reads the delta after its
/// spool reservation. A second GET whose reconstruction is in the
/// range-spool cache reads only the delta's GET (its metadata).
#[tokio::test]
async fn a_large_delta_reads_its_delta_after_admission() {
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
            2,
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
            let size = want.len() as u64;
            for (first, last) in [(1000, Some(1999)), (size - 10, None)] {
                let (body, _, range) = run_range(&svc, ranged(key, first, last)).await.unwrap();
                let end = last.unwrap_or(size - 1);
                assert_eq!(body, want[first as usize..=end as usize], "{key} range");
                assert_eq!(range, format!("bytes {first}-{end}/{size}"), "{key}");
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

/// THE rule of the engine's reads: the bytes of an object are read with
/// the metadata of the same response (`open_object`,
/// `open_passthrough_range`), and that metadata is what the GET serves.
/// The metadata-less reads (`get_passthrough_stream`,
/// `get_passthrough_stream_range`) paired an earlier read's ETag with the
/// bytes of a version that a write put in between.
#[test]
fn the_engine_reads_bytes_with_their_metadata() {
    let text = std::fs::read_to_string("src/deltaglider/engine/retrieve.rs").unwrap();
    let prod = crate::source_scan::prod_text(&text);
    let squashed: String = prod.split_whitespace().collect();
    for bare in [".get_passthrough_stream(", ".get_passthrough_stream_range("] {
        assert!(
            !squashed.contains(bare),
            "retrieve.rs calls {bare}: use open_object / open_passthrough_range"
        );
    }
}

/// Routing: the two parallel reads of a cold GET share one bucket
/// resolution (one HeadBucket), as do concurrent GETs.
#[tokio::test]
async fn a_cold_routed_get_resolves_its_bucket_once() {
    let fx = fixture(Kind::Routed).await;
    let fresh = backend_on(&fx.endpoint, Kind::Routed);
    let svc = svc_on(engine_on(&fresh, None));
    fx.fake.set_delay_ms("HEAD_BUCKET", 50);
    fx.fake.clear();
    let gets = (0..4).map(|_| run_get(&svc, get("pics/a.jpg")));
    for (body, _) in futures::future::join_all(gets).await {
        assert_eq!(body, fx.photo);
    }
    let heads = fx
        .fake
        .requests()
        .iter()
        .filter(|r| *r == "HEAD /b" || *r == "HEAD /b/")
        .count();
    assert_eq!(heads, 1, "{:?}", fx.fake.requests());
}

/// P3.4: a large delta GET downloads its reference to a spool file with
/// ONE GET: the spool is reserved at the size from the GET response
/// (its Content-Length; the decrypted length behind the encrypting
/// wrapper). A HEAD for the size went first. Both the shared download
/// (range-spool cache on) and the GET's own copy (cache off).
#[tokio::test]
async fn a_large_delta_downloads_its_reference_with_one_get() {
    for kind in KINDS {
        for shared in [true, false] {
            let fx = fixture(kind).await;
            let mut cfg = crate::config::Config::default();
            cfg.tuning.spool_threshold_bytes = Some(1024);
            if !shared {
                cfg.range_spool_ttl_secs = 0;
            }
            let engine = DeltaGliderEngine::new_with_backend(fx.backend.clone(), &cfg, None);
            engine.head("b", "dir/v1.zip").await.unwrap();
            fx.fake.clear();
            let (data, _) = engine.retrieve("b", "dir/v1.zip").await.unwrap();
            assert_eq!(data, fx.versions[1], "{kind:?} shared={shared}");
            let reqs = object_requests(&fx.fake);
            let reference: Vec<&String> = reqs
                .iter()
                .filter(|r| r.ends_with("/dir/reference.bin"))
                .collect();
            assert_eq!(
                reference,
                ["GET /b/dir/reference.bin"],
                "{kind:?} shared={shared}: {reqs:?}"
            );
        }
    }
}

/// The filesystem keeps its hardlink: the reference source is the file
/// itself, linked (not copied) into the spool.
#[tokio::test]
async fn the_filesystem_reference_source_hardlinks() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let fs = crate::storage::FilesystemBackend::new(dir.path().join("data"))
        .await
        .unwrap();
    let backend: Backend = Arc::new(DynStorageBackend::new_box(fs));
    let engine = engine_on(&backend, None);
    engine.create_bucket("b").await.unwrap();
    let base = rnd(64 * 1024, 21);
    engine
        .store("b", "dir/v0.zip", &base, None, Default::default())
        .await
        .unwrap();
    let source = backend.open_reference_source("b", "dir").await.unwrap();
    assert_eq!(source.size, 64 * 1024);
    let dest = dir.path().join("spooled-reference");
    assert_eq!(source.write_to(&dest).await.unwrap(), 64 * 1024);
    let stored = std::fs::read_dir(dir.path().join("data")).unwrap().count();
    assert!(stored > 0);
    let linked = std::fs::metadata(&dest).unwrap();
    assert_eq!(linked.nlink(), 2, "the reference is hardlinked, not copied");
}

/// The filesystem reference source is the file opened by the read: a
/// write that replaced `reference.bin` after the open (a new baseline)
/// does not change what it writes. Before, it hardlinked the path, so the
/// spool got the new file under the old metadata and size, and its
/// returned length came from the new file.
#[tokio::test]
async fn the_filesystem_reference_source_writes_the_file_it_opened() {
    let dir = tempfile::tempdir().unwrap();
    let fs = crate::storage::FilesystemBackend::new(dir.path().join("data"))
        .await
        .unwrap();
    let backend: Backend = Arc::new(DynStorageBackend::new_box(fs));
    let engine = engine_on(&backend, None);
    engine.create_bucket("b").await.unwrap();
    let base = rnd(64 * 1024, 21);
    engine
        .store("b", "dir/v0.zip", &base, None, Default::default())
        .await
        .unwrap();
    let opened = backend.get_reference("b", "dir").await.unwrap();
    for read_all in [false, true] {
        let source = backend.open_reference_source("b", "dir").await.unwrap();
        let meta = backend.get_reference_metadata("b", "dir").await.unwrap();
        let newer = rnd(80 * 1024, 22 + u64::from(read_all));
        backend
            .put_reference(
                "b",
                "dir",
                bytes::Bytes::from(newer),
                &meta,
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .unwrap();
        let got = if read_all {
            source.read_all().await.unwrap()
        } else {
            let dest = dir.path().join("spooled-reference");
            let written = source.write_to(&dest).await.unwrap();
            let got = std::fs::read(&dest).unwrap();
            assert_eq!(written, got.len() as u64);
            got
        };
        assert_eq!(got.len(), opened.len(), "read_all {read_all}");
        assert!(
            got == opened,
            "read_all {read_all}: the bytes of the opened file"
        );
        // Back to the first version for the next round.
        backend
            .put_reference(
                "b",
                "dir",
                bytes::Bytes::from(opened.clone()),
                &meta,
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .unwrap();
    }
}

/// A body longer than its declared size fails before it overruns the
/// spool reservation.
#[tokio::test]
async fn a_reference_body_longer_than_its_size_fails() {
    let dir = tempfile::tempdir().unwrap();
    let stream: crate::storage::ByteStream = Box::pin(futures::stream::iter(vec![
        Ok(bytes::Bytes::from_static(b"12345")),
        Ok(bytes::Bytes::from_static(b"678")),
    ]));
    let meta = crate::types::FileMetadata::new_passthrough(
        "reference.bin".into(),
        String::new(),
        String::new(),
        6,
        None,
    );
    let source = crate::storage::ReferenceSource::stream(stream, 6, meta);
    assert!(source.write_to(&dir.path().join("r")).await.is_err());
}

fn ranged(key: &str, first: u64, last: Option<u64>) -> s3s::dto::GetObjectInput {
    let mut g = get(key);
    g.range = Some(s3s::dto::Range::Int { first, last });
    g
}

/// A GET's body, ETag and Content-Range; the error code on failure.
async fn run_range(
    svc: &super::DeltaGliderS3Service,
    input: s3s::dto::GetObjectInput,
) -> Result<(Vec<u8>, String, String), String> {
    use futures::StreamExt;
    let resp = get_object(svc, request(input))
        .await
        .map_err(|e| e.code().as_str().to_string())?;
    let etag = resp
        .output
        .e_tag
        .as_ref()
        .map(|e| e.value().to_string())
        .unwrap_or_default();
    let range = resp.output.content_range.clone().unwrap_or_default();
    let mut out = Vec::new();
    if let Some(mut body) = resp.output.body {
        while let Some(c) = body.next().await {
            out.extend_from_slice(&c.unwrap());
        }
    }
    Ok((out, etag, range))
}

/// P3.7: a ranged GET of a passthrough object (no If-Range, no
/// conditionals) sends the ranged GET and the `.delta` HEAD in parallel:
/// the metadata comes from the ranged response. Was HEAD x2 + ranged GET.
/// Behind a wrapper with a key the photo is v1 (single-shot) encrypted:
/// the wrapper's first ranged GET gives the metadata and the start of the
/// body, with no HEAD (P3.5), and one more GET reads the rest of the body
/// unless the first one held it all: 4 -> 3 requests, or 2.
/// (`encrypted_ranges_send_no_metadata_head` covers the chunked format.)
#[tokio::test]
async fn a_passthrough_range_is_one_round_trip() {
    for kind in KINDS {
        let fx = fixture(kind).await;
        let size = fx.photo.len() as u64;
        let etag = fx
            .backend
            .get_passthrough_metadata("b", "pics", "a.jpg")
            .await
            .unwrap()
            .etag()
            .trim_matches('"')
            .to_string();
        for (first, last) in [
            (0, Some(99)),
            (140_000, Some(140_099)),
            (199_000, None),
            (5, Some(size + 50)),
        ] {
            let svc = svc_on(engine_on(&fx.backend, None));
            fx.fake.clear();
            let (body, got_etag, range) = run_range(&svc, ranged("pics/a.jpg", first, last))
                .await
                .unwrap();
            let end = last.unwrap_or(u64::MAX).min(size - 1);
            assert_eq!(
                body,
                fx.photo[first as usize..=end as usize],
                "{kind:?} {first}"
            );
            assert_eq!(range, format!("bytes {first}-{end}/{size}"), "{kind:?}");
            assert_eq!(got_etag, etag, "{kind:?}");
            let mut reqs = key_requests(&fx.fake);
            reqs.sort();
            if kind == Kind::Encrypted {
                // The guessed read; the rest of the body unless the guess,
                // which runs through the frame of the range's end, held it.
                let gets = if last.is_some_and(|l| l >= size) {
                    1
                } else {
                    2
                };
                let mut want = vec!["GET /b/pics/a.jpg"; gets];
                want.push("HEAD /b/pics/a.jpg.delta");
                assert_eq!(reqs, want, "{kind:?} {first}");
            } else {
                assert_eq!(
                    reqs,
                    ["GET /b/pics/a.jpg", "HEAD /b/pics/a.jpg.delta"],
                    "{kind:?}"
                );
            }
        }
    }
}

/// A range that starts past the end answers InvalidRange (416) from the
/// backend's own answer, with no HEAD first.
#[tokio::test]
async fn a_passthrough_range_past_the_end_is_invalid_range() {
    for kind in KINDS {
        let fx = fixture(kind).await;
        let svc = svc_on(engine_on(&fx.backend, None));
        let size = fx.photo.len() as u64;
        let err = run_range(&svc, ranged("pics/a.jpg", size, None))
            .await
            .unwrap_err();
        assert_eq!(err, "InvalidRange", "{kind:?}");
    }
}

/// The `.delta` form exists and is newer (a crash leftover): the ranged
/// passthrough body is discarded and the delta path answers. An older
/// `.delta` loses to the passthrough. A delta-only key reads the delta.
#[tokio::test]
async fn a_newer_delta_wins_over_the_ranged_passthrough() {
    for newer_passthrough in [false, true] {
        let fx = fixture(Kind::Plain).await;
        let delta_meta = fx
            .backend
            .get_delta_metadata("b", "dir", "v1.zip")
            .await
            .unwrap();
        let leftover = rnd(4096, 77);
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
            .put_passthrough(
                "b",
                "dir",
                "v1.zip",
                bytes::Bytes::from(leftover.clone()),
                &meta,
            )
            .await
            .unwrap();
        let svc = svc_on(engine_on(&fx.backend, None));
        let (body, _, range) = run_range(&svc, ranged("dir/v1.zip", 10, Some(109)))
            .await
            .unwrap();
        let (want, size) = if newer_passthrough {
            (&leftover, leftover.len())
        } else {
            (&fx.versions[1], fx.versions[1].len())
        };
        assert_eq!(
            body,
            want[10..=109],
            "newer passthrough: {newer_passthrough}"
        );
        assert_eq!(range, format!("bytes 10-109/{size}"));
    }
    let fx = fixture(Kind::Plain).await;
    let svc = svc_on(engine_on(&fx.backend, None));
    let (body, _, _) = run_range(&svc, ranged("dir/v0.zip", 0, Some(9)))
        .await
        .unwrap();
    assert_eq!(body, fx.versions[0][..10]);
}

/// Shapes that keep HEAD-first: a suffix range (it needs the size) and a
/// range with a conditional header.
#[tokio::test]
async fn suffix_and_conditional_ranges_keep_head_first() {
    let fx = fixture(Kind::Plain).await;
    let svc = svc_on(engine_on(&fx.backend, None));
    let size = fx.photo.len();
    let mut g = get("pics/a.jpg");
    g.range = Some(s3s::dto::Range::Suffix { length: 100 });
    let (body, _, _) = run_range(&svc, g).await.unwrap();
    assert_eq!(body, fx.photo[size - 100..]);
    let mut g = ranged("pics/a.jpg", 0, Some(9));
    g.if_match = Some(s3s::dto::ETagCondition::Any);
    fx.fake.clear();
    let (body, _, _) = run_range(&svc, g).await.unwrap();
    assert_eq!(body, fx.photo[..10]);
    assert_eq!(count(&key_requests(&fx.fake), "HEAD"), 2);
}

/// A store that answers GET with a chunked body (no Content-Length) gets
/// the same ranged answers as one that sends it: the length of a ranged
/// body comes from its Content-Range. Before, a missing Content-Length
/// read as 0: a wrong Content-Range (`bytes 0-0/...`), an InternalError
/// behind the encrypting wrapper, and a suffix range that loaded the
/// whole object.
#[tokio::test]
async fn ranges_without_content_length_answer_as_with_it() {
    for kind in KINDS {
        let fx = fixture(kind).await;
        let size = fx.photo.len() as u64;
        fx.fake.set_chunked_gets(true);
        for (first, last) in [(0, Some(99)), (140_000, Some(140_099)), (199_000, None)] {
            let svc = svc_on(engine_on(&fx.backend, None));
            let end = last.unwrap_or(u64::MAX).min(size - 1);
            let (body, _, range) = run_range(&svc, ranged("pics/a.jpg", first, last))
                .await
                .unwrap_or_else(|e| panic!("{kind:?} {first}: {e}"));
            assert_eq!(
                body,
                fx.photo[first as usize..=end as usize],
                "{kind:?} {first}"
            );
            assert_eq!(range, format!("bytes {first}-{end}/{size}"), "{kind:?}");
        }
        let svc = svc_on(engine_on(&fx.backend, None));
        let mut g = get("pics/a.jpg");
        g.range = Some(s3s::dto::Range::Suffix { length: 100 });
        let (body, _, range) = run_range(&svc, g)
            .await
            .unwrap_or_else(|e| panic!("{kind:?} suffix: {e}"));
        assert_eq!(body, fx.photo[(size - 100) as usize..], "{kind:?} suffix");
        assert_eq!(range, format!("bytes {}-{}/{size}", size - 100, size - 1));
    }
}

/// A foreign object (no DG metadata) read from a store that sends no
/// Content-Length: the GET answers its real size, and the metadata cache
/// does not keep a size of 0.
#[tokio::test]
async fn a_foreign_object_without_content_length_keeps_its_size() {
    let fx = fixture(Kind::Plain).await;
    let body = rnd(5000, 3);
    fx.fake.insert_object("b/pics/foreign.jpg", &body, &[]);
    fx.fake.set_chunked_gets(true);
    let svc = svc_on(engine_on(&fx.backend, None));
    let resp = get_object(&svc, request(get("pics/foreign.jpg")))
        .await
        .unwrap();
    assert_eq!(resp.output.content_length, Some(5000));
    let engine = svc.state.engine.load();
    let (data, meta) = engine.retrieve("b", "pics/foreign.jpg").await.unwrap();
    assert_eq!(data, body);
    assert_eq!(meta.file_size, 5000, "the cached size");
}

fn md5_hex(data: &[u8]) -> String {
    hex::encode(<md5::Md5 as md5::Digest>::digest(data))
}

/// A wrong guess of a plain GET reads the other form after its HEAD.
/// When a write replaces the object between the HEAD and that read, the
/// response pairs the ETag and the body of ONE version. Before, the ETag
/// came from the HEAD (version A) and the body from the GET (version B).
#[tokio::test]
async fn a_wrong_guess_serves_the_etag_of_its_bytes() {
    let fx = fixture(Kind::Plain).await;
    let writer = engine_on(&fx.backend, None);
    let a = rnd(64 * 1024, 99);
    let b = rnd(64 * 1024, 1234);
    writer
        .store("b", "dir/other.zip", &a, None, Default::default())
        .await
        .unwrap();
    let stored = writer
        .store("b", "dir/bsrc.zip", &b, None, Default::default())
        .await
        .unwrap();
    assert_eq!(stored.metadata.storage_info.label(), "passthrough");
    let svc = svc_on(engine_on(&fx.backend, None));
    // The HEAD of the passthrough form reads A; the GET after it reads B.
    fx.fake
        .overwrite_after_reads("b/dir/other.zip", "b/dir/bsrc.zip", 1);
    let (body, etag) = run_get(&svc, get("dir/other.zip")).await;
    assert!(body == a || body == b, "a body of one version");
    assert_eq!(etag, md5_hex(&body), "the ETag of the body served");
}

/// The overwrite of `pics/a.jpg` by another photo that the HEAD-first
/// GET paths race: the HEAD (the first read of the key) sees version A,
/// every later read sees B.
async fn photo_overwritten_after_its_head(fx: &Fixture) -> Vec<u8> {
    let writer = engine_on(&fx.backend, None);
    let b = rnd(fx.photo.len(), 4321);
    writer
        .store("b", "pics/b.jpg", &b, None, Default::default())
        .await
        .unwrap();
    fx.fake
        .overwrite_after_reads("b/pics/a.jpg", "b/pics/b.jpg", 1);
    b
}

/// A conditional GET judges If-Match on its HEAD, then reads the bytes.
/// When a write replaces the object between the two, the judgement must
/// hold for the bytes served: a 412 (the served version does not match),
/// never a 200 of version B for an If-Match on version A.
#[tokio::test]
async fn a_conditional_get_judges_the_version_it_serves() {
    for kind in KINDS {
        let fx = fixture(kind).await;
        let svc = svc_on(engine_on(&fx.backend, None));
        let etag_a = md5_hex(&fx.photo);
        let b = photo_overwritten_after_its_head(&fx).await;
        let mut g = get("pics/a.jpg");
        g.if_match = Some(s3s::dto::ETagCondition::ETag(s3s::dto::ETag::Strong(
            etag_a.clone(),
        )));
        match run_range(&svc, g).await {
            Err(code) => assert!(
                code == "PreconditionFailed" || code == "SlowDown",
                "{kind:?}: {code}"
            ),
            Ok((body, etag, _)) => panic!(
                "{kind:?}: 200 with ETag {etag} (A: {}, B: {}) for If-Match {etag_a}",
                body == fx.photo,
                body == b
            ),
        }
    }
}

/// A ranged GET with a conditional reads the HEAD first, then the range.
/// When a write replaces the object between the two, the range must not
/// join version B's bytes to version A's ETag and size (a resumed
/// download would join two versions). An If-Match on A answers 412 from
/// B; an If-Match that B passes too (`*`) answers 503 SlowDown, which
/// clients retry: the range was resolved against A.
#[tokio::test]
async fn a_conditional_range_serves_the_version_it_judged() {
    for kind in KINDS {
        let etag_a = md5_hex(&fixture(kind).await.photo);
        for (condition, want) in [
            (
                s3s::dto::ETagCondition::ETag(s3s::dto::ETag::Strong(etag_a.clone())),
                "PreconditionFailed",
            ),
            (s3s::dto::ETagCondition::Any, "SlowDown"),
        ] {
            let fx = fixture(kind).await;
            let svc = svc_on(engine_on(&fx.backend, None));
            let b = photo_overwritten_after_its_head(&fx).await;
            let mut g = ranged("pics/a.jpg", 100, Some(199));
            g.if_match = Some(condition);
            match run_range(&svc, g).await {
                Err(code) => assert_eq!(code, want, "{kind:?}"),
                Ok((body, etag, _)) => panic!(
                    "{kind:?}: 206 with ETag {etag} (A: {etag_a}), A's bytes: {}, B's: {}",
                    body == fx.photo[100..200],
                    body == b[100..200]
                ),
            }
        }
    }
}

/// The filesystem's ranged read takes the metadata and the bytes from one
/// open file. Before, it read the xattr by path and then opened the path:
/// a write that renamed a new file over it in between paired one
/// version's metadata (its ETag) with another version's bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_filesystem_range_reads_one_version() {
    let dir = tempfile::tempdir().unwrap();
    let fs = crate::storage::FilesystemBackend::new(dir.path().to_path_buf())
        .await
        .unwrap();
    let backend: Backend = Arc::new(DynStorageBackend::new_box(fs));
    backend.create_bucket("b").await.unwrap();
    let version = |i: u64| {
        let body = rnd(1024 + (i as usize % 7) * 512, i + 1);
        let meta = crate::types::FileMetadata::new_passthrough(
            "x.bin".into(),
            hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&body)),
            md5_hex(&body),
            body.len() as u64,
            None,
        );
        (bytes::Bytes::from(body), meta)
    };
    let (body, meta) = version(0);
    backend
        .put_passthrough("b", "p", "x.bin", body, &meta)
        .await
        .unwrap();
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let (backend, done) = (backend.clone(), done.clone());
        tokio::spawn(async move {
            for i in 1..400 {
                let (body, meta) = version(i);
                backend
                    .put_passthrough("b", "p", "x.bin", body, &meta)
                    .await
                    .unwrap();
            }
            done.store(true, std::sync::atomic::Ordering::SeqCst);
        })
    };
    let mut reads = 0;
    let mut mixed = 0;
    while !done.load(std::sync::atomic::Ordering::SeqCst) {
        use futures::TryStreamExt;
        let (stream, len, meta) = backend
            .open_passthrough_range("b", "p", "x.bin", 0, u64::MAX - 1)
            .await
            .unwrap();
        let body: Vec<bytes::Bytes> = stream.try_collect().await.unwrap();
        let body = body.concat();
        reads += 1;
        if md5_hex(&body) != meta.md5 || len != meta.file_size {
            mixed += 1;
        }
    }
    writer.await.unwrap();
    assert!(reads > 0);
    assert_eq!(mixed, 0, "{mixed} of {reads} reads mixed two versions");
}
