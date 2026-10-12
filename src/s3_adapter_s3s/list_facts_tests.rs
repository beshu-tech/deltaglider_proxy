// SPDX-License-Identifier: BUSL-1.1

//! `metadata=true` (D1): each listed object carries its storage facts,
//! `x-amz-meta-dg-note` and (for a delta) `x-amz-meta-dg-delta-size`,
//! from the listing alone, with exactly the values HeadObject shows.

use super::list::{client_list_page, list_metadata_map};
use super::*;
use crate::deltaglider::{DeltaGliderEngine, DynEngine};
use crate::storage::{DynStorageBackend, EncryptingBackend, EncryptionConfig, EncryptionKey};
use std::sync::Arc;

fn encrypted(inner: impl crate::storage::StorageBackend + 'static) -> Arc<DynEngine> {
    let cfg = Arc::new(arc_swap::ArcSwap::new(Arc::new(EncryptionConfig {
        key: Some(EncryptionKey::from_hex(&"ab".repeat(32)).unwrap()),
        key_id: Some("kid".into()),
        ..Default::default()
    })));
    engine_on(DynStorageBackend::new_box(EncryptingBackend::new(
        inner, cfg,
    )))
}

fn plain(inner: impl crate::storage::StorageBackend + 'static) -> Arc<DynEngine> {
    engine_on(DynStorageBackend::new_box(inner))
}

fn engine_on(backend: Box<DynStorageBackend<'static>>) -> Arc<DynEngine> {
    Arc::new(DeltaGliderEngine::new_with_backend(
        Arc::new(backend),
        &crate::config::Config::default(),
        None,
    ))
}

/// Deltas, a passthrough with user metadata, a delta-eligible key stored
/// as passthrough, and a folder marker.
async fn fill(engine: &DynEngine) {
    engine.create_bucket("b").await.unwrap();
    crate::deltaglider::store_deltas(engine, "rel", 3).await;
    let user = std::collections::HashMap::from([("foo".to_string(), "bar".to_string())]);
    engine
        .store("b", "rel/notes.txt", b"some notes", None, user)
        .await
        .unwrap();
    let no_delta = std::collections::HashMap::from([(
        crate::types::meta_keys::NO_DELTA.to_string(),
        "true".to_string(),
    )]);
    engine
        .store("b", "rel/plain.zip", b"not a delta", None, no_delta)
        .await
        .unwrap();
    engine
        .store("b", "rel/docs/", b"", None, Default::default())
        .await
        .unwrap();
}

async fn listed(engine: &Arc<DynEngine>, metadata: bool) -> Vec<(String, FileMetadata)> {
    client_list_page(engine, "b", "rel/", None, 1000, None, metadata, None, 50)
        .await
        .unwrap()
        .objects
}

/// The storage facts of the HEAD of `key`, as the LIST names them.
async fn head_facts(engine: &DynEngine, key: &str) -> std::collections::HashMap<String, String> {
    let meta = engine.head("b", key).await.unwrap();
    response_metadata_map(&meta, Reader::Authenticated)
        .into_iter()
        .filter(|(k, _)| k == "dg-note" || k == "dg-delta-size")
        .map(|(k, v)| (format!("x-amz-meta-{k}"), v))
        .collect()
}

/// Every listed entry: its listed facts, size and ETag equal the HEAD's.
async fn assert_list_matches_head(engine: &Arc<DynEngine>, label: &str) {
    let objects = listed(engine, true).await;
    let keys: Vec<&str> = objects.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        keys,
        [
            "rel/build-000.zip",
            "rel/build-001.zip",
            "rel/build-002.zip",
            "rel/docs/",
            "rel/notes.txt",
            "rel/plain.zip"
        ],
        "{label}"
    );
    for (key, meta) in &objects {
        let list = list_metadata_map(meta, Reader::Authenticated);
        assert_eq!(list, head_facts(engine, key).await, "{label}: {key}");
        let head = engine.head("b", key).await.unwrap();
        assert_eq!(meta.file_size, head.file_size, "{label}: {key} size");
        assert_eq!(meta.etag(), head.etag(), "{label}: {key} etag");
        assert!(
            !list.contains_key("x-amz-meta-foo"),
            "{label}: user metadata"
        );
    }
}

#[tokio::test]
async fn listed_storage_facts_equal_the_head_on_the_filesystem() {
    for enc in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let fs = crate::storage::FilesystemBackend::new(dir.path().to_path_buf())
            .await
            .unwrap();
        let engine = if enc { encrypted(fs) } else { plain(fs) };
        fill(&engine).await;
        assert_list_matches_head(&engine, &format!("fs enc={enc}")).await;
    }
}

/// On S3: the writer (process cache), and a node that knows nothing but
/// the durable listing facts. A `metadata=true` LIST sends no HEAD, but
/// for a delta on an encrypted backend.
#[tokio::test]
async fn listed_storage_facts_equal_the_head_on_s3_with_no_head() {
    use crate::storage::s3_test_support::{for_test_endpoint, for_test_endpoint_as_node};
    for enc in [false, true] {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        let writer = if enc {
            encrypted(for_test_endpoint(&endpoint))
        } else {
            plain(for_test_endpoint(&endpoint))
        };
        fill(&writer).await;
        // The facts of the 4 objects whose listing differs (3 deltas, and
        // with encryption every object but the marker) land in the
        // background.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let want = if enc { 5 } else { 3 };
        while fake
            .requests()
            .iter()
            .filter(|r| r.starts_with("PUT /b/.dg/facts/"))
            .count()
            < want
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let node = format!("cold-{enc}");
        let cold = if enc {
            encrypted(for_test_endpoint_as_node(&endpoint, &node))
        } else {
            plain(for_test_endpoint_as_node(&endpoint, &node))
        };
        for (engine, label) in [(&writer, "writer"), (&cold, "cold node")] {
            fake.clear();
            let objects = listed(engine, true).await;
            assert_eq!(objects.len(), 6);
            // Plain: none. Encrypted: the listed `.delta` size is the
            // ciphertext's, so a delta (only) is HEADed for dg-delta-size,
            // unless the metadata cache knows it (the writer's PUT).
            let heads: Vec<String> = fake
                .requests()
                .into_iter()
                .filter(|r| r.starts_with("HEAD /b/"))
                .collect();
            let max = if enc { 3 } else { 0 };
            assert!(
                heads.len() <= max && heads.iter().all(|r| r.ends_with(".delta")),
                "s3 enc={enc} {label}: {heads:?}"
            );
            assert_list_matches_head(engine, &format!("s3 enc={enc} {label}")).await;
        }
    }
}

/// An anonymous reader (a public prefix) gets no storage facts.
#[tokio::test]
async fn an_anonymous_reader_gets_no_storage_facts() {
    let dir = tempfile::tempdir().unwrap();
    let engine = plain(
        crate::storage::FilesystemBackend::new(dir.path().to_path_buf())
            .await
            .unwrap(),
    );
    fill(&engine).await;
    for (_, meta) in listed(&engine, true).await {
        assert!(list_metadata_map(&meta, Reader::Anonymous).is_empty());
    }
}
