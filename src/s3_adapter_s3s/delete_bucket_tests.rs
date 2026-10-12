// SPDX-License-Identifier: BUSL-1.1

use super::*;
use crate::storage::DynStorageBackend;

/// Forwards to the engine and counts its listing requests.
struct Recording<'a> {
    engine: &'a crate::deltaglider::DynEngine,
    calls: Mutex<Vec<Option<String>>>,
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
        self.calls
            .lock()
            .unwrap()
            .push(delimiter.map(str::to_string));
        self.engine
            .list_objects(bucket, prefix, delimiter, max_keys, cursor, metadata)
            .await
    }
}

async fn fs_engine() -> (tempfile::TempDir, crate::deltaglider::DynEngine) {
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
    (dir, engine)
}

/// DeleteBucket's emptiness check answers the first key of the flat
/// listing (the `example_key` of `BucketNotEmpty`) from one flat listing
/// page on the filesystem: the walk is sorted and stops after the page
/// (s3surface-12 read `/`-delimited levels because a flat listing walked
/// the whole bucket).
#[tokio::test]
async fn first_visible_key_is_the_flat_first_key() {
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
        let (_dir, engine) = fs_engine().await;
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
            calls: Mutex::new(Vec::new()),
        };
        let got = first_visible_key(&recording, "b").await.unwrap();
        assert_eq!(got, flat, "layout {keys:?}");
        let calls = recording.calls.into_inner().unwrap();
        assert_eq!(calls, [None], "layout {keys:?}");
    }
}

/// What counts as empty for DeleteBucket, pinned on the filesystem: a
/// bucket that holds only proxy internals (an orphan reference.bin, temp
/// files, empty folders) is empty; any user key (a folder marker, a
/// dot-file) is not.
#[tokio::test]
async fn what_counts_as_empty_on_the_filesystem() {
    let cases: &[(&[&str], Option<&str>)] = &[
        (&[], None),
        (&["a/reference.bin"], None),
        (&["a/b/reference.bin", "a/.dg-tmp.1"], None),
        (&["empty/sub/"], None),
        (&["a/reference.bin", "a/x.zip"], Some("a/x.zip")),
        (&["docs/"], Some("docs/")),
        (&[".env"], Some(".env")),
    ];
    for (files, want) in cases {
        let (dir, engine) = fs_engine().await;
        let ds = dir.path().join("b").join("deltaspaces");
        for f in *files {
            let path = ds.join(f);
            if f.ends_with('/') && *f != "docs/" {
                std::fs::create_dir_all(&path).unwrap();
                continue;
            }
            if *f == "docs/" {
                engine
                    .store("b", f, b"", None, Default::default())
                    .await
                    .unwrap();
                continue;
            }
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"x").unwrap();
        }
        let got = first_visible_key(&engine, "b").await.unwrap();
        assert_eq!(got.as_deref(), *want, "{files:?}");
    }
}

/// The same rules on S3, and the cost: one flat LIST request, also when
/// the first keys of the bucket are proxy internals. Before: one delimited
/// LIST per level and a flat one (5 requests for `a/b/c/x.zip.delta`).
#[tokio::test]
async fn what_counts_as_empty_on_s3_and_its_cost() {
    use crate::usage_scanner::test_support::{fake_s3_engine, put_raw_many};
    let cases: &[(&[&str], Option<&str>)] = &[
        (&[], None),
        (&["a/reference.bin"], None),
        (&[".dg/facts/x"], None),
        (&["a/b/c/x.zip.delta"], Some("a/b/c/x.zip")),
        (&["a/reference.bin", "a/x.zip.delta"], Some("a/x.zip")),
        (&["docs/"], Some("docs/")),
        (&[".env"], Some(".env")),
    ];
    for (keys, want) in cases {
        let (engine, fake, endpoint) = fake_s3_engine().await;
        put_raw_many(&endpoint, keys.iter().map(|k| k.to_string())).await;
        fake.clear();
        let got = first_visible_key(&engine, "b").await.unwrap();
        assert_eq!(got.as_deref(), *want, "{keys:?}");
        let requests = fake.requests();
        let lists = requests
            .iter()
            .filter(|r| r.contains("list-type=2"))
            .count();
        assert!(
            lists <= 1 && lists == requests.len(),
            "{keys:?}: {requests:?}"
        );
    }
}
