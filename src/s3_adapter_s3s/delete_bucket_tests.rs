// SPDX-License-Identifier: BUSL-1.1

use super::*;
use crate::storage::DynStorageBackend;

/// Forwards to the engine and records each request's delimiter.
struct Recording<'a> {
    engine: &'a crate::deltaglider::DynEngine,
    delimiters: Mutex<Vec<Option<String>>>,
}

impl crate::iam::listing::Lister for Recording<'_> {
    async fn list(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys: u32,
        cursor: Option<&str>,
        metadata: bool,
    ) -> Result<crate::deltaglider::ListObjectsPage, crate::deltaglider::EngineError> {
        self.delimiters
            .lock()
            .unwrap()
            .push(delimiter.map(str::to_string));
        self.engine
            .list_objects(bucket, prefix, delimiter, max_keys, cursor, metadata)
            .await
    }
}

/// s3surface-12: DeleteBucket's emptiness check answers the first key
/// of the flat listing (the `example_key` of `BucketNotEmpty`), but
/// reads only `/`-delimited levels: a flat listing walked the whole
/// bucket on the filesystem backend.
#[tokio::test]
async fn first_visible_key_is_the_flat_first_key_by_delimited_reads() {
    let layouts: &[&[&str]] = &[
        &[],
        &["z.txt"],
        &["a/b/c.txt", "a.txt", "b.txt"],
        &["a/b/c.txt", "z.txt"],
        &["se/x.txt", "seg/y.txt"],
        &["docs/", "docs/a.txt"],
        &["deep/1/2/3/4.bin", "deep/1/2/5.bin"],
    ];
    for keys in layouts {
        let dir = tempfile::tempdir().unwrap();
        let backend: Box<crate::storage::DynStorageBackend<'static>> = DynStorageBackend::new_box(
            crate::storage::FilesystemBackend::new(dir.path().to_path_buf())
                .await
                .unwrap(),
        );
        let engine: crate::deltaglider::DynEngine =
            crate::deltaglider::DeltaGliderEngine::new_with_backend(
                Arc::new(backend),
                &crate::config::Config::default(),
                None,
            );
        engine.create_bucket("b").await.unwrap();
        for k in *keys {
            // A folder marker (`docs/`) has an empty body.
            let body: &[u8] = if k.ends_with('/') { b"" } else { b"x" };
            engine
                .store("b", k, body, None, Default::default())
                .await
                .unwrap();
        }
        let flat = engine
            .list_objects("b", "", None, 1, None, false)
            .await
            .unwrap()
            .objects
            .first()
            .map(|(k, _)| k.clone());
        let recording = Recording {
            engine: &engine,
            delimiters: Mutex::new(Vec::new()),
        };
        let got = first_visible_key(&recording, "b").await.unwrap();
        assert_eq!(got, flat, "layout {keys:?}");
        let delimiters = recording.delimiters.into_inner().unwrap();
        assert!(
            delimiters.iter().all(|d| d.as_deref() == Some("/")),
            "a flat listing for layout {keys:?}: {delimiters:?}"
        );
    }
}
