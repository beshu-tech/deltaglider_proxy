// SPDX-License-Identifier: BUSL-1.1

//! Review D3: a key that ends in `/` is a folder marker, an object of its
//! own. `PUT photos/` stores it, and `DELETE photos/` deletes only it.

use super::*;
use crate::config::Config;
use crate::storage::{EncryptingBackend, EncryptionConfig, EncryptionKey, FilesystemBackend};
use arc_swap::ArcSwap;

async fn engines() -> Vec<(tempfile::TempDir, DynEngine)> {
    let mut out = Vec::new();
    for encrypted in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let fs = FilesystemBackend::new(dir.path().to_path_buf())
            .await
            .unwrap();
        let backend: Box<dyn StorageBackend> = if encrypted {
            let key = EncryptionKey::from_hex(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            )
            .unwrap();
            let cfg = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
                key: Some(key),
                key_id: Some("kid-1".to_string()),
                ..Default::default()
            })));
            Box::new(EncryptingBackend::new(fs, cfg))
        } else {
            Box::new(fs)
        };
        let engine: DynEngine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None);
        engine.create_bucket("b").await.unwrap();
        out.push((dir, engine));
    }
    out
}

async fn keys(engine: &DynEngine, prefix: &str, delimiter: Option<&str>) -> Vec<String> {
    let page = engine
        .list_objects("b", prefix, delimiter, 1000, None, false)
        .await
        .unwrap();
    let mut out: Vec<String> = page.objects.into_iter().map(|(k, _)| k).collect();
    out.extend(page.common_prefixes);
    out.sort();
    out
}

#[tokio::test]
async fn a_folder_marker_is_an_object_of_its_own() {
    for (_dir, engine) in engines().await {
        engine
            .store("b", "photos/", b"", None, Default::default())
            .await
            .expect("PUT photos/ stores a zero-byte marker");
        engine
            .store("b", "photos/a.txt", b"hello", None, Default::default())
            .await
            .unwrap();

        let meta = engine.head("b", "photos/").await.unwrap();
        assert_eq!(meta.file_size, 0);
        let (body, _) = engine.retrieve("b", "photos/").await.unwrap();
        assert!(body.is_empty());
        assert_eq!(keys(&engine, "", Some("/")).await, vec!["photos/"]);
        assert_eq!(
            keys(&engine, "photos/", Some("/")).await,
            vec!["photos/", "photos/a.txt"]
        );
        assert_eq!(
            keys(&engine, "", None).await,
            vec!["photos/", "photos/a.txt"]
        );

        // DELETE photos/ removes the marker and nothing under it.
        engine.delete("b", "photos/").await.unwrap();
        assert!(matches!(
            engine.head("b", "photos/").await,
            Err(EngineError::NotFound(_))
        ));
        assert_eq!(keys(&engine, "", None).await, vec!["photos/a.txt"]);
        assert!(matches!(
            engine.delete("b", "photos/").await,
            Err(EngineError::NotFound(_))
        ));
    }
}

#[tokio::test]
async fn an_empty_folder_keeps_its_marker_and_the_bucket_is_not_empty() {
    for (_dir, engine) in engines().await {
        engine
            .store("b", "empty/sub/", b"", None, Default::default())
            .await
            .unwrap();
        assert_eq!(keys(&engine, "", None).await, vec!["empty/sub/"]);
        assert_eq!(keys(&engine, "empty/", Some("/")).await, vec!["empty/sub/"]);
        assert!(engine.delete_bucket("b").await.is_err());
        // Deleting a child object must not prune the marker's directory.
        engine
            .store("b", "empty/sub/x.txt", b"x", None, Default::default())
            .await
            .unwrap();
        engine.delete("b", "empty/sub/x.txt").await.unwrap();
        assert_eq!(keys(&engine, "", None).await, vec!["empty/sub/"]);
        engine.delete("b", "empty/sub/").await.unwrap();
        assert!(keys(&engine, "", None).await.is_empty());
        engine.delete_bucket("b").await.unwrap();
    }
}

/// Only a zero-byte body makes a marker; every other ingest path refuses a
/// marker key, so no data object is ever written under a marker's name.
#[tokio::test]
async fn a_marker_key_with_a_body_is_refused() {
    for (_dir, engine) in engines().await {
        assert!(matches!(
            engine
                .store("b", "photos/", b"data", None, Default::default())
                .await,
            Err(EngineError::InvalidArgument(_))
        ));
        assert!(matches!(
            engine
                .store_passthrough_chunked(
                    "b",
                    "photos/",
                    &[bytes::Bytes::from_static(b"data")],
                    4,
                    None,
                    Default::default(),
                )
                .await,
            Err(EngineError::InvalidArgument(_))
        ));
        assert!(keys(&engine, "", None).await.is_empty());
    }
}

/// A marker PUT goes through `validated_key`, not the ingest gate, so a
/// zero-byte `PUT` writes into the reserved `.dg/facts/` namespace, which
/// `validate_ingest` refuses for every data PUT.
#[tokio::test]
#[ignore = "review3: pending fix"]
async fn review3_a_marker_put_passes_the_ingest_gate() {
    for (_dir, engine) in engines().await {
        for key in [".dg/facts/x/"] {
            let err = engine
                .store("b", key, b"", None, Default::default())
                .await
                .err();
            assert!(
                matches!(err, Some(EngineError::InvalidArgument(_))),
                "PUT {key} (empty body) must be refused like a data PUT, got {err:?}"
            );
        }
    }
}
