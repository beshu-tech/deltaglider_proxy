// SPDX-License-Identifier: BUSL-1.1

//! PutObject through `api::body_ingest` on an engine over the filesystem
//! backend, with a 1 MiB spool threshold: the medium each body takes, the
//! RAM peak, and, for every way a body can fail, that nothing is stored and
//! the RAM and spool budgets are whole again.

use super::multipart::adapter_test_support::service;
use super::*;
use crate::api::checksum::{computed_digest, ChecksumAlgorithm, ChecksumPlan};
use crate::deltaglider::memory_budget::MemoryBudget;
use crate::deltaglider::{DeltaGliderEngine, DynEngine};
use crate::metrics::Metrics;
use crate::storage::{DynStorageBackend, FilesystemBackend};
use base64::Engine as _;
use bytes::Bytes;
use md5::Digest as _;
use std::sync::atomic::{AtomicBool, Ordering};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const THRESHOLD: usize = MIB;
const SPOOL_MIB: usize = 16;
const MAX_OBJECT: usize = 32 * MIB;

struct Rig {
    _dir: tempfile::TempDir,
    spool_dir: std::path::PathBuf,
    svc: DeltaGliderS3Service,
    metrics: Arc<Metrics>,
}

impl Rig {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(
            FilesystemBackend::new(dir.path().join("data"))
                .await
                .unwrap(),
        );
        let mut config = crate::config::Config {
            max_object_size: MAX_OBJECT as u64,
            ..Default::default()
        };
        config.tuning.spool_threshold_bytes = Some(THRESHOLD as u64);
        config.tuning.spool_acquire_timeout_secs = 1;
        config.buckets.insert(
            "frozen".into(),
            crate::bucket_policy::BucketPolicyConfig {
                quota_bytes: Some(0),
                ..Default::default()
            },
        );
        let metrics = Arc::new(Metrics::new());
        let spool_dir = dir.path().join("spool");
        let engine: DynEngine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &config, Some(metrics.clone()))
                .with_memory_budget(MemoryBudget::new(8 * MIB as u64))
                .with_spool_dir(
                    crate::deltaglider::spool::SpoolDir::new(
                        spool_dir.clone(),
                        (SPOOL_MIB * MIB) as u64,
                    )
                    .unwrap(),
                );
        engine.create_bucket("b").await.unwrap();
        engine.create_bucket("frozen").await.unwrap();
        Self {
            _dir: dir,
            spool_dir,
            svc: service(AppState::for_tests(engine)),
            metrics,
        }
    }

    fn engine(&self) -> Arc<DynEngine> {
        self.svc.state.engine.load_full()
    }

    fn bodies(&self, medium: &str, reason: &str) -> u64 {
        self.metrics
            .ingest_bodies_total
            .with_label_values(&[medium, reason])
            .get()
    }

    /// Nothing held: the RAM budget, the spool budget and the spool dir.
    fn assert_restored(&self, when: &str) {
        let engine = self.engine();
        assert_eq!(engine.memory_budget().held_bytes(), 0, "{when}: RAM held");
        assert_eq!(
            engine.spool_dir().free_mib(),
            SPOOL_MIB,
            "{when}: spool budget held"
        );
        let files = std::fs::read_dir(&self.spool_dir).unwrap().count();
        assert_eq!(files, 0, "{when}: a spool file was left behind");
    }

    async fn put(&self, req: s3s::S3Request<s3s::dto::PutObjectInput>) -> s3s::S3Result<String> {
        let resp = object::put_object(&self.svc, req).await?;
        Ok(resp.output.e_tag.unwrap().value().to_string())
    }

    async fn stored(&self, key: &str) -> Option<Vec<u8>> {
        self.engine().retrieve("b", key).await.ok().map(|(d, _)| d)
    }
}

fn body(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 % 251) as u8).collect()
}

type Item = Result<Bytes, std::io::Error>;

/// `data` in 64 KiB chunks, the way a client body arrives.
fn chunks(data: &[u8]) -> Vec<Item> {
    data.chunks(64 * KIB)
        .map(|c| Ok(Bytes::copy_from_slice(c)))
        .collect()
}

/// An aws-chunked framing of `data` with `trailer` lines after the last chunk.
fn framed(data: &[u8], trailer: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for piece in data.chunks(64 * KIB) {
        out.extend_from_slice(format!("{:x}\r\n", piece.len()).as_bytes());
        out.extend_from_slice(piece);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("0\r\n{trailer}\r\n").as_bytes());
    out
}

fn crc32_b64(data: &[u8]) -> String {
    let mut h = ChecksumPlan::only(ChecksumAlgorithm::Crc32).hasher();
    h.update(data);
    let digest = computed_digest(ChecksumAlgorithm::Crc32, &h.finalize()).unwrap();
    base64::engine::general_purpose::STANDARD.encode(digest)
}

fn md5_b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(md5::Md5::digest(data))
}

/// A PutObject of `key` with `items` as its body (unknown transport
/// length, as a streamed body has) and `headers`.
fn put_req(
    key: &str,
    items: Vec<Item>,
    headers: &[(&'static str, String)],
    content_md5: Option<String>,
) -> s3s::S3Request<s3s::dto::PutObjectInput> {
    let input = s3s::dto::PutObjectInput::builder()
        .bucket("b".into())
        .key(key.into())
        .content_md5(content_md5)
        .body(Some(s3s::dto::StreamingBlob::wrap(futures::stream::iter(
            items,
        ))))
        .build()
        .unwrap();
    let mut req = super::multipart::adapter_test_support::request(input);
    for (name, value) in headers {
        req.headers.append(*name, value.parse().unwrap());
    }
    req
}

fn len_header(n: usize) -> Vec<(&'static str, String)> {
    vec![("content-length", n.to_string())]
}

/// Headers of an unsigned aws-chunked body of `decoded` bytes.
fn chunked_headers(
    decoded: usize,
    framed_len: usize,
    announce: bool,
) -> Vec<(&'static str, String)> {
    let mut h = vec![
        (
            "x-amz-content-sha256",
            "STREAMING-UNSIGNED-PAYLOAD-TRAILER".to_string(),
        ),
        ("x-amz-decoded-content-length", decoded.to_string()),
        ("content-length", framed_len.to_string()),
    ];
    if announce {
        h.push(("x-amz-trailer", "x-amz-checksum-crc32".to_string()));
    }
    h
}

/// A large body goes to a spool file and is stored from it: no RAM held
/// for it, the counter says spool, and every budget is whole afterwards.
/// A small body is read into memory, within the threshold.
#[tokio::test]
async fn a_put_three_times_the_threshold_is_spooled_and_stored() {
    let rig = Rig::new().await;
    let data = body(3 * THRESHOLD);
    let etag = rig
        .put(put_req(
            "a/photo.jpg",
            chunks(&data),
            &len_header(data.len()),
            Some(md5_b64(&data)),
        ))
        .await
        .unwrap();
    assert_eq!(etag, hex::encode(md5::Md5::digest(&data)));
    assert_eq!(rig.bodies("spool", "over_threshold"), 1);
    assert_eq!(rig.engine().memory_budget().peak_bytes(), 0);
    rig.assert_restored("after a spooled passthrough");
    // (A GET may keep a cached spool file of its own.)
    assert_eq!(rig.stored("a/photo.jpg").await.unwrap(), data);

    // A delta-eligible key takes the same spooled store (its body becomes
    // the folder's cached reference file, so the spool is not checked).
    rig.put(put_req(
        "a/app.zip",
        chunks(&data),
        &len_header(data.len()),
        None,
    ))
    .await
    .unwrap();
    assert_eq!(rig.stored("a/app.zip").await.unwrap(), data);
    assert_eq!(rig.bodies("spool", "over_threshold"), 2);

    let small = body(200 * KIB);
    rig.put(put_req(
        "a/small.jpg",
        chunks(&small),
        &len_header(small.len()),
        None,
    ))
    .await
    .unwrap();
    assert_eq!(rig.stored("a/small.jpg").await.unwrap(), small);
    assert_eq!(rig.bodies("memory", "under_threshold"), 1);
    let peak = rig.engine().memory_budget().peak_bytes();
    assert!(peak > 0 && peak <= THRESHOLD as u64, "RAM peak {peak}");
    assert_eq!(rig.engine().memory_budget().held_bytes(), 0);
}

/// A trailer checksum is checked as the framed body streams: announced
/// (computed on the way) or not (computed again over the stored body).
#[tokio::test]
async fn a_framed_body_with_a_right_trailer_checksum_is_stored() {
    let rig = Rig::new().await;
    let data = body(3 * THRESHOLD);
    let trailer = format!("x-amz-checksum-crc32:{}\r\n", crc32_b64(&data));
    for (key, announce) in [("t/announced.jpg", true), ("t/late.jpg", false)] {
        let wire = framed(&data, &trailer);
        rig.put(put_req(
            key,
            chunks(&wire),
            &chunked_headers(data.len(), wire.len(), announce),
            None,
        ))
        .await
        .unwrap_or_else(|e| panic!("{key}: {e:?}"));
        rig.assert_restored(key);
        assert_eq!(rig.stored(key).await.unwrap(), data, "{key}");
    }
}

/// Every way a body can fail, for a body in memory and a spooled one:
/// the right S3 code, nothing stored, every budget whole again.
#[tokio::test]
async fn every_put_failure_stores_nothing_and_restores_the_budgets() {
    let rig = Rig::new().await;
    for size in [200 * KIB, 3 * THRESHOLD] {
        let data = body(size);
        let other = body(size + 1)[1..].to_vec();
        let wrong_trailer = format!("x-amz-checksum-crc32:{}\r\n", crc32_b64(&other));
        let mut last_chunk_fails = chunks(&data);
        *last_chunk_fails.last_mut().unwrap() =
            Err(std::io::Error::other("UploadStreamError: Sha256Mismatch"));
        let mut mid_stream_error = chunks(&data);
        mid_stream_error.truncate(2);
        mid_stream_error.push(Err(std::io::Error::other("connection reset")));
        let wire = framed(&data, &wrong_trailer);
        let mut truncated = framed(&data, "");
        truncated.truncate(truncated.len() - 5);
        let cases: Vec<(&str, s3s::S3Request<_>, &str)> = vec![
            (
                "payload hash mismatch on the last chunk",
                put_req("f/k", last_chunk_fails, &len_header(size), None),
                "BadDigest",
            ),
            (
                "Content-MD5 mismatch",
                put_req(
                    "f/k",
                    chunks(&data),
                    &len_header(size),
                    Some(md5_b64(&other)),
                ),
                "BadDigest",
            ),
            (
                "checksum header mismatch",
                put_req(
                    "f/k",
                    chunks(&data),
                    &[
                        ("content-length", size.to_string()),
                        ("x-amz-checksum-crc32", crc32_b64(&other)),
                    ],
                    None,
                ),
                "BadDigest",
            ),
            (
                "wrong announced trailer",
                put_req(
                    "f/k",
                    chunks(&wire),
                    &chunked_headers(size, wire.len(), true),
                    None,
                ),
                "BadDigest",
            ),
            (
                "wrong trailer nobody announced (late re-hash)",
                put_req(
                    "f/k",
                    chunks(&wire),
                    &chunked_headers(size, wire.len(), false),
                    None,
                ),
                "BadDigest",
            ),
            (
                "announced trailer missing",
                put_req(
                    "f/k",
                    chunks(&framed(&data, "")),
                    &chunked_headers(size, 0, true),
                    None,
                ),
                "InvalidRequest",
            ),
            (
                "truncated framing",
                put_req(
                    "f/k",
                    chunks(&truncated),
                    &chunked_headers(size, truncated.len(), false),
                    None,
                ),
                "InvalidArgument",
            ),
            (
                "stream error mid-way",
                put_req("f/k", mid_stream_error, &len_header(size), None),
                "InternalError",
            ),
            (
                "body longer than declared",
                put_req("f/k", chunks(&data), &len_header(size - 1), None),
                "InvalidArgument",
            ),
            (
                "body shorter than declared",
                put_req("f/k", chunks(&data), &len_header(size + 1), None),
                "IncompleteBody",
            ),
        ];
        for (what, req, code) in cases {
            let when = format!("{what} ({size} bytes)");
            let err = rig.put(req).await.unwrap_err();
            assert_eq!(err.code().as_str(), code, "{when}: {err:?}");
            assert!(rig.stored("f/k").await.is_none(), "{when}: stored");
            rig.assert_restored(&when);
        }
    }
}

/// A body that must be refused for what its headers say is refused before
/// a byte of it is read.
#[tokio::test]
async fn the_headers_are_judged_before_a_byte_is_read() {
    let rig = Rig::new().await;
    /// (what, bucket, headers, Content-MD5, code)
    type Case<'a> = (
        &'a str,
        &'a str,
        Vec<(&'static str, String)>,
        Option<String>,
        &'a str,
    );
    let cases: Vec<Case> = vec![
        (
            "above max_object_size",
            "b",
            len_header(MAX_OBJECT + 1),
            None,
            "EntityTooLarge",
        ),
        (
            "above the spool budget",
            "b",
            len_header(SPOOL_MIB * MIB + MIB),
            None,
            "EntityTooLarge",
        ),
        ("no length", "b", vec![], None, "MissingContentLength"),
        (
            "malformed Content-MD5",
            "b",
            len_header(10),
            Some("not base64!".into()),
            "InvalidDigest",
        ),
        (
            "malformed checksum header",
            "b",
            vec![
                ("content-length", "10".into()),
                ("x-amz-checksum-crc32", "dGVzdA==AA".into()),
            ],
            None,
            "InvalidRequest",
        ),
        (
            "bucket over its quota",
            "frozen",
            len_header(10),
            None,
            "AccessDenied",
        ),
    ];
    for (what, bucket, headers, content_md5, code) in cases {
        let polled = Arc::new(AtomicBool::new(false));
        let flag = polled.clone();
        let stream = futures::stream::poll_fn(move |_| {
            flag.store(true, Ordering::SeqCst);
            std::task::Poll::Ready(None::<Item>)
        });
        let input = s3s::dto::PutObjectInput::builder()
            .bucket(bucket.into())
            .key("h/k".into())
            .content_md5(content_md5)
            .body(Some(s3s::dto::StreamingBlob::wrap(stream)))
            .build()
            .unwrap();
        let mut req = super::multipart::adapter_test_support::request(input);
        for (name, value) in &headers {
            req.headers.append(*name, value.parse().unwrap());
        }
        let err = rig.put(req).await.unwrap_err();
        assert_eq!(err.code().as_str(), code, "{what}: {err:?}");
        if what == "above the spool budget" {
            assert!(
                err.message().unwrap_or("").contains("DGP_SPOOL_MAX_BYTES"),
                "{what}: {err:?}"
            );
        }
        assert!(!polled.load(Ordering::SeqCst), "{what}: the body was read");
        rig.assert_restored(what);
    }
}
