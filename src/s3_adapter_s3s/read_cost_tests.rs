// SPDX-License-Identifier: BUSL-1.1

//! What the read verbs send upstream, pinned per backend shape (plain S3,
//! S3 behind the encrypting wrapper, S3 behind the routing backend) and
//! per cache state (a cold engine, as after a restart or on another node;
//! a warm one). Each count is exact: a test that allows "at most" hides a
//! regression of one request.

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
    backend: Backend,
    fake: Arc<FakeS3>,
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
        backend,
        fake,
        versions,
    }
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
