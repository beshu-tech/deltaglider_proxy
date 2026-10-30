// SPDX-License-Identifier: BUSL-1.1

//! Server-side copy of a passthrough object: the backend copies the stored
//! file ([`StorageBackend::copy_within`]), and the engine does what a store
//! of the same object does around its write (the key's write lock, the
//! usage counter, the other stored form, the metadata cache).

use super::usage::CounterPrior;
use super::*;
use crate::storage::{CopyOutcome, ObjectAt, ObjectVariant};

impl<S: StorageBackend> DeltaGliderEngine<S> {
    /// Copy the passthrough object `src` (`source`: its HEAD, the
    /// generation pin) to `dst` inside the backend, with `metadata` (built
    /// by `transfer::destination_metadata`, as the store path would write
    /// it). `Ok(None)`: the backend cannot copy it (another backend or
    /// encryption key, a provider without CopyObject, a source that is not
    /// a passthrough object); nothing was written, and the caller copies
    /// the bytes. A source that changed since `source` is
    /// `PreconditionFailed`. The caller decides that the destination stores
    /// the object as it is (no delta). `lock`: the destination's write
    /// lock. `prior`: what the destination key holds, when the caller read
    /// it (`None`: read here).
    pub async fn copy_passthrough_within(
        &self,
        lock: &super::conditional::ObjectWriteGuard,
        src: (&str, &str),
        dst: (&str, &str),
        source: &FileMetadata,
        metadata: FileMetadata,
        prior: Option<CounterPrior>,
    ) -> Result<Option<StoreResult>, EngineError> {
        if !matches!(source.storage_info, StorageInfo::Passthrough)
            || !matches!(metadata.storage_info, StorageInfo::Passthrough)
        {
            return Ok(None);
        }
        let (src_key, src_ds) = self.validated_key(src.0, src.1)?;
        // A legacy object stored under its `.delta` name: the plain read
        // path knows it; the copy leaves it to the stream path.
        if source.original_name.strip_suffix(".delta") == Some(&src_key.filename) {
            return Ok(None);
        }
        self.ensure_within_passthrough_ceiling(source.file_size)?;
        let (dst_key, dst_ds) = self.validated_key_ingest(lock, dst.0, dst.1)?;
        let prior = match prior {
            Some(prior) => prior,
            None => self.prior_for_counter(dst.0, dst.1).await,
        };
        // The key's write lock only, like every passthrough write (see
        // `write_passthrough_locked`).
        // The metadata cache keeps the destination's entry until the copy
        // commits: a copy that is `Unsupported` falls back to a read of
        // the same object (a self-copy), and that read uses the entry.
        let outcome = self
            .storage
            .copy_within(
                ObjectVariant::Passthrough,
                ObjectAt {
                    bucket: src.0,
                    prefix: &src_ds,
                    filename: &src_key.filename,
                },
                ObjectAt {
                    bucket: dst.0,
                    prefix: &dst_ds,
                    filename: &dst_key.filename,
                },
                source,
                &metadata,
            )
            .await
            .inspect_err(|_| self.metadata_cache.invalidate(dst.0, dst.1))?;
        if outcome == CopyOutcome::Unsupported {
            return Ok(None);
        }
        // Committed: count it before the next await.
        self.record_commit(dst.0, &prior, &metadata);
        self.drop_other_form(
            lock,
            dst.0,
            &dst_ds,
            &dst_key.filename,
            ObjectVariant::Passthrough,
            &prior,
        )
        .await;
        self.metadata_cache.insert(dst.0, dst.1, metadata.clone());
        let size = metadata.file_size;
        Ok(Some(
            StoreResult::new(metadata, size).with_accounting(prior.replaced(), 0),
        ))
    }
}

#[cfg(test)]
mod tests {
    use crate::storage::encrypting::{ENCRYPTION_KEY_ID_KEY, ENCRYPTION_MARKER_KEY};
    use crate::storage::{
        same_stored_generation, CopyOutcome, DynStorageBackend, EncryptingBackend,
        EncryptionConfig, EncryptionKey, FilesystemBackend, ObjectAt, ObjectVariant,
        RoutingBackend, StorageBackend, StorageBackendExt,
    };
    use crate::types::FileMetadata;
    use std::collections::HashMap;
    use std::sync::Arc;

    /// Passthrough metadata whose content hash is `sha` repeated (hex, as
    /// a seal needs).
    fn meta(sha: &str) -> FileMetadata {
        FileMetadata::new_passthrough("a.jpg".into(), sha.repeat(32), "cd".repeat(16), 4, None)
    }

    #[test]
    fn same_stored_generation_truth_table() {
        let a = meta("aa");
        assert!(same_stored_generation(&a, &a.clone()));
        assert!(!same_stored_generation(&a, &meta("bb")), "other content");
        let mut later = a.clone();
        later.created_at += chrono::Duration::seconds(1);
        assert!(!same_stored_generation(&a, &later), "rewritten");
        let encrypted = |id: &str| {
            let mut e = a.clone();
            e.user_metadata
                .insert(ENCRYPTION_MARKER_KEY.into(), "v2".into());
            e.user_metadata
                .insert(ENCRYPTION_KEY_ID_KEY.into(), id.into());
            e
        };
        assert!(
            !same_stored_generation(&a, &encrypted("k1")),
            "re-encrypted in place"
        );
        assert!(
            !same_stored_generation(&encrypted("k1"), &encrypted("k2")),
            "re-encrypted under another key"
        );
        assert!(same_stored_generation(&encrypted("k1"), &encrypted("k1")));
        let delta = |r: &str, size: u64| {
            let mut d = FileMetadata::new_delta(
                "a.jpg".into(),
                "aa".into(),
                "m".into(),
                4,
                "reference.bin".into(),
                r.into(),
                size,
                None,
            );
            d.created_at = a.created_at;
            d
        };
        assert!(same_stored_generation(&delta("r", 9), &delta("r", 9)));
        assert!(
            !same_stored_generation(&delta("r", 9), &delta("q", 9)),
            "other reference"
        );
        assert!(
            !same_stored_generation(&delta("r", 9), &delta("r", 8)),
            "other delta"
        );
        assert!(!same_stored_generation(&a, &delta("r", 9)), "other form");
    }

    fn at<'a>(bucket: &'a str) -> ObjectAt<'a> {
        ObjectAt {
            bucket,
            prefix: "pics",
            filename: "a.jpg",
        }
    }

    /// Store `a.jpg` in `bucket` through `backend`; returns its metadata as
    /// a HEAD reads it (with the at-rest markers).
    async fn put(backend: &impl StorageBackend, bucket: &str) -> FileMetadata {
        backend.create_bucket(bucket).await.unwrap();
        backend
            .put_passthrough(
                bucket,
                "pics",
                "a.jpg",
                bytes::Bytes::from_static(b"data"),
                &meta("aa"),
            )
            .await
            .unwrap();
        backend
            .get_passthrough_metadata(bucket, "pics", "a.jpg")
            .await
            .unwrap()
    }

    /// A config that encrypts with a key of its own per id (in v2 the id is
    /// a label: a rotation is another key).
    fn keyed(id: &str) -> Arc<arc_swap::ArcSwap<EncryptionConfig>> {
        let hex = if id == "rotated" { "cd" } else { "ab" };
        Arc::new(arc_swap::ArcSwap::new(Arc::new(EncryptionConfig {
            key: Some(EncryptionKey::from_hex(&hex.repeat(32)).unwrap()),
            key_id: Some(id.into()),
            ..Default::default()
        })))
    }

    /// The encrypting wrapper copies stored bytes only when it would store
    /// them the same way: under its own key id. A source under another key
    /// (a key rotation) or in plaintext (encryption turned on later) is
    /// `Unsupported`: the caller re-encrypts through the proxy.
    #[tokio::test]
    async fn the_encrypting_wrapper_copies_only_its_own_ciphertext() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = keyed("kid");
        let enc = EncryptingBackend::new(
            FilesystemBackend::new(dir.path().to_path_buf())
                .await
                .unwrap(),
            cfg.clone(),
        );
        let source = put(&enc, "b").await;
        enc.create_bucket("c").await.unwrap();
        let copy = |m: FileMetadata| {
            let enc = &enc;
            async move {
                enc.copy_within(
                    ObjectVariant::Passthrough,
                    at("b"),
                    at("c"),
                    &m,
                    &meta("aa"),
                )
                .await
                .unwrap()
            }
        };
        assert_eq!(copy(source.clone()).await, CopyOutcome::Copied);
        assert_eq!(
            enc.get_passthrough("c", "pics", "a.jpg").await.unwrap(),
            b"data"
        );
        cfg.store(keyed("rotated").load_full());
        assert_eq!(copy(source.clone()).await, CopyOutcome::Unsupported);
        let plain = FilesystemBackend::new(dir.path().to_path_buf())
            .await
            .unwrap();
        let plain_source = put(&plain, "p").await;
        cfg.store(keyed("kid").load_full());
        let outcome = enc
            .copy_within(
                ObjectVariant::Passthrough,
                at("p"),
                at("c"),
                &plain_source,
                &meta("aa"),
            )
            .await
            .unwrap();
        assert_eq!(outcome, CopyOutcome::Unsupported);
    }

    /// Routing copies server-side only inside one backend instance.
    #[tokio::test]
    async fn routing_copies_only_inside_one_backend() {
        let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let fs = |p: std::path::PathBuf| async move {
            Arc::new(DynStorageBackend::new_box(
                FilesystemBackend::new(p).await.unwrap(),
            ))
        };
        let backends = HashMap::from([
            ("local-disk".to_string(), fs(d1.path().to_path_buf()).await),
            ("aws-dr".to_string(), fs(d2.path().to_path_buf()).await),
        ]);
        let routes = HashMap::from([
            ("releases".to_string(), ("local-disk".to_string(), None)),
            ("downloads".to_string(), ("local-disk".to_string(), None)),
            ("db-archive".to_string(), ("aws-dr".to_string(), None)),
        ]);
        let routing = RoutingBackend::new(backends, routes, "local-disk".to_string()).unwrap();
        let source = put(&routing, "releases").await;
        for b in ["downloads", "db-archive"] {
            routing.create_bucket(b).await.unwrap();
        }
        let copy = |dst: &'static str| {
            let (routing, source) = (&routing, &source);
            async move {
                routing
                    .copy_within(
                        ObjectVariant::Passthrough,
                        at("releases"),
                        at(dst),
                        source,
                        &meta("aa"),
                    )
                    .await
                    .unwrap()
            }
        };
        assert_eq!(copy("downloads").await, CopyOutcome::Copied);
        assert_eq!(copy("db-archive").await, CopyOutcome::Unsupported);
    }

    /// A re-encrypt between the copy's HEAD and the copy rewrites the
    /// source in place with the same content and created-at. The copy
    /// must see that the stored bytes changed: copied under the HEAD's
    /// plaintext markers, the ciphertext was served as the object.
    #[tokio::test]
    async fn a_source_re_encrypted_after_its_head_is_not_copied() {
        let dir = tempfile::tempdir().unwrap();
        let fs = || FilesystemBackend::new(dir.path().to_path_buf());
        let plain = EncryptingBackend::new(
            fs().await.unwrap(),
            Arc::new(arc_swap::ArcSwap::new(
                Arc::new(EncryptionConfig::default()),
            )),
        );
        let encrypting = EncryptingBackend::new(fs().await.unwrap(), keyed("kid"));
        let head = put(&plain, "b").await;
        plain.create_bucket("c").await.unwrap();
        encrypting
            .put_passthrough(
                "b",
                "pics",
                "a.jpg",
                bytes::Bytes::from_static(b"data"),
                &head,
            )
            .await
            .unwrap();

        let copied = plain
            .copy_within(
                ObjectVariant::Passthrough,
                at("b"),
                at("c"),
                &head,
                &meta("aa"),
            )
            .await;

        // Not copied: the plaintext wrapper sees a sealed source (v2) and
        // declines, or the inner pin sees another version.
        assert!(
            matches!(
                copied,
                Ok(CopyOutcome::Unsupported)
                    | Err(crate::storage::StorageError::PreconditionFailed(_))
            ),
            "{copied:?}"
        );
        assert!(plain.get_passthrough("c", "pics", "a.jpg").await.is_err());
    }
}
