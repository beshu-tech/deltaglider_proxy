// SPDX-License-Identifier: BUSL-1.1

//! Unit tests of the filesystem backend.

use super::*;

#[cfg(test)]
mod backend_tests {
    //! Unit tests for the filesystem backend guards that don't need a
    //! running proxy. Integration tests live in
    //! `tests/bucket_existence_test.rs`.
    use super::*;
    use crate::types::FileMetadata;

    /// Build a minimal FileMetadata for testing the put_* paths. The
    /// content is never read because put_* should fail before touching it.
    /// The pending set is process-wide: tests that assert on it or flush
    /// it run one at a time.
    static PENDING_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn dummy_metadata(filename: &str) -> FileMetadata {
        FileMetadata::new_passthrough(
            filename.to_string(),
            "0".repeat(64), // sha256 hex
            "0".repeat(32), // md5 hex
            0,
            None,
        )
    }

    /// Direct StorageBackend test: put_passthrough to a missing bucket
    /// must fail with BucketNotFound, NOT silently create a bucket root.
    #[tokio::test]
    async fn test_require_bucket_exists_rejects_put_passthrough() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");

        // Attempt to write without ever calling create_bucket.
        let err = backend
            .put_passthrough(
                "missing-bucket",
                "prefix",
                "file.bin",
                b"payload",
                &dummy_metadata("file.bin"),
            )
            .await
            .expect_err("must refuse");

        match err {
            StorageError::BucketNotFound(b) => assert_eq!(b, "missing-bucket"),
            other => panic!("expected BucketNotFound, got {:?}", other),
        }

        // The bucket directory must NOT have been created.
        assert!(
            !tmp.path().join("missing-bucket").exists(),
            "put_passthrough must not create the bucket root on failure"
        );
    }

    /// Same guard covers put_delta.
    #[tokio::test]
    async fn test_require_bucket_exists_rejects_put_delta() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");

        let err = backend
            .put_delta(
                "ghost",
                "ns",
                "f.delta",
                b"x",
                &dummy_metadata("f.delta"),
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .expect_err("must refuse");

        assert!(matches!(err, StorageError::BucketNotFound(_)));
        assert!(!tmp.path().join("ghost").exists());
    }

    /// Same guard covers put_reference.
    /// #63: a bucket DECLARED in config is pre-created at startup via
    /// ensure_declared_bucket, so its first write no longer 404s. This is
    /// declared intent only — the write path itself still refuses to create a
    /// bucket implicitly (test_require_bucket_exists_rejects_put_reference).
    #[tokio::test]
    async fn test_ensure_declared_bucket_creates_dir_then_write_succeeds() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");

        // Before: writing to an undeclared bucket 404s.
        assert!(matches!(
            backend
                .put_reference(
                    "declared",
                    "ns",
                    b"x",
                    &dummy_metadata("reference.bin"),
                    crate::deltaglider::RefWriteProof::for_tests()
                )
                .await,
            Err(StorageError::BucketNotFound(_))
        ));

        // Declare it (what startup does for each storage.buckets entry).
        backend
            .ensure_declared_bucket("declared")
            .await
            .expect("ensure_declared_bucket");
        assert!(tmp.path().join("declared").is_dir());

        // Now the first write succeeds — no explicit CreateBucket needed.
        backend
            .put_reference(
                "declared",
                "ns",
                b"x",
                &dummy_metadata("reference.bin"),
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .expect("write into declared bucket");

        // Idempotent: a second call is fine.
        backend
            .ensure_declared_bucket("declared")
            .await
            .expect("idempotent");
    }

    #[tokio::test]
    async fn test_require_bucket_exists_rejects_put_reference() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");

        let err = backend
            .put_reference(
                "ghost",
                "ns",
                b"ref",
                &dummy_metadata("reference.bin"),
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .expect_err("must refuse");

        assert!(matches!(err, StorageError::BucketNotFound(_)));
        assert!(!tmp.path().join("ghost").exists());
    }

    /// C-P0-1 regression: `ensure_dir` must NOT silently recreate the
    /// bucket root if a parallel `delete_bucket` removed it between
    /// `require_bucket_exists` and the actual write. Pre-fix,
    /// `ensure_dir` called `fs::create_dir_all(parent)` which happily
    /// resurrected `<root>/<bucket>/...` and the operator's deletion
    /// was silently undone.
    ///
    /// We exercise the race directly: create the bucket, remove it
    /// behind the backend's back, then call a put_* path. The
    /// `require_bucket_exists` precheck catches some races (race-A:
    /// delete BEFORE precheck), but here we simulate race-B: delete
    /// AFTER precheck. The check happens at the start of `put_*`; we
    /// run delete *after* `require_bucket_exists` would have passed.
    /// In practice the first race window is precheck → ensure_dir; the
    /// second is ensure_dir → atomic_write_with_metadata. This test
    /// pins the precheck → ensure_dir window.
    #[tokio::test]
    async fn test_ensure_dir_does_not_resurrect_deleted_bucket() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("racy").await.expect("create bucket");

        // Simulate: between require_bucket_exists and the write, the
        // bucket dir disappears. We can do that by removing it
        // directly with std::fs (the backend doesn't know).
        std::fs::remove_dir_all(tmp.path().join("racy")).unwrap();

        // Now put_passthrough — `require_bucket_exists` will catch this
        // because it's the first thing it checks. So this path proves
        // race-A is closed.
        let err = backend
            .put_passthrough("racy", "ns", "f.bin", b"payload", &dummy_metadata("f.bin"))
            .await
            .expect_err("must refuse");
        assert!(matches!(err, StorageError::BucketNotFound(_)));
        assert!(
            !tmp.path().join("racy").exists(),
            "must not have resurrected the bucket"
        );
    }

    /// Direct unit test of `ensure_dir`: when the bucket root is
    /// missing, even if something else points us at a path inside the
    /// bucket subtree, we must NOT create the bucket root. Pre-fix the
    /// `create_dir_all(parent)` path would happily build the whole
    /// tree from root downward.
    #[tokio::test]
    async fn test_ensure_dir_refuses_when_bucket_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");

        let bogus_path = tmp
            .path()
            .join("phantom-bucket")
            .join("deltaspaces")
            .join("p")
            .join("file.bin");
        let err = backend
            .ensure_dir("phantom-bucket", &bogus_path)
            .await
            .expect_err("must refuse to create dirs in a missing bucket");

        match err {
            StorageError::BucketNotFound(b) => assert_eq!(b, "phantom-bucket"),
            other => panic!("expected BucketNotFound, got {:?}", other),
        }
        assert!(
            !tmp.path().join("phantom-bucket").exists(),
            "ensure_dir must not silently materialise the bucket root"
        );
    }

    /// Same guard covers put_passthrough_chunked.
    #[tokio::test]
    async fn test_require_bucket_exists_rejects_put_passthrough_chunked() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");

        let chunks = vec![Bytes::from_static(b"hello"), Bytes::from_static(b"world")];
        let err = backend
            .put_passthrough_chunked(
                "ghost",
                "ns",
                "chunky.bin",
                &chunks,
                &dummy_metadata("chunky.bin"),
            )
            .await
            .expect_err("must refuse");

        assert!(matches!(err, StorageError::BucketNotFound(_)));
        assert!(!tmp.path().join("ghost").exists());
    }

    /// After create_bucket, put_passthrough should succeed.
    #[tokio::test]
    async fn test_put_after_create_bucket_succeeds() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");

        backend.create_bucket("real-bucket").await.expect("create");

        backend
            .put_passthrough(
                "real-bucket",
                "",
                "file.bin",
                b"payload",
                &dummy_metadata("file.bin"),
            )
            .await
            .expect("put after create should succeed");

        // File is under deltaspaces/ inside the bucket dir.
        assert!(tmp
            .path()
            .join("real-bucket")
            .join("deltaspaces")
            .join("file.bin")
            .exists());
    }

    #[tokio::test]
    async fn test_delete_delta_prunes_empty_nested_prefix_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");

        backend
            .put_delta(
                "bucket",
                "a/b",
                "file.bin",
                b"delta",
                &dummy_metadata("file.bin"),
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .expect("put delta");
        backend
            .delete_delta("bucket", "a/b", "file.bin")
            .await
            .expect("delete delta");

        assert!(!tmp
            .path()
            .join("bucket")
            .join("deltaspaces")
            .join("a")
            .exists());
        assert!(tmp.path().join("bucket").join("deltaspaces").exists());
    }

    #[tokio::test]
    async fn test_delete_reference_prunes_reference_only_prefix_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");
        let meta = FileMetadata::new_reference(
            "reference.bin".into(),
            "source.bin".into(),
            "0".repeat(64),
            "0".repeat(32),
            3,
            None,
        );

        backend
            .put_reference(
                "bucket",
                "only/ref",
                b"ref",
                &meta,
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .expect("put reference");
        backend
            .delete_reference(
                "bucket",
                "only/ref",
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .expect("delete reference");

        assert!(!tmp
            .path()
            .join("bucket")
            .join("deltaspaces")
            .join("only")
            .exists());
    }

    /// S3 prefixes are strings, not directories: a prefix that ends inside a
    /// name (`n`, `nightly/pg`) must still match the keys below it.
    #[tokio::test]
    async fn test_bulk_list_matches_prefixes_that_end_inside_a_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");
        for (prefix, name) in [("nightly", "pg-01.sql"), ("", "notes.txt"), ("", "top.txt")] {
            let meta =
                FileMetadata::new_passthrough(name.into(), "0".repeat(64), "0".repeat(32), 3, None);
            backend
                .put_passthrough("bucket", prefix, name, b"abc", &meta)
                .await
                .expect("put");
        }

        let keys = |listed: Vec<(String, FileMetadata)>| {
            let mut k: Vec<String> = listed.into_iter().map(|(k, _)| k).collect();
            k.sort();
            k
        };
        let list = |p: &'static str| {
            let backend = &backend;
            async move { backend.bulk_list_objects("bucket", p).await.expect("list") }
        };
        // Callers filter by prefix; the backend must not drop matching keys.
        let n = keys(list("n").await);
        assert!(n.contains(&"nightly/pg-01.sql".to_string()), "{n:?}");
        assert!(n.contains(&"notes.txt".to_string()), "{n:?}");
        let pg = keys(list("nightly/pg").await);
        assert!(pg.contains(&"nightly/pg-01.sql".to_string()), "{pg:?}");
        assert!(keys(list("nightly/x").await)
            .iter()
            .all(|k| !k.starts_with("nightly/x")));
        assert!(keys(list("missing/").await).is_empty());
    }

    /// The folder-size scan reads baselines from the same walk as the
    /// objects (no per-folder lookup), with the same prefix rule.
    #[tokio::test]
    async fn bulk_listing_reports_baselines_with_the_object_prefix_rule() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");
        let meta = FileMetadata::new_reference(
            "reference.bin".into(),
            "source.bin".into(),
            "0".repeat(64),
            "0".repeat(32),
            3,
            None,
        );
        for ds in ["fw/v1", "fw2", "other"] {
            backend
                .put_reference(
                    "bucket",
                    ds,
                    b"ref",
                    &meta,
                    crate::deltaglider::RefWriteProof::for_tests(),
                )
                .await
                .expect("put reference");
        }
        let listing = backend
            .bulk_list_objects_with_baselines("bucket", "fw", None, None)
            .await
            .expect("list");
        let mut keys: Vec<(String, u64)> = listing.baselines;
        keys.sort();
        assert_eq!(
            keys,
            [
                ("fw/v1/reference.bin".to_string(), 3),
                ("fw2/reference.bin".to_string(), 3)
            ]
        );
        assert!(listing.objects.is_empty(), "a baseline is never an object");
    }

    #[tokio::test]
    async fn test_delegated_list_hides_empty_and_reference_only_prefixes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");
        fs::create_dir_all(
            tmp.path()
                .join("bucket")
                .join("deltaspaces")
                .join("empty/child"),
        )
        .await
        .expect("create empty prefix");
        let meta = FileMetadata::new_reference(
            "reference.bin".into(),
            "source.bin".into(),
            "0".repeat(64),
            "0".repeat(32),
            3,
            None,
        );
        backend
            .put_reference(
                "bucket",
                "ghost",
                b"ref",
                &meta,
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .expect("put reference");

        let listed = backend
            .list_objects_delegated("bucket", "", Some("/"), 100, None)
            .await
            .expect("list")
            .expect("delegated");

        assert!(listed.objects.is_empty());
        assert!(listed.common_prefixes.is_empty());
    }

    /// Dot-DIRS with real content are legitimate user prefixes and must show up
    /// in delimiter listings (S3-backend + flat-listing parity). Only `.dg` is
    /// internal; dot-FILES remain hidden (atomic-write temp namespace).
    /// The OS path join resolves `.`/`..`/empty segments, so every path this
    /// backend builds — object reads and writes, raw delta/reference writes,
    /// and listings — refuses them instead of reaching another key's file.
    #[tokio::test]
    async fn aliasing_key_segments_are_refused_on_every_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");
        backend
            .put_passthrough(
                "bucket",
                "a",
                "secret.txt",
                b"s",
                &dummy_metadata("secret.txt"),
            )
            .await
            .expect("put");
        fn invalid<T>(r: Result<T, StorageError>) -> bool {
            matches!(r, Err(StorageError::InvalidKey(_)))
        }
        for prefix in ["a/.", "./a", "a/", "a/..", "x/../a"] {
            assert!(
                invalid(
                    backend
                        .get_passthrough_metadata("bucket", prefix, "secret.txt")
                        .await
                ),
                "read through {prefix:?}"
            );
            assert!(
                invalid(
                    backend
                        .put_delta(
                            "bucket",
                            prefix,
                            "x.zip",
                            b"d",
                            &dummy_metadata("x.zip"),
                            crate::deltaglider::RefWriteProof::for_tests()
                        )
                        .await
                ),
                "raw delta write through {prefix:?}"
            );
        }
        for filename in [".", ".."] {
            assert!(invalid(
                backend
                    .get_passthrough_metadata("bucket", "a", filename)
                    .await
            ));
        }
        for prefix in ["a/./", "a//", "./a/", "a/./s"] {
            assert!(
                invalid(
                    backend
                        .list_objects_delegated("bucket", prefix, Some("/"), 100, None)
                        .await
                ),
                "delimited list of {prefix:?}"
            );
        }
        for prefix in ["a/./", "./a/", "a//", "a/./s"] {
            assert!(
                invalid(backend.bulk_list_objects("bucket", prefix).await),
                "bulk list of {prefix:?}"
            );
        }
        // Plain and dot-leading names stay valid.
        assert!(backend
            .get_passthrough_metadata("bucket", "a", "secret.txt")
            .await
            .is_ok());
        assert!(backend
            .list_objects_delegated("bucket", "a/.hid", Some("/"), 100, None)
            .await
            .is_ok());
        assert!(backend.bulk_list_objects("bucket", "a/").await.is_ok());
    }

    #[tokio::test]
    async fn test_delegated_list_shows_dot_dirs_hides_dg_and_temp_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");

        backend
            .put_passthrough(
                "bucket",
                ".well-known",
                "cert.txt",
                b"pem",
                &dummy_metadata("cert.txt"),
            )
            .await
            .expect("put dot-dir object");
        backend
            .put_passthrough("bucket", "", "top.txt", b"x", &dummy_metadata("top.txt"))
            .await
            .expect("put root object");
        // Internal residue at the bucket root: a `.dg` dir and a temp file.
        let ds = tmp.path().join("bucket").join("deltaspaces");
        fs::create_dir_all(ds.join(".dg")).await.expect("mk .dg");
        fs::write(ds.join(".dg").join("reference.bin"), b"ref")
            .await
            .expect("write ref");
        fs::write(ds.join(".dg-tmp.upload"), b"partial")
            .await
            .expect("write temp");

        let listed = backend
            .list_objects_delegated("bucket", "", Some("/"), 100, None)
            .await
            .expect("list")
            .expect("delegated");

        assert_eq!(
            listed.common_prefixes,
            vec![".well-known/".to_string()],
            "dot-dir with data must list; .dg must stay hidden"
        );
        let keys: Vec<&str> = listed.objects.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["top.txt"], "temp files must stay hidden");

        // And the dot-dir's own level lists its content.
        let inner = backend
            .list_objects_delegated("bucket", ".well-known/", Some("/"), 100, None)
            .await
            .expect("list inner")
            .expect("delegated inner");
        let inner_keys: Vec<&str> = inner.objects.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(inner_keys, vec![".well-known/cert.txt"]);
    }

    #[tokio::test]
    async fn test_delete_bucket_removes_reference_only_and_empty_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");
        let meta = FileMetadata::new_reference(
            "reference.bin".into(),
            "source.bin".into(),
            "0".repeat(64),
            "0".repeat(32),
            3,
            None,
        );
        backend
            .put_reference(
                "bucket",
                "ghost",
                b"ref",
                &meta,
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .expect("put reference");
        let hidden = tmp
            .path()
            .join("bucket")
            .join("deltaspaces")
            .join("ghost")
            .join(".tmpSt4le0");
        fs::write(hidden, b"tmp").await.expect("write hidden");
        fs::create_dir_all(
            tmp.path()
                .join("bucket")
                .join("deltaspaces")
                .join("empty/child"),
        )
        .await
        .expect("create empty prefix");

        backend
            .delete_bucket("bucket")
            .await
            .expect("delete bucket");
        assert!(!tmp.path().join("bucket").exists());
    }

    /// S-P1-3 regression: zero-byte unmanaged files get the canonical
    /// empty-content MD5, NOT an empty string.
    #[test]
    fn unmanaged_etag_zero_size_is_canonical_empty_md5() {
        let mtime = chrono::Utc::now();
        assert_eq!(
            synthesise_unmanaged_etag(0, &mtime),
            "d41d8cd98f00b204e9800998ecf8427e",
            "empty unmanaged files must map to the canonical empty MD5"
        );
    }

    /// Pre-fix this test would have asserted `etag() == "\""` because
    /// `md5 = String::new()` rendered as a quoted empty string.
    /// Post-fix: stable, non-empty, hex-32 — looks like a real ETag
    /// to SDK consumers.
    #[test]
    fn unmanaged_etag_nonempty_is_stable_hex32() {
        let mtime = chrono::Utc::now();
        let a = synthesise_unmanaged_etag(1024, &mtime);
        let b = synthesise_unmanaged_etag(1024, &mtime);
        assert_eq!(a, b, "same (size, mtime) must produce same etag");
        assert_eq!(a.len(), 32, "must be hex-32 like a real MD5");
        assert!(
            a.chars().all(|c| c.is_ascii_hexdigit()),
            "must be valid hex"
        );
        assert_ne!(
            a, "d41d8cd98f00b204e9800998ecf8427e",
            "non-empty file must NOT collide with the empty-MD5 sentinel"
        );
    }

    /// Any change to size or mtime invalidates the etag — that's the
    /// property change-detection clients rely on.
    #[test]
    fn unmanaged_etag_size_change_invalidates() {
        let mtime = chrono::Utc::now();
        let a = synthesise_unmanaged_etag(1024, &mtime);
        let b = synthesise_unmanaged_etag(1025, &mtime);
        assert_ne!(a, b);
    }

    #[test]
    fn unmanaged_etag_mtime_change_invalidates() {
        let now = chrono::Utc::now();
        let later = now + chrono::Duration::seconds(1);
        let a = synthesise_unmanaged_etag(1024, &now);
        let b = synthesise_unmanaged_etag(1024, &later);
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn test_delete_bucket_removes_root_internal_residue() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");

        let bucket_dir = tmp.path().join("bucket");
        fs::write(bucket_dir.join(".tmp-lock"), b"stale")
            .await
            .unwrap();
        fs::create_dir_all(bucket_dir.join("tmp-work/subdir"))
            .await
            .expect("create tmp dir");
        fs::write(bucket_dir.join("tmp-work/subdir/cache.bin"), b"stale")
            .await
            .expect("write tmp payload");

        backend
            .delete_bucket("bucket")
            .await
            .expect("delete bucket with root residue");
        assert!(!tmp.path().join("bucket").exists());
    }

    #[tokio::test]
    async fn test_delete_bucket_rejects_visible_data() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");
        backend
            .put_passthrough(
                "bucket",
                "visible",
                "file.bin",
                b"payload",
                &dummy_metadata("file.bin"),
            )
            .await
            .expect("put passthrough");
        let meta = FileMetadata::new_reference(
            "reference.bin".into(),
            "source.bin".into(),
            "0".repeat(64),
            "0".repeat(32),
            3,
            None,
        );
        backend
            .put_reference(
                "bucket",
                "visible",
                b"ref",
                &meta,
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .expect("put reference");

        let err = backend
            .delete_bucket("bucket")
            .await
            .expect_err("non-empty bucket must be rejected");

        assert!(matches!(err, StorageError::BucketNotEmpty(_)));
        assert!(tmp.path().join("bucket").exists());
        assert!(tmp
            .path()
            .join("bucket")
            .join("deltaspaces")
            .join("visible")
            .join("reference.bin")
            .exists());
    }

    #[tokio::test]
    async fn get_reference_to_file_is_byte_identical() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");
        let payload: Vec<u8> = (0..50_000u32).flat_map(|n| n.to_le_bytes()).collect();
        let meta = FileMetadata::new_reference(
            "reference.bin".into(),
            "source.bin".into(),
            "0".repeat(64),
            "0".repeat(32),
            payload.len() as u64,
            None,
        );
        backend
            .put_reference(
                "bucket",
                "deltas",
                &payload,
                &meta,
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .expect("put reference");

        let dest = tmp.path().join("spool_ref.bin");
        let n = backend
            .get_reference_to_file("bucket", "deltas", &dest)
            .await
            .expect("reference to file");
        assert_eq!(n, payload.len() as u64);
        let back = fs::read(&dest).await.expect("read dest");
        assert_eq!(
            back, payload,
            "materialised reference must be byte-identical"
        );

        // dest pre-existing (e.g. a pre-created spool temp) must be overwritten,
        // not error.
        let n2 = backend
            .get_reference_to_file("bucket", "deltas", &dest)
            .await
            .expect("reference to file over existing dest");
        assert_eq!(n2, payload.len() as u64);

        // Missing reference → NotFound (not a silent empty file).
        let err = backend
            .get_reference_to_file("bucket", "absent", &dest)
            .await
            .expect_err("missing reference must error");
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    fn ref_meta(len: usize) -> FileMetadata {
        FileMetadata::new_reference(
            "reference.bin".into(),
            "source.bin".into(),
            "0".repeat(64),
            "0".repeat(32),
            len as u64,
            None,
        )
    }

    /// D2: a failed replace must leave the old reference intact. Before, the
    /// old file was deleted first, then the copy failed (or stopped short on
    /// a full disk across filesystems) and the deltaspace lost its baseline.
    #[tokio::test]
    async fn put_reference_from_file_failure_keeps_old_reference() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");
        backend
            .put_reference(
                "bucket",
                "d",
                b"old-baseline",
                &ref_meta(12),
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .expect("put reference");
        let missing = tmp.path().join("no-such-spool-file");
        backend
            .put_reference_from_file(
                "bucket",
                "d",
                &missing,
                &ref_meta(3),
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .expect_err("unreadable source must fail");
        assert_eq!(
            backend
                .get_reference("bucket", "d")
                .await
                .expect("still there"),
            b"old-baseline"
        );
    }

    /// D2: the stored reference must be its own file. A hardlink shared the
    /// inode with the source, so a later write to the source (or the xattr
    /// write for the reference) changed both.
    #[tokio::test]
    async fn put_reference_from_file_does_not_share_the_source_inode() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");
        let src = tmp.path().join("spool.bin");
        fs::write(&src, b"baseline").await.expect("write src");
        backend
            .put_reference_from_file(
                "bucket",
                "d",
                &src,
                &ref_meta(8),
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .expect("put from file");
        // Overwrite the source in place (same inode).
        {
            use std::io::{Seek, Write};
            let mut f = std::fs::OpenOptions::new().write(true).open(&src).unwrap();
            f.seek(std::io::SeekFrom::Start(0)).unwrap();
            f.write_all(b"XXXXXXXX").unwrap();
        }
        assert_eq!(
            backend.get_reference("bucket", "d").await.expect("get"),
            b"baseline"
        );
        assert!(
            xattr::get(&src, xattr_meta::XATTR_NAME).unwrap().is_none(),
            "reference metadata must not land on the source file"
        );
    }

    /// D4: a client key whose file name starts with `.` is a user object.
    /// LIST hid it and DeleteBucket deleted it as temp residue. Only this
    /// backend's own temp files are internal.
    #[tokio::test]
    async fn dot_prefixed_user_objects_are_listed_and_block_delete_bucket() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");
        let meta = |name: &str| {
            FileMetadata::new_passthrough(name.into(), "0".repeat(64), "0".repeat(32), 1, None)
        };
        backend
            .put_passthrough("bucket", "", ".env", b"x", &meta(".env"))
            .await
            .expect("put .env");
        backend
            .put_passthrough("bucket", "cfg", ".gitignore", b"x", &meta(".gitignore"))
            .await
            .expect("put cfg/.gitignore");
        // Temp residue of an interrupted atomic write (old and new names).
        let ds = tmp.path().join("bucket").join("deltaspaces");
        fs::write(ds.join("cfg").join(".tmpAb12Cd"), b"t")
            .await
            .unwrap();
        fs::write(ds.join("cfg").join(".dg-tmp.Zz99xx"), b"t")
            .await
            .unwrap();

        let mut flat: Vec<String> = backend
            .bulk_list_objects("bucket", "")
            .await
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        flat.sort();
        assert_eq!(flat, vec![".env".to_string(), "cfg/.gitignore".to_string()]);

        let delegated = backend
            .list_objects_delegated("bucket", "cfg/", Some("/"), 1000, None)
            .await
            .unwrap()
            .expect("filesystem delegates");
        let keys: Vec<&str> = delegated.objects.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["cfg/.gitignore"]);

        let err = backend
            .delete_bucket("bucket")
            .await
            .expect_err("dot-file objects are data");
        assert!(matches!(err, StorageError::BucketNotEmpty(_)));
        assert!(ds.join(".env").exists());
        assert!(ds.join("cfg").join(".gitignore").exists());
    }

    #[test]
    fn internal_temp_names() {
        for n in [".tmpAb12Cd", ".tmp000000", ".dg-tmp.x", ".dg-tmp.Zz99xx"] {
            assert!(is_internal_temp_name(n), "{n}");
        }
        for n in [
            ".env",
            ".gitignore",
            ".tmp",
            ".tmpfile.txt",
            ".tmp-lock",
            "a.tmpAb12Cd",
        ] {
            assert!(!is_internal_temp_name(n), "{n}");
        }
    }

    fn pending_has(path: &Path) -> bool {
        super::PENDING_FSYNC.lock().iter().any(|p| p == path)
    }

    /// Only an object write inside `with_deferred_fsync` defers its fsync;
    /// the flush makes it durable and empties the set. A reference write
    /// in the same scope, and any write outside it, fsync at once.
    #[tokio::test]
    async fn only_scoped_object_writes_defer_their_fsync() {
        let _serial = PENDING_TESTS.lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");
        let meta = |n: &str| {
            FileMetadata::new_passthrough(n.into(), "0".repeat(64), "0".repeat(32), 1, None)
        };
        let path = |n: &str| backend.passthrough_path("bucket", "d", n).unwrap();

        backend
            .put_passthrough("bucket", "d", "plain.bin", b"x", &meta("plain.bin"))
            .await
            .unwrap();
        assert!(!pending_has(&path("plain.bin")), "outside the scope: fsync");

        crate::storage::with_deferred_fsync(async {
            backend
                .put_passthrough("bucket", "d", "copy.bin", b"x", &meta("copy.bin"))
                .await
                .unwrap();
            backend
                .put_passthrough_chunked(
                    "bucket",
                    "d",
                    "chunked.bin",
                    &[Bytes::from_static(b"x")],
                    &meta("chunked.bin"),
                )
                .await
                .unwrap();
            backend
                .put_reference(
                    "bucket",
                    "d",
                    b"ref",
                    &ref_meta(3),
                    crate::deltaglider::RefWriteProof::for_tests(),
                )
                .await
                .unwrap();
        })
        .await;
        assert!(pending_has(&path("copy.bin")));
        assert!(pending_has(&path("chunked.bin")));
        let reference = backend.reference_path("bucket", "d").unwrap();
        assert!(
            !pending_has(&reference),
            "a reference write keeps its fsync"
        );

        backend.flush_pending().await.unwrap();
        assert!(!pending_has(&path("copy.bin")));
        assert!(!pending_has(&path("chunked.bin")));
    }

    /// The pending set stays bounded: the write that fills it flushes it.
    #[tokio::test]
    async fn the_pending_fsync_set_is_bounded() {
        let _serial = PENDING_TESTS.lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");
        crate::storage::with_deferred_fsync(async {
            for i in 0..super::PENDING_FSYNC_MAX + 10 {
                let name = format!("f{i}.bin");
                let meta = FileMetadata::new_passthrough(
                    name.clone(),
                    "0".repeat(64),
                    "0".repeat(32),
                    1,
                    None,
                );
                backend
                    .put_passthrough("bucket", "d", &name, b"x", &meta)
                    .await
                    .unwrap();
                assert!(super::PENDING_FSYNC.lock().len() < super::PENDING_FSYNC_MAX);
            }
        })
        .await;
        backend.flush_pending().await.unwrap();
    }

    /// storage-1: a DELETE of the last object of `a/b/c` prunes the empty
    /// ancestors while a PUT into another deltaspace (`a/b`, or the nested
    /// `a/b/c/d`) runs under a different engine lock, between its mkdir
    /// and its temp-file create. The PUT must still succeed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_prune_never_fails_a_concurrent_put_in_another_deltaspace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = std::sync::Arc::new(
            FilesystemBackend::new(tmp.path().to_path_buf())
                .await
                .expect("new backend"),
        );
        backend.create_bucket("bucket").await.expect("create");
        let meta = dummy_metadata("x.bin");
        let mut failures = Vec::new();
        for i in 0..600 {
            let put_prefix = if i % 2 == 0 { "a/b" } else { "a/b/c/d" };
            backend
                .put_passthrough("bucket", "a/b/c", "x.bin", b"x", &meta)
                .await
                .expect("seed");
            let (b1, b2, m) = (backend.clone(), backend.clone(), meta.clone());
            let del =
                tokio::spawn(
                    async move { b1.delete_passthrough("bucket", "a/b/c", "x.bin").await },
                );
            let put = tokio::spawn(async move {
                b2.put_passthrough("bucket", put_prefix, "y.bin", b"y", &m)
                    .await
            });
            del.await.unwrap().expect("delete");
            if let Err(e) = put.await.unwrap() {
                failures.push(format!("{put_prefix}: {e}"));
            }
            let _ = backend
                .delete_passthrough("bucket", put_prefix, "y.bin")
                .await;
        }
        assert!(
            failures.is_empty(),
            "{} failed PUTs, first: {:?}",
            failures.len(),
            failures.first()
        );
    }

    /// storage-12: a write creates its directories only through
    /// `ensure_dir`, which never re-creates a deleted bucket. A
    /// `create_dir_all` in a write path (as `put_reference_from_file` had)
    /// brings back a bucket deleted after `require_bucket_exists`. Only the
    /// backend root (`new`) and `create_bucket` may call it.
    #[test]
    fn only_new_and_create_bucket_call_create_dir_all() {
        let prod = [
            crate::source_scan::prod_text(include_str!("mod.rs")),
            crate::source_scan::prod_text(include_str!("write.rs")),
        ]
        .concat();
        let calls = prod
            .lines()
            .filter(|l| !l.trim_start().starts_with("//") && l.contains("create_dir_all("))
            .count();
        assert_eq!(
            calls, 2,
            "a write path calls create_dir_all; use ensure_dir"
        );
    }

    /// storage-2: a range past the object's end (the caller's size can be
    /// stale) underflowed `end - start + 1` or declared more bytes than the
    /// body has. Out-of-range is `InvalidRange`; an end past the object is
    /// clamped, so the declared length matches the body.
    #[tokio::test]
    async fn range_reads_out_of_bounds_are_an_error_and_ends_are_clamped() {
        use futures::StreamExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");
        backend
            .put_passthrough(
                "bucket",
                "p",
                "o.bin",
                b"0123456789",
                &dummy_metadata("o.bin"),
            )
            .await
            .unwrap();
        for (start, end) in [(20, 25), (10, 10), (8, 2)] {
            let res = backend
                .get_passthrough_stream_range("bucket", "p", "o.bin", start, end)
                .await;
            assert!(
                matches!(res, Err(StorageError::InvalidRange(_))),
                "({start},{end}) must be InvalidRange"
            );
        }
        let (stream, len) = backend
            .get_passthrough_stream_range("bucket", "p", "o.bin", 5, 100)
            .await
            .unwrap();
        let body: Vec<u8> = stream.map(|c| c.unwrap().to_vec()).concat().await;
        assert_eq!((len, body.as_slice()), (5, &b"56789"[..]));
    }

    /// storage-5: a write is durable only when its directory entry is: the
    /// Sync path fsyncs the parent directory after the rename, and the
    /// deferred path queues the parent for `flush_pending`.
    #[tokio::test]
    async fn a_write_fsyncs_its_parent_directory() {
        let _serial = PENDING_TESTS.lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");
        let synced = |dir: &Path| super::SYNCED.lock().iter().any(|p| p == dir);

        backend
            .put_passthrough("bucket", "s", "f.bin", b"x", &dummy_metadata("f.bin"))
            .await
            .unwrap();
        let sync_dir = backend.deltaspace_dir("bucket", "s").unwrap();
        assert!(synced(&sync_dir), "the Sync path must fsync the parent dir");

        let deferred_dir = backend.deltaspace_dir("bucket", "d").unwrap();
        crate::storage::with_deferred_fsync(async {
            for name in ["a.bin", "b.bin"] {
                backend
                    .put_passthrough("bucket", "d", name, b"x", &dummy_metadata(name))
                    .await
                    .unwrap();
            }
        })
        .await;
        let queued = super::PENDING_FSYNC
            .lock()
            .iter()
            .filter(|p| **p == deferred_dir)
            .count();
        // At most once (dedup); a concurrent test's flush may take it early.
        assert!(queued <= 1, "the parent dir is queued once, got {queued}");
        backend.flush_pending().await.unwrap();
        assert!(
            synced(&deferred_dir),
            "flush_pending must fsync the parent dir"
        );
    }
}

/// Review B11: a stat error on a bucket or listing directory is an error,
/// never "no bucket" or "no objects". An empty listing made a mirror tool
/// delete every copy; a missing bucket made the router pick another backend.
#[tokio::test]
async fn a_stat_error_never_reads_as_absent() {
    let dir = tempfile::tempdir().unwrap();
    let fs = FilesystemBackend::new(dir.path().to_path_buf())
        .await
        .unwrap();
    fs.create_bucket("b").await.unwrap();
    let meta = FileMetadata::new_passthrough("k".into(), "s".into(), "m".into(), 1, None);
    fs.put_passthrough("b", "d", "k", b"x", &meta)
        .await
        .unwrap();
    let bucket = fault::fail_io(&dir.path().join("b"), libc::EIO);
    assert!(fs.head_bucket("b").await.is_err());
    assert!(matches!(
        fs.put_passthrough("b", "d", "k2", b"x", &meta).await,
        Err(StorageError::Io(_))
    ));
    assert!(fs.delete_bucket("b").await.is_err());
    assert!(bucket.fired() >= 3);
    drop(bucket);
    let listing = fault::fail_io(&dir.path().join("b/deltaspaces"), libc::EIO);
    assert!(fs.bulk_list_objects("b", "").await.is_err());
    assert!(fs.list_deltaspaces("b").await.is_err());
    assert!(fs.list_reference_prefixes("b", "").await.is_err());
    assert!(fs
        .list_objects_delegated("b", "", Some("/"), 1000, None)
        .await
        .is_err());
    drop(listing);
    let object = fault::fail_io(&dir.path().join("b/deltaspaces/d/k"), libc::EIO);
    assert!(matches!(
        fs.get_passthrough("b", "d", "k").await,
        Err(StorageError::Io(_))
    ));
    assert!(fs.get_passthrough_stream("b", "d", "k").await.is_err());
    assert!(fs.get_passthrough_metadata("b", "d", "k").await.is_ok());
    drop(object);
    assert_eq!(fs.bulk_list_objects("b", "").await.unwrap().len(), 1);
}

/// One bucket `b` with, under `x/`: a passthrough `a.txt`, a delta
/// `app-2.zip` without its xattr, a delta `app-3.zip` whose xattr names
/// another file (a copy that cloned the source metadata), a delta
/// `app-4.zip` whose xattr is not JSON, and the folder marker `x/`.
async fn listing_fixture() -> (tempfile::TempDir, FilesystemBackend) {
    let dir = tempfile::tempdir().unwrap();
    let fs = FilesystemBackend::new(dir.path().to_path_buf())
        .await
        .unwrap();
    fs.create_bucket("b").await.unwrap();
    let pass = FileMetadata::new_passthrough("a.txt".into(), "s".into(), "m".into(), 1, None);
    fs.put_passthrough("b", "x", "a.txt", b"a", &pass)
        .await
        .unwrap();
    let delta = |name: &str| {
        FileMetadata::new_delta(
            name.into(),
            "s".into(),
            "m".into(),
            100,
            "x/reference.bin".into(),
            "r".into(),
            3,
            None,
        )
    };
    let proof = crate::deltaglider::RefWriteProof::for_tests();
    for (file, named) in [
        ("app-2.zip", "app-2.zip"),
        ("app-3.zip", "app-1.zip"),
        ("app-4.zip", "app-4.zip"),
    ] {
        fs.put_delta("b", "x", file, b"ddd", &delta(named), proof)
            .await
            .unwrap();
    }
    let deltas = dir.path().join("b/deltaspaces/x");
    xattr::remove(deltas.join("app-2.zip.delta"), xattr_meta::XATTR_NAME).unwrap();
    xattr::set(
        deltas.join("app-4.zip.delta"),
        xattr_meta::XATTR_NAME,
        b"not json {{{",
    )
    .unwrap();
    fs.put_directory_marker("b", "x/").await.unwrap();
    (dir, fs)
}

/// Review D4: both listing forms name each object by its stored file. The
/// flat listing named a delta without an xattr `x/app-2.zip.delta`, and a
/// delta with a cloned xattr by the SOURCE's name.
#[tokio::test]
async fn flat_and_delimited_listings_name_the_same_keys() {
    let (_dir, fs) = listing_fixture().await;
    let flat: std::collections::BTreeSet<String> = fs
        .bulk_list_objects("b", "x/")
        .await
        .unwrap()
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    let delimited: std::collections::BTreeSet<String> = fs
        .list_objects_delegated("b", "x/", Some("/"), 1000, None)
        .await
        .unwrap()
        .unwrap()
        .objects
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    let want: std::collections::BTreeSet<String> =
        ["x/", "x/a.txt", "x/app-2.zip", "x/app-3.zip", "x/app-4.zip"]
            .into_iter()
            .map(String::from)
            .collect();
    assert_eq!(flat, want, "flat listing");
    assert_eq!(delimited, want, "delimited listing");
}

/// Review D3: an object whose metadata cannot be read is still listed
/// (from its stat, as a delta when its name says so), and the deltaspace
/// scan counts it. A listing that dropped it let a mirror tool delete the
/// copies.
#[tokio::test]
async fn list_keeps_an_object_whose_metadata_cannot_be_read() {
    let (_dir, fs) = listing_fixture().await;
    let flat = fs.bulk_list_objects("b", "x/").await.unwrap();
    let (_, meta) = flat.iter().find(|(k, _)| k == "x/app-4.zip").unwrap();
    assert!(meta.is_unresolved_delta_stub(), "{:?}", meta.storage_info);
    assert_eq!(meta.file_size, 3, "the stored size");
    assert_eq!(meta.original_name, "app-4.zip");
    let scan = fs.scan_deltaspace("b", "x").await.unwrap();
    let mut names: Vec<&str> = scan.iter().map(|m| m.original_name.as_str()).collect();
    names.sort();
    assert_eq!(names, ["", "a.txt", "app-2.zip", "app-3.zip", "app-4.zip"]);
}
