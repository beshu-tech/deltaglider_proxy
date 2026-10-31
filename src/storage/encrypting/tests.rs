// SPDX-License-Identifier: BUSL-1.1

//! Unit tests of the encrypting wrapper (format v2, issue #120). The format
//! itself is tested in `v2.rs`, the sealed metadata in `seal.rs`.

use super::*;
use crate::storage::StorageBackendExt;
use bytes::Bytes;
use futures::TryStreamExt;

const F: usize = v2::FRAME_PLAINTEXT;

fn key(byte: u8) -> EncryptionKey {
    EncryptionKey::from_hex(&format!("{byte:02x}").repeat(32)).unwrap()
}

fn shared(cfg: EncryptionConfig) -> Arc<ArcSwap<EncryptionConfig>> {
    Arc::new(ArcSwap::new(Arc::new(cfg)))
}

/// A backend config that encrypts with `k` under the id `id`.
fn keyed(k: EncryptionKey, id: &str) -> EncryptionConfig {
    EncryptionConfig {
        key: Some(k),
        key_id: Some(id.to_string()),
        ..Default::default()
    }
}

fn sha_hex(data: &[u8]) -> String {
    hex::encode(<sha2::Sha256 as sha2::Digest>::digest(data))
}

fn md5_hex(data: &[u8]) -> String {
    hex::encode(<md5::Md5 as md5::Digest>::digest(data))
}

/// Passthrough metadata as the engine writes it.
fn pmeta(name: &str, data: &[u8]) -> FileMetadata {
    FileMetadata::new_passthrough(
        name.into(),
        sha_hex(data),
        md5_hex(data),
        data.len() as u64,
        None,
    )
}

/// Delta metadata whose stored body is `delta`.
fn dmeta(name: &str, delta: &[u8]) -> FileMetadata {
    FileMetadata::new_delta(
        name.into(),
        "ab".repeat(32),
        "cd".repeat(16),
        delta.len() as u64 + 1000,
        "reference.bin".into(),
        "ef".repeat(32),
        delta.len() as u64,
        None,
    )
}

/// Reference metadata whose body is `data`.
fn rmeta(data: &[u8]) -> FileMetadata {
    FileMetadata::new_reference(
        "reference.bin".into(),
        "app.zip".into(),
        sha_hex(data),
        md5_hex(data),
        data.len() as u64,
        None,
    )
}

fn body_of(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

async fn collect(stream: ByteStream) -> Result<Vec<u8>, StorageError> {
    let parts: Vec<Bytes> = stream.try_collect().await?;
    Ok(parts.concat())
}

/// The bytes of a range read, or its error (the open or the stream).
async fn read_range<B: StorageBackend + 'static>(
    w: &EncryptingBackend<B>,
    bucket: &str,
    prefix: &str,
    name: &str,
    start: u64,
    end: u64,
) -> Result<Vec<u8>, StorageError> {
    let read = w
        .open_passthrough_range(bucket, prefix, name, start, end)
        .await?;
    collect(read.stream).await
}

fn has_seal(m: &FileMetadata) -> bool {
    m.user_metadata.contains_key(seal::SEALED_KEY)
}

fn has_any_marker(m: &FileMetadata) -> bool {
    ENCRYPTION_MARKER_KEYS
        .iter()
        .any(|k| m.user_metadata.contains_key(*k))
}

struct Fs {
    dir: tempfile::TempDir,
    w: EncryptingBackend<crate::storage::FilesystemBackend>,
    cfg: Arc<ArcSwap<EncryptionConfig>>,
}

async fn fs(cfg: EncryptionConfig) -> Fs {
    let dir = tempfile::tempdir().unwrap();
    let inner = crate::storage::FilesystemBackend::new(dir.path().join("data"))
        .await
        .unwrap();
    let cfg = shared(cfg);
    let w = EncryptingBackend::new(inner, cfg.clone());
    for b in ["b", "b2"] {
        w.create_bucket(b).await.unwrap();
    }
    Fs { dir, w, cfg }
}

impl Fs {
    fn spool(&self) -> crate::deltaglider::spool::SpoolDir {
        crate::deltaglider::spool::SpoolDir::new(self.dir.path().join("spool"), 256 * 1024 * 1024)
            .unwrap()
    }

    async fn put(&self, prefix: &str, name: &str, data: &[u8]) {
        self.w
            .put_passthrough(
                "b",
                prefix,
                name,
                Bytes::copy_from_slice(data),
                &pmeta(name, data),
            )
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod wrapper_tests {
    use super::*;
    use crate::deltaglider::RefWriteProof;
    use crate::storage::RewriteBasis;
    use std::sync::Mutex;

    #[test]
    fn ciphertext_listings_without_a_cache_hit_are_stored_only() {
        let m = FileMetadata::fallback(
            "x".into(),
            1,
            "e".into(),
            chrono::Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        let objects = vec![
            ("a.bin".to_string(), m.clone()),
            ("b.bin".to_string(), m.clone()),
            ("dir/".to_string(), m),
        ];
        let mut sizes = vec![ListedSize::Listed, ListedSize::Cached, ListedSize::Listed];
        downgrade_ciphertext_listings(&objects, &mut sizes);
        assert_eq!(
            sizes,
            [
                ListedSize::StoredOnly,
                ListedSize::Cached,
                ListedSize::Listed
            ]
        );
    }

    /// The copy paths strip every marker, the seal included, so a copy
    /// never carries a stale "this is encrypted" claim onto another body.
    #[test]
    fn strip_encryption_markers_removes_every_marker_and_preserves_rest() {
        let mut md = std::collections::HashMap::new();
        md.insert(ENCRYPTION_MARKER_KEY.to_string(), "v2".into());
        md.insert(ENCRYPTION_KEY_ID_KEY.to_string(), "k1".into());
        md.insert(seal::SEALED_KEY.to_string(), "2.abc".into());
        md.insert("user-tag".to_string(), "keep-me".into());
        strip_encryption_markers(&mut md);
        assert_eq!(md.len(), 1, "{md:?}");
        assert_eq!(md.get("user-tag").map(String::as_str), Some("keep-me"));
    }

    #[test]
    fn a_fingerprint_label_is_the_hex_of_the_v2_fingerprint() {
        let label = fingerprint_label(&key(0x11));
        assert_eq!(
            label,
            format!(
                "fp-{}",
                hex::encode(v2::SiteKey::new(&key(0x11)).fingerprint())
            )
        );
        assert_eq!(label.len(), 3 + 2 * v2::FP_LEN);
        assert_ne!(label, fingerprint_label(&key(0x12)));
    }

    /// storage-2: the adapter clamps a range against a size from the
    /// metadata cache, which can be stale. Out-of-range is an `InvalidRange`
    /// error, never a panic; an end past the object is clamped.
    #[tokio::test]
    async fn range_reads_out_of_bounds_are_an_error_not_a_panic() {
        let r = fs(keyed(key(1), "kid-1")).await;
        r.put("p", "o.bin", b"0123456789").await;
        for (start, end) in [(20, 25), (10, 10), (8, 2), (70_000, 70_010)] {
            let res =
                r.w.open_passthrough_range("b", "p", "o.bin", start, end)
                    .await;
            assert!(
                matches!(res, Err(StorageError::InvalidRange(_))),
                "({start},{end}) must be InvalidRange"
            );
        }
        let crate::storage::RangeRead { stream, len, .. } =
            r.w.open_passthrough_range("b", "p", "o.bin", 5, 100)
                .await
                .unwrap();
        assert_eq!(
            (len, collect(stream).await.unwrap()),
            (5, b"56789".to_vec())
        );
    }

    /// Every write path stores a v2 body of exactly `stored_len` bytes and
    /// sealed metadata, and reads back byte-identical, across the frame
    /// boundaries (empty, sub-frame, exact frames, multi-frame + tail).
    #[tokio::test]
    async fn every_write_path_round_trips_at_frame_boundaries() {
        let r = fs(keyed(key(1), "kid-1")).await;
        let sd = r.spool();
        for size in [
            0usize,
            1,
            100,
            F - 1,
            F,
            F + 1,
            2 * F,
            2 * F + 7,
            5 * F + 123,
        ] {
            let data = body_of(size);
            let prefix = format!("s{size}");
            let p = prefix.as_str();
            let meta = pmeta("x", &data);
            let src = r.dir.path().join(format!("src{size}"));
            std::fs::write(&src, &data).unwrap();
            let third = size / 3;
            let parts: Vec<std::path::PathBuf> = [0..third, third..2 * third, 2 * third..size]
                .into_iter()
                .enumerate()
                .map(|(i, range)| {
                    let path = r.dir.path().join(format!("part{size}-{i}"));
                    std::fs::write(&path, &data[range]).unwrap();
                    path
                })
                .collect();
            let chunks: Vec<Bytes> = data.chunks(7777).map(Bytes::copy_from_slice).collect();
            let w = &r.w;
            w.put_passthrough("b", p, "buffered", Bytes::from(data.clone()), &meta)
                .await
                .unwrap();
            w.put_passthrough_file(
                "b",
                p,
                "file",
                &src,
                &meta,
                SpoolBudget::new(&sd, None, None),
            )
            .await
            .unwrap();
            w.put_passthrough_parts(
                "b",
                p,
                "parts",
                &parts,
                &meta,
                SpoolBudget::new(&sd, None, None),
            )
            .await
            .unwrap();
            w.put_passthrough_chunked("b", p, "chunked", &chunks, &meta)
                .await
                .unwrap();
            w.put_delta(
                "b",
                p,
                "app.zip",
                Bytes::from(data.clone()),
                &dmeta("app.zip", &data),
                RefWriteProof::for_tests(),
            )
            .await
            .unwrap();
            w.put_reference(
                "b",
                p,
                Bytes::from(data.clone()),
                &rmeta(&data),
                RefWriteProof::for_tests(),
            )
            .await
            .unwrap();

            let want_wire = v2::stored_len(size as u64).unwrap();
            for name in ["buffered", "file", "parts", "chunked"] {
                let at = format!("size {size} {name}");
                let wire = w.inner.get_passthrough("b", p, name).await.unwrap();
                assert_eq!(wire.len() as u64, want_wire, "{at}");
                assert_eq!(wire[..4], v2::MAGIC, "{at}");
                let raw = w
                    .inner
                    .get_passthrough_metadata("b", p, name)
                    .await
                    .unwrap();
                assert!(has_seal(&raw), "{at}: no seal");
                assert_eq!(raw.user_metadata[ENCRYPTION_MARKER_KEY], seal::DISK_MARKER);
                assert!(raw.md5.is_empty(), "{at}: md5 in clear");
                assert!(raw.file_sha256.starts_with("sealed:"), "{at}");
                assert!(!raw.user_metadata.contains_key(ENCRYPTION_KEY_ID_KEY));

                assert_eq!(w.get_passthrough("b", p, name).await.unwrap(), data, "{at}");
                let shown = w.get_passthrough_metadata("b", p, name).await.unwrap();
                assert_eq!(shown.md5, md5_hex(&data), "{at}");
                assert_eq!(shown.file_sha256, sha_hex(&data), "{at}");
                assert_eq!(shown.file_size, size as u64, "{at}");
                assert!(!has_seal(&shown), "{at}: the seal leaked");
                if size > 10 {
                    let (s, e) = (size as u64 / 2 - 5, size as u64 / 2 + 5);
                    let got = read_range(w, "b", p, name, s, e).await.unwrap();
                    assert_eq!(got, data[s as usize..=e as usize], "{at} range");
                }
            }
            assert_eq!(w.get_delta("b", p, "app.zip").await.unwrap(), data);
            let raw = w.inner.get_delta_metadata("b", p, "app.zip").await.unwrap();
            assert!(has_seal(&raw) && raw.file_size == 0, "size {size}: delta");
            let shown = w.get_delta_metadata("b", p, "app.zip").await.unwrap();
            assert_eq!(shown.file_size, size as u64 + 1000);
            assert_eq!(w.get_reference("b", p).await.unwrap(), data);
            let source = w.open_reference_source("b", p).await.unwrap();
            assert_eq!(source.size, size as u64);
            assert_eq!(source.read_all().await.unwrap(), data, "size {size}: ref");
        }
    }

    /// A sealed reference streams into the caller's file frame by frame
    /// (one frame of memory, whatever its size). A changed frame fails the
    /// write instead of writing what follows it.
    #[tokio::test]
    async fn a_sealed_reference_is_written_frame_by_frame_and_checked() {
        let r = fs(keyed(key(1), "kid-1")).await;
        let data = body_of(3 * F + 5);
        r.w.put_reference(
            "b",
            "p",
            Bytes::from(data.clone()),
            &rmeta(&data),
            RefWriteProof::for_tests(),
        )
        .await
        .unwrap();
        let out = r.dir.path().join("ref.out");
        let source = r.w.open_reference_source("b", "p").await.unwrap();
        assert_eq!(source.size, data.len() as u64);
        assert_eq!(source.write_to(&out).await.unwrap(), data.len() as u64);
        assert_eq!(std::fs::read(&out).unwrap(), data);

        let stored = r.dir.path().join("data/b/deltaspaces/p/reference.bin");
        let mut wire = std::fs::read(&stored).unwrap();
        let in_frame_2 = v2::HEADER_LEN + v2::FRAME_WIRE_LEN + 10;
        wire[in_frame_2] ^= 1;
        std::fs::write(&stored, &wire).unwrap();
        let source = r.w.open_reference_source("b", "p").await.unwrap();
        assert!(
            source.write_to(&out).await.is_err(),
            "a changed frame was written"
        );
    }

    // ── A spy backend: one passthrough object in memory ──

    struct CountingBackend {
        bytes: Mutex<Vec<u8>>,
        metadata: Mutex<Option<FileMetadata>>,
        ranges_requested: Mutex<Vec<(u64, u64)>>,
        /// Make metadata READS fail (a transient backend error).
        fail_meta_reads: std::sync::atomic::AtomicBool,
        /// Spool whose free budget `put_passthrough_file` records.
        probe_spool: Option<crate::deltaglider::spool::SpoolDir>,
        /// (source path, free spool MiB) per `put_passthrough_file` call.
        file_puts: Mutex<Vec<(std::path::PathBuf, usize)>>,
        /// What `listing_tokens` answers.
        tokens: Mutex<Vec<Option<crate::storage::ListedToken>>>,
        /// What `keep_listing_token` was handed.
        kept: Mutex<Vec<crate::storage::ListedToken>>,
    }

    impl CountingBackend {
        fn new() -> Self {
            Self {
                bytes: Mutex::new(Vec::new()),
                metadata: Mutex::new(None),
                ranges_requested: Mutex::new(Vec::new()),
                fail_meta_reads: Default::default(),
                probe_spool: None,
                file_puts: Mutex::new(Vec::new()),
                tokens: Mutex::new(Vec::new()),
                kept: Mutex::new(Vec::new()),
            }
        }

        fn slice(&self, start: u64, end: u64) -> Result<Vec<u8>, StorageError> {
            let bytes = self.bytes.lock().unwrap();
            if start >= bytes.len() as u64 {
                return Err(StorageError::InvalidRange(format!("{start}-{end}")));
            }
            let end = end.min(bytes.len() as u64 - 1);
            Ok(bytes[start as usize..=end as usize].to_vec())
        }
    }

    fn cb_err() -> StorageError {
        StorageError::Other("CountingBackend: not implemented for this test".into())
    }

    fn once(data: Vec<u8>) -> ByteStream {
        Box::pin(futures::stream::once(async move { Ok(Bytes::from(data)) }))
    }

    impl StorageBackend for CountingBackend {
        async fn copy_within(
            &self,
            _: crate::storage::ObjectVariant,
            _: crate::storage::ObjectAt<'_>,
            _: crate::storage::ObjectAt<'_>,
            _: &FileMetadata,
            _: &FileMetadata,
        ) -> Result<crate::storage::CopyOutcome, StorageError> {
            Ok(crate::storage::CopyOutcome::Unsupported)
        }
        async fn reference_fence(
            &self,
            b: &str,
            p: &str,
        ) -> Result<crate::storage::RefFence, StorageError> {
            crate::storage::unfenced_reference_fence(self, b, p).await
        }
        async fn reference_fence_with_metadata(
            &self,
            b: &str,
            p: &str,
        ) -> Result<(crate::storage::RefFence, Option<FileMetadata>), StorageError> {
            crate::storage::unfenced_reference_fence_with_metadata(self, b, p).await
        }
        async fn write_reference(
            &self,
            _: &str,
            _: &str,
            _: crate::storage::RefWrite<'_>,
            _: &RefWriteProof,
        ) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn write_reference_fenced(
            &self,
            b: &str,
            p: &str,
            op: crate::storage::RefWrite<'_>,
            _: &crate::storage::RefFence,
            proof: &RefWriteProof,
        ) -> Result<crate::storage::RefFence, StorageError> {
            crate::storage::unfenced_reference_write(self, b, p, op, proof).await
        }
        async fn open_object(
            &self,
            b: &str,
            p: &str,
            o: StoredObject<'_>,
        ) -> Result<crate::storage::ObjectRead, StorageError> {
            crate::storage::open_object_by_parts(self, b, p, o).await
        }
        async fn open_reference_source(
            &self,
            b: &str,
            p: &str,
        ) -> Result<ReferenceSource, StorageError> {
            crate::storage::open_reference_source_by_parts(self, b, p).await
        }
        /// One read: the range, the metadata and the stored total.
        async fn open_passthrough_range(
            &self,
            b: &str,
            p: &str,
            f: &str,
            start: u64,
            end: u64,
        ) -> Result<RangeRead, StorageError> {
            self.ranges_requested.lock().unwrap().push((start, end));
            let metadata = self.get_passthrough_metadata(b, p, f).await?;
            let slice = self.slice(start, end)?;
            Ok(RangeRead {
                len: slice.len() as u64,
                stream: once(slice),
                metadata,
                stored_total: self.bytes.lock().unwrap().len() as u64,
            })
        }
        async fn listing_tokens(
            &self,
            _: &str,
            objects: &[(String, FileMetadata)],
        ) -> Vec<Option<crate::storage::ListedToken>> {
            let mut t = self.tokens.lock().unwrap().clone();
            t.resize(objects.len(), None);
            t
        }
        async fn keep_listing_token(&self, _: &str, t: &crate::storage::ListedToken) {
            self.kept.lock().unwrap().push(t.clone());
        }
        fn lite_list_carries_logical_facts(&self, _: &str) -> bool {
            false
        }
        async fn read_key_claim(
            &self,
            _: &str,
        ) -> Result<Option<crate::storage::KeyClaim>, crate::storage::StorageError> {
            Ok(None)
        }
        async fn write_key_claim(
            &self,
            _: &str,
            _: crate::storage::ClaimWrite<'_>,
        ) -> Result<crate::storage::ClaimOutcome, crate::storage::StorageError> {
            Ok(crate::storage::ClaimOutcome::Written)
        }
        async fn read_conversion_marker(
            &self,
            _: &str,
        ) -> Result<crate::storage::MarkerRead, crate::storage::StorageError> {
            Ok(crate::storage::MarkerRead {
                marker: None,
                server_now: None,
            })
        }
        async fn write_conversion_marker(
            &self,
            _: &str,
            _: crate::storage::MarkerWrite<'_>,
        ) -> Result<crate::storage::ClaimOutcome, crate::storage::StorageError> {
            Ok(crate::storage::ClaimOutcome::Written)
        }
        async fn get_passthrough_metadata_for_rewrite(
            &self,
            b: &str,
            p: &str,
            f: &str,
        ) -> Result<(FileMetadata, RewriteBasis), StorageError> {
            let meta = self.get_passthrough_metadata(b, p, f).await?;
            Ok((meta.clone(), RewriteBasis::stored(meta)))
        }
        async fn put_passthrough_metadata(
            &self,
            _: &str,
            _: &str,
            _: &str,
            m: &FileMetadata,
            _: &RewriteBasis,
        ) -> Result<(), StorageError> {
            *self.metadata.lock().unwrap() = Some(m.clone());
            Ok(())
        }

        // All other trait methods: not needed for these tests.
        async fn create_bucket(&self, _: &str) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn delete_bucket(&self, _: &str) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
            Err(cb_err())
        }
        async fn list_buckets_with_dates(
            &self,
        ) -> Result<Vec<(String, chrono::DateTime<chrono::Utc>)>, StorageError> {
            Err(cb_err())
        }
        async fn head_bucket(&self, _: &str) -> Result<bool, StorageError> {
            Err(cb_err())
        }
        async fn get_object(
            &self,
            _: &str,
            _: &str,
            o: crate::storage::StoredObject<'_>,
        ) -> Result<Vec<u8>, StorageError> {
            match o {
                crate::storage::StoredObject::Passthrough(_) => {
                    Ok(self.bytes.lock().unwrap().clone())
                }
                _ => Err(cb_err()),
            }
        }

        async fn delete_object(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: crate::storage::ObjectVariant,
        ) -> Result<(), StorageError> {
            Err(cb_err())
        }

        async fn put_object(
            &self,
            _: &str,
            _: &str,
            t: crate::storage::StoredWrite<'_>,
            body: crate::storage::PutBody<'_>,
            m: &FileMetadata,
        ) -> Result<(), StorageError> {
            if let crate::storage::StoredWrite::Delta { .. } = t {
                return Err(cb_err());
            }
            if let crate::storage::PutBody::File { path, .. } = body {
                let free = self.probe_spool.as_ref().map_or(0, |s| s.free_mib());
                self.file_puts
                    .lock()
                    .unwrap()
                    .push((path.to_path_buf(), free));
                return Ok(());
            }
            let data = body.into_bytes().await?;
            *self.bytes.lock().unwrap() = data.to_vec();
            *self.metadata.lock().unwrap() = Some(m.clone());
            Ok(())
        }

        async fn head_object(
            &self,
            _: &str,
            _: &str,
            o: crate::storage::StoredObject<'_>,
        ) -> Result<FileMetadata, StorageError> {
            if !matches!(o, crate::storage::StoredObject::Passthrough(_)) {
                return Err(cb_err());
            }
            if self
                .fail_meta_reads
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return Err(StorageError::Throttled("transient".into()));
            }
            self.metadata.lock().unwrap().clone().ok_or_else(cb_err)
        }

        async fn has_reference(&self, _: &str, _: &str) -> Result<bool, StorageError> {
            Ok(false)
        }
        async fn flush_pending(&self) -> Result<(), StorageError> {
            Ok(())
        }
        async fn scan_deltaspace(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Vec<FileMetadata>, StorageError> {
            Err(cb_err())
        }
        async fn list_deltaspaces(&self, _: &str) -> Result<Vec<String>, StorageError> {
            Err(cb_err())
        }
        async fn list_reference_prefixes(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Vec<String>, StorageError> {
            Err(cb_err())
        }
        async fn total_size(&self, _: Option<&str>) -> Result<u64, StorageError> {
            Err(cb_err())
        }
        async fn put_directory_marker(&self, _: &str, _: &str) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn bulk_list_objects(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
            Err(cb_err())
        }
        async fn enrich_list_metadata(
            &self,
            _: &str,
            o: Vec<(String, FileMetadata)>,
        ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
            Ok(o)
        }
    }

    /// A spy wrapper with `cfg` that holds `test.bin` of `size` bytes,
    /// sealed under `writer` (the key `kid-w`), and its plaintext.
    async fn counting(
        cfg: EncryptionConfig,
        writer: EncryptionKey,
        size: usize,
    ) -> (EncryptingBackend<CountingBackend>, Vec<u8>) {
        let data = body_of(size);
        let w = EncryptingBackend::new(CountingBackend::new(), shared(keyed(writer, "kid-w")));
        w.put_passthrough_chunked(
            "b",
            "p",
            "test.bin",
            &[Bytes::from(data.clone())],
            &pmeta("test.bin", &data),
        )
        .await
        .unwrap();
        let inner = std::sync::Arc::into_inner(w.inner).expect("the only owner");
        (EncryptingBackend::new(inner, shared(cfg)), data)
    }

    /// B3: a range in frames 7-8 of a 10-frame object is ONE ranged read of
    /// exactly those two frames (the response carries the metadata, so no
    /// HEAD and no header fetch), never a read from the file start.
    #[tokio::test]
    async fn a_range_read_is_one_read_of_the_covering_frames() {
        let (w, data) = counting(keyed(key(1), "kid-1"), key(1), 10 * F).await;
        let (start, end) = (7 * F as u64 + 100, 8 * F as u64 + 500);
        let crate::storage::RangeRead { stream, len, .. } = w
            .open_passthrough_range("b", "p", "test.bin", start, end)
            .await
            .unwrap();
        assert_eq!(len, end - start + 1);
        assert_eq!(
            collect(stream).await.unwrap(),
            &data[start as usize..=end as usize]
        );
        let ranges = w.inner.ranges_requested.lock().unwrap().clone();
        assert_eq!(
            ranges,
            [(
                v2::wire_offset_of_frame(7),
                v2::wire_offset_of_frame(8) + v2::FRAME_WIRE_LEN as u64 - 1
            )]
        );
    }

    /// "The last 100 bytes of a large object" reads the last frame only.
    #[tokio::test]
    async fn the_last_bytes_of_a_large_object_are_a_bounded_read() {
        let size = 64 * F;
        let (w, data) = counting(keyed(key(1), "kid-1"), key(1), size).await;
        let (start, end) = (size as u64 - 100, size as u64 - 1);
        let crate::storage::RangeRead { stream, len, .. } = w
            .open_passthrough_range("b", "p", "test.bin", start, end)
            .await
            .unwrap();
        assert_eq!(len, 100);
        assert_eq!(collect(stream).await.unwrap(), &data[start as usize..]);
        let ranges = w.inner.ranges_requested.lock().unwrap().clone();
        assert_eq!(ranges.len(), 1, "{ranges:?}");
        assert_eq!(ranges[0].0, v2::wire_offset_of_frame(63));
    }

    /// The configs whose reads may serve plaintext: a bucket without a key,
    /// and an encrypting one with `plaintext_reads: serve`.
    fn plaintext_serving() -> [(&'static str, EncryptionConfig); 2] {
        [
            ("no key", EncryptionConfig::default()),
            (
                "serve",
                EncryptionConfig {
                    serve_plaintext: true,
                    ..keyed(key(1), "kid-1")
                },
            ),
        ]
    }

    /// Strip the markers (and the seal) of the stored object, as a backup
    /// that drops xattrs or a copy without metadata does, and set its 4th
    /// byte (the format digit) to `digit`.
    fn strip(w: &EncryptingBackend<CountingBackend>, digit: u8) {
        let mut m = w.inner.metadata.lock().unwrap().clone().unwrap();
        strip_encryption_markers(&mut m.user_metadata);
        *w.inner.metadata.lock().unwrap() = Some(m);
        w.inner.bytes.lock().unwrap()[3] = digit;
    }

    /// A sealed body of the wrong stored size (cut short, or with bytes
    /// appended) fails when it is opened, before the first byte goes out:
    /// a full GET never sends a 200 and then breaks the stream.
    #[tokio::test]
    async fn a_sealed_body_of_the_wrong_size_fails_before_the_first_byte() {
        for change in [-1i64, -(F as i64), 1] {
            let (w, _) = counting(keyed(key(1), "kid-1"), key(1), 3 * F).await;
            {
                let mut bytes = w.inner.bytes.lock().unwrap();
                let len = (bytes.len() as i64 + change) as usize;
                bytes.resize(len, 0);
            }
            let res = w
                .open_object("b", "p", StoredObject::Passthrough("test.bin"))
                .await;
            assert!(
                matches!(res, Err(StorageError::Encryption(_))),
                "stored size changed by {change}: opened"
            );
        }
    }

    /// B9: a `DGE<digit>` body whose metadata lost its markers is refused,
    /// never served as plaintext, whatever the format digit.
    #[tokio::test]
    async fn a_dge_body_without_markers_is_refused() {
        for digit in *b"219" {
            for (label, cfg) in plaintext_serving() {
                let (w, _) = counting(cfg, key(1), 4096).await;
                strip(&w, digit);
                let err = w
                    .get_passthrough("b", "p", "test.bin")
                    .await
                    .expect_err("ciphertext served as plaintext");
                assert!(
                    err.to_string().contains("stripped"),
                    "{label} DGE{}: {err}",
                    digit as char
                );
            }
        }
        // DGE and a non-digit is ordinary plaintext.
        let (w, _) = counting(EncryptionConfig::default(), key(1), 4096).await;
        strip(&w, b'x');
        let stored = w.inner.bytes.lock().unwrap().clone();
        assert_eq!(
            w.get_passthrough("b", "p", "test.bin").await.unwrap(),
            stored
        );
    }

    /// The range path refuses a stripped body too. On a backend with a key
    /// it does so at every offset: under `serve` the first bytes are read
    /// as well, and without `serve` an unmarked object is refused anyway.
    /// On a backend without any key only a range from byte 0 sees the
    /// magic: nothing there can decrypt it, and a second read per range is
    /// not worth it for bytes that carry no plaintext.
    #[tokio::test]
    async fn a_dge_body_without_markers_is_refused_on_range_reads() {
        let mut cases: Vec<(&str, EncryptionConfig, u64)> =
            vec![("no key", EncryptionConfig::default(), 0)];
        for start in [0, 100, 70_000] {
            cases.push(("refuse", keyed(key(1), "kid-1"), start));
            let [_, (_, serve)] = plaintext_serving();
            cases.push(("serve", serve, start));
        }
        for (label, cfg, start) in cases {
            let (w, _) = counting(cfg, key(1), 2 * F).await;
            strip(&w, b'2');
            let res = read_range(&w, "b", "p", "test.bin", start, start + 50).await;
            assert!(res.is_err(), "{label} at {start}: ciphertext served");
        }
        // A plaintext object is served by range where plaintext may be.
        for (label, cfg) in plaintext_serving() {
            let w = EncryptingBackend::new(CountingBackend::new(), shared(cfg));
            let data = body_of(4096);
            w.inner
                .put_passthrough("b", "p", "t", Bytes::from(data.clone()), &pmeta("t", &data))
                .await
                .unwrap();
            let got = read_range(&w, "b", "p", "t", 100, 150).await.unwrap();
            assert_eq!(got, data[100..=150], "{label}");
        }
    }

    /// Production bodies are `unfold` streams (s3_body_to_stream), which
    /// panic when polled after they end. An object shorter than the magic
    /// ends inside the sniff; the sniff must not poll it again.
    #[tokio::test]
    async fn dge_sniff_never_polls_an_ended_stream() {
        let body = |data: &'static [u8]| -> ByteStream {
            Box::pin(futures::stream::unfold(Some(data), |d| async move {
                d.map(|d| (Ok::<_, StorageError>(Bytes::from_static(d)), None))
            }))
        };
        for data in [&b"v0"[..], b"abc", b"abcd", b"abcdefg"] {
            let out = collect(sniff_dge_magic(body(data))).await.unwrap();
            assert_eq!(out, data.to_vec());
        }
    }

    /// The sniff sees the magic even when the backend emits it split over
    /// chunks shorter than 4 bytes; other bytes pass through unchanged.
    #[tokio::test]
    async fn dge_sniff_joins_short_leading_chunks() {
        let split = |parts: &[&'static [u8]]| -> ByteStream {
            let items: Vec<Result<Bytes, StorageError>> =
                parts.iter().map(|p| Ok(Bytes::from_static(p))).collect();
            Box::pin(futures::stream::iter(items))
        };
        for magic in [&[&b"DG"[..], b"E", b"2rest"][..], &[b"D", b"GE7", b"x"]] {
            assert!(
                collect(sniff_dge_magic(split(magic))).await.is_err(),
                "split magic must be caught"
            );
        }
        let ok = collect(sniff_dge_magic(split(&[b"ab", b"c", b"defg"])))
            .await
            .unwrap();
        assert_eq!(ok, b"abcdefg".to_vec());
        let not_digit = collect(sniff_dge_magic(split(&[b"DGE", b"x"])))
            .await
            .unwrap();
        assert_eq!(not_digit, b"DGEx".to_vec());
        let short = collect(sniff_dge_magic(split(&[b"DG"]))).await.unwrap();
        assert_eq!(short, b"DG".to_vec(), "a 2-byte object is not ciphertext");
        assert!(collect(sniff_dge_magic(split(&[])))
            .await
            .unwrap()
            .is_empty());
    }

    /// Tier 4: a metadata-only rewrite fails CLOSED when it cannot read the
    /// stored seal; it never writes the caller's metadata without it.
    #[tokio::test]
    async fn put_passthrough_metadata_fails_closed_without_the_stored_seal() {
        let (w, _) = counting(keyed(key(1), "kid-1"), key(1), 11).await;
        let stored = w.inner.metadata.lock().unwrap().clone().unwrap();
        w.inner
            .fail_meta_reads
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let rewrite = pmeta("test.bin", &body_of(11));
        assert!(w
            .put_passthrough_metadata("b", "p", "test.bin", &rewrite, &RewriteBasis::none())
            .await
            .is_err());
        let now = w.inner.metadata.lock().unwrap().clone().unwrap();
        assert_eq!(
            now.user_metadata.get(seal::SEALED_KEY),
            stored.user_metadata.get(seal::SEALED_KEY),
            "the stored seal must survive"
        );
    }

    /// A metadata rewrite re-seals under the stored object's own keys: the
    /// body stays readable, the new client metadata is covered by the new
    /// seal. With or without an earlier read as its basis.
    #[tokio::test]
    async fn a_metadata_rewrite_reseals_the_stored_object() {
        let r = fs(keyed(key(1), "kid-1")).await;
        let data = body_of(3 * F + 5);
        r.put("p", "a.bin", &data).await;
        let first =
            r.w.inner
                .get_passthrough_metadata("b", "p", "a.bin")
                .await
                .unwrap();
        for (round, via_basis) in [(1, false), (2, true)] {
            let (shown, basis) = if via_basis {
                r.w.get_passthrough_metadata_for_rewrite("b", "p", "a.bin")
                    .await
                    .unwrap()
            } else {
                let shown =
                    r.w.get_passthrough_metadata("b", "p", "a.bin")
                        .await
                        .unwrap();
                (shown, RewriteBasis::none())
            };
            let mut next = shown.clone();
            next.user_metadata.insert("round".into(), round.to_string());
            next.content_type = Some("application/zip".into());
            r.w.put_passthrough_metadata("b", "p", "a.bin", &next, &basis)
                .await
                .unwrap();
            let raw =
                r.w.inner
                    .get_passthrough_metadata("b", "p", "a.bin")
                    .await
                    .unwrap();
            assert_ne!(
                raw.user_metadata[seal::SEALED_KEY],
                first.user_metadata[seal::SEALED_KEY]
            );
            let now =
                r.w.get_passthrough_metadata("b", "p", "a.bin")
                    .await
                    .unwrap();
            assert_eq!(now.user_metadata["round"], round.to_string());
            assert_eq!(now.md5, md5_hex(&data));
            assert_eq!(r.w.get_passthrough("b", "p", "a.bin").await.unwrap(), data);
        }
        // A rewrite that names another body size is refused.
        let mut wrong =
            r.w.get_passthrough_metadata("b", "p", "a.bin")
                .await
                .unwrap();
        wrong.file_size += 1;
        assert!(r
            .w
            .put_passthrough_metadata("b", "p", "a.bin", &wrong, &RewriteBasis::none())
            .await
            .is_err());
    }

    /// A stored object that carries a marker but no seal is not re-sealed
    /// (its bytes were never authenticated): the rewrite is refused.
    #[tokio::test]
    async fn a_metadata_rewrite_refuses_a_marked_but_unsealed_object() {
        let r = fs(keyed(key(1), "kid-1")).await;
        r.put("p", "a.bin", b"secret").await;
        let mut raw =
            r.w.inner
                .get_passthrough_metadata("b", "p", "a.bin")
                .await
                .unwrap();
        raw.user_metadata.remove(seal::SEALED_KEY);
        r.w.inner
            .put_passthrough_metadata("b", "p", "a.bin", &raw, &RewriteBasis::none())
            .await
            .unwrap();
        let err =
            r.w.put_passthrough_metadata(
                "b",
                "p",
                "a.bin",
                &pmeta("a.bin", b"secret"),
                &RewriteBasis::none(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("encryption marker"), "{err}");
    }

    /// A plaintext object stays plaintext on a metadata rewrite, also in an
    /// encrypting bucket, and a caller's markers are dropped.
    #[tokio::test]
    async fn a_plaintext_object_stays_plaintext_on_a_metadata_rewrite() {
        let r = fs(EncryptionConfig {
            serve_plaintext: true,
            ..keyed(key(1), "kid-1")
        })
        .await;
        r.w.inner
            .put_passthrough(
                "b",
                "p",
                "plain",
                Bytes::from_static(b"plain"),
                &pmeta("plain", b"plain"),
            )
            .await
            .unwrap();
        let mut m = pmeta("plain", b"plain");
        m.user_metadata
            .insert(ENCRYPTION_MARKER_KEY.into(), "v2".into());
        m.user_metadata
            .insert(seal::SEALED_KEY.into(), "2.forged".into());
        m.user_metadata.insert("team".into(), "ci".into());
        r.w.put_passthrough_metadata("b", "p", "plain", &m, &RewriteBasis::none())
            .await
            .unwrap();
        let raw =
            r.w.inner
                .get_passthrough_metadata("b", "p", "plain")
                .await
                .unwrap();
        assert!(!has_any_marker(&raw), "{:?}", raw.user_metadata);
        assert_eq!(raw.user_metadata["team"], "ci");
        assert_eq!(
            r.w.get_passthrough("b", "p", "plain").await.unwrap(),
            b"plain"
        );
    }

    // ── Keys ──

    /// A read finds the key by the fingerprint in the seal, under any id:
    /// the presented `dg-encryption-key-id` is the id that THIS config
    /// gives the key, never a stored claim.
    #[tokio::test]
    async fn reads_pick_the_key_by_the_fingerprint_in_the_seal() {
        let r = fs(keyed(key(0xa1), "kid-a")).await;
        r.put("p", "x", b"under a").await;
        let shown = r.w.get_passthrough_metadata("b", "p", "x").await.unwrap();
        assert_eq!(
            shown.user_metadata[ENCRYPTION_MARKER_KEY],
            PRESENTED_MARKER_VALUE
        );
        assert_eq!(shown.user_metadata[ENCRYPTION_KEY_ID_KEY], "kid-a");
        let raw =
            r.w.inner
                .get_passthrough_metadata("b", "p", "x")
                .await
                .unwrap();
        assert!(!raw.user_metadata.contains_key(ENCRYPTION_KEY_ID_KEY));

        let rotated = EncryptionConfig {
            legacy_key: Some(key(0xa1)),
            legacy_key_id: Some("kid-old".into()),
            ..keyed(key(0xb2), "kid-b")
        };
        let renamed = keyed(key(0xa1), "renamed");
        let mut ring = keyed(key(0xb2), "kid-b");
        ring.keyring.insert("from-ring".into(), key(0xa1));
        for (cfg, id) in [
            (rotated, "kid-old"),
            (renamed, "renamed"),
            (ring, "from-ring"),
        ] {
            r.cfg.store(Arc::new(cfg));
            assert_eq!(
                r.w.get_passthrough("b", "p", "x").await.unwrap(),
                b"under a"
            );
            let shown = r.w.get_passthrough_metadata("b", "p", "x").await.unwrap();
            assert_eq!(shown.user_metadata[ENCRYPTION_KEY_ID_KEY], id);
            let got = read_range(&r.w, "b", "p", "x", 2, 4).await.unwrap();
            assert_eq!(got, b"der");
        }
    }

    /// An object under a key the backend does not have fails with a
    /// message that names the fingerprint and the fix, on every read path.
    #[tokio::test]
    async fn a_read_under_an_unknown_key_names_the_fingerprint() {
        let r = fs(keyed(key(0xa1), "kid-a")).await;
        r.put("p", "x", b"under a").await;
        r.cfg.store(Arc::new(keyed(key(0xb2), "kid-b")));
        let fp = hex::encode(v2::SiteKey::new(&key(0xa1)).fingerprint());
        let errors = [
            r.w.get_passthrough("b", "p", "x").await.unwrap_err(),
            r.w.get_passthrough_metadata("b", "p", "x")
                .await
                .unwrap_err(),
            read_range(&r.w, "b", "p", "x", 0, 3).await.unwrap_err(),
        ];
        for err in errors {
            let msg = err.to_string();
            assert!(msg.contains(&fp) && msg.contains("legacy_key"), "{msg}");
        }
        // A backend without any key cannot read it either.
        r.cfg.store(Arc::new(EncryptionConfig::default()));
        assert!(r.w.get_passthrough("b", "p", "x").await.is_err());
    }

    /// The decrypt-only shim (proxy AES → native SSE): no primary key,
    /// writes pass through as plaintext, the legacy key reads the history.
    #[tokio::test]
    async fn the_shim_reads_legacy_objects_and_writes_plaintext() {
        let r = fs(keyed(key(0xa1), "id-K1")).await;
        r.put("p", "old", b"history").await;
        r.cfg.store(Arc::new(EncryptionConfig {
            write_mode: WriteMode::PassThrough,
            legacy_key: Some(key(0xa1)),
            legacy_key_id: Some("id-K1".into()),
            ..Default::default()
        }));
        assert_eq!(
            r.w.get_passthrough("b", "p", "old").await.unwrap(),
            b"history"
        );
        r.put("p", "new", b"plain now").await;
        assert_eq!(
            r.w.inner.get_passthrough("b", "p", "new").await.unwrap(),
            b"plain now"
        );
        let raw =
            r.w.inner
                .get_passthrough_metadata("b", "p", "new")
                .await
                .unwrap();
        assert!(!has_any_marker(&raw));
    }

    // ── Write modes ──

    /// Every write path honours `WriteMode::PassThrough` even with a key
    /// (the proxy-AES → native-SSE transition): it stores plaintext.
    #[tokio::test]
    async fn passthrough_mode_writes_plaintext_on_every_path() {
        let r = fs(EncryptionConfig {
            write_mode: WriteMode::PassThrough,
            ..keyed(key(1), "current")
        })
        .await;
        let sd = r.spool();
        let data = b"chunked plaintext".to_vec();
        let meta = pmeta("x", &data);
        let src = r.dir.path().join("src");
        std::fs::write(&src, &data).unwrap();
        let w = &r.w;
        w.put_passthrough("b", "p", "a", Bytes::from(data.clone()), &meta)
            .await
            .unwrap();
        w.put_passthrough_chunked("b", "p", "c", &[Bytes::from(data.clone())], &meta)
            .await
            .unwrap();
        w.put_passthrough_file(
            "b",
            "p",
            "f",
            &src,
            &meta,
            SpoolBudget::new(&sd, None, None),
        )
        .await
        .unwrap();
        w.put_passthrough_parts(
            "b",
            "p",
            "m",
            std::slice::from_ref(&src),
            &meta,
            SpoolBudget::new(&sd, None, None),
        )
        .await
        .unwrap();
        for name in ["a", "c", "f", "m"] {
            assert_eq!(
                w.inner.get_passthrough("b", "p", name).await.unwrap(),
                data,
                "{name}"
            );
            let raw = w
                .inner
                .get_passthrough_metadata("b", "p", name)
                .await
                .unwrap();
            assert!(!has_any_marker(&raw), "{name}");
            assert_eq!(w.get_passthrough("b", "p", name).await.unwrap(), data);
        }
        assert_eq!(w.file_put_spool_bytes("b", 1 << 20, true).await, 0);
    }

    #[test]
    fn encrypt_is_the_default_write_mode() {
        assert_eq!(WriteMode::default(), WriteMode::Encrypt);
        assert_eq!(keyed(key(1), "id").write_mode, WriteMode::Encrypt);
    }

    /// `WriteMode::Refuse` (a backend set to encrypt, without a key): every
    /// write fails, none stores plaintext.
    #[tokio::test]
    async fn refuse_mode_refuses_every_write() {
        let r = fs(EncryptionConfig {
            write_mode: WriteMode::Refuse,
            ..Default::default()
        })
        .await;
        let sd = r.spool();
        let src = r.dir.path().join("src");
        std::fs::write(&src, b"x").unwrap();
        let (w, m, x) = (&r.w, pmeta("x", b"x"), Bytes::from_static(b"x"));
        let results = [
            w.put_passthrough("b", "p", "x", x.clone(), &m).await,
            w.put_passthrough_chunked("b", "p", "x", std::slice::from_ref(&x), &m)
                .await,
            w.put_passthrough_file("b", "p", "x", &src, &m, SpoolBudget::new(&sd, None, None))
                .await,
            w.put_delta(
                "b",
                "p",
                "x",
                x.clone(),
                &dmeta("x", b"x"),
                RefWriteProof::for_tests(),
            )
            .await,
            w.put_reference("b", "p", x, &rmeta(b"x"), RefWriteProof::for_tests())
                .await,
            w.create_multipart_upload("b", "p", "x", &m)
                .await
                .map(|_| ()),
        ];
        for (i, res) in results.into_iter().enumerate() {
            let err = res.expect_err("a write stored plaintext");
            assert!(err.to_string().contains("refuses the write"), "{i}: {err}");
        }
        assert!(w.inner.get_passthrough("b", "p", "x").await.is_err());
    }

    /// S15: client metadata that names the wrapper's markers never decides
    /// how the body is stored or read: a client `dg-encrypted`,
    /// `dg-encryption-key-id` or `dg-sealed` (a sync tool that copies GET
    /// metadata into the next PUT) is dropped on every write path.
    #[tokio::test]
    async fn client_supplied_markers_never_reach_storage() {
        let forge = |mut m: FileMetadata| {
            m.user_metadata
                .insert(ENCRYPTION_MARKER_KEY.into(), seal::DISK_MARKER.into());
            m.user_metadata
                .insert(ENCRYPTION_KEY_ID_KEY.into(), "forged-kid".into());
            m.user_metadata
                .insert(seal::SEALED_KEY.into(), "2.forged".into());
            m.user_metadata.insert("team".into(), "ci".into());
            m
        };
        for (mode, cfg) in [
            ("no-key", EncryptionConfig::default()),
            ("encrypt", keyed(key(1), "kid-1")),
        ] {
            let r = fs(cfg).await;
            let sd = r.spool();
            let body = b"client body".to_vec();
            let meta = forge(pmeta("x", &body));
            let src = r.dir.path().join("src.bin");
            std::fs::write(&src, &body).unwrap();
            let w = &r.w;
            let b = || Bytes::from(body.clone());
            w.put_passthrough("b", "p", "a", b(), &meta).await.unwrap();
            w.put_passthrough_file(
                "b",
                "p",
                "c",
                &src,
                &meta,
                SpoolBudget::new(&sd, None, None),
            )
            .await
            .unwrap();
            w.put_passthrough_chunked("b", "p", "d", &[b()], &meta)
                .await
                .unwrap();
            w.put_passthrough_parts(
                "b",
                "p",
                "e",
                std::slice::from_ref(&src),
                &meta,
                SpoolBudget::new(&sd, None, None),
            )
            .await
            .unwrap();
            w.put_delta(
                "b",
                "p",
                "f",
                b(),
                &forge(dmeta("f", &body)),
                RefWriteProof::for_tests(),
            )
            .await
            .unwrap();
            w.put_reference(
                "b",
                "p",
                b(),
                &forge(rmeta(&body)),
                RefWriteProof::for_tests(),
            )
            .await
            .unwrap();
            let mut raws = Vec::new();
            for f in ["a", "c", "d", "e"] {
                assert_eq!(
                    w.get_passthrough("b", "p", f).await.unwrap(),
                    body,
                    "{mode}: {f}"
                );
                let shown = w.get_passthrough_metadata("b", "p", f).await.unwrap();
                assert_ne!(
                    shown
                        .user_metadata
                        .get(ENCRYPTION_KEY_ID_KEY)
                        .map(String::as_str),
                    Some("forged-kid"),
                    "{mode}: {f}"
                );
                assert_eq!(shown.user_metadata["team"], "ci");
                raws.push(w.inner.get_passthrough_metadata("b", "p", f).await.unwrap());
            }
            raws.push(w.inner.get_delta_metadata("b", "p", "f").await.unwrap());
            raws.push(w.inner.get_reference_metadata("b", "p").await.unwrap());
            for raw in &raws {
                assert!(
                    !raw.user_metadata.contains_key(ENCRYPTION_KEY_ID_KEY),
                    "{mode}"
                );
                assert_ne!(
                    raw.user_metadata.get(seal::SEALED_KEY).map(String::as_str),
                    Some("2.forged"),
                    "{mode}"
                );
                assert_eq!(has_seal(raw), mode == "encrypt", "{mode}");
            }
            assert_eq!(w.get_delta("b", "p", "f").await.unwrap(), body, "{mode}");
            assert_eq!(w.get_reference("b", "p").await.unwrap(), body, "{mode}");

            // A metadata-only rewrite must not plant a marker either.
            let mut rewrite = w.get_passthrough_metadata("b", "p", "a").await.unwrap();
            rewrite
                .user_metadata
                .insert(seal::SEALED_KEY.into(), "2.forged".into());
            w.put_passthrough_metadata("b", "p", "a", &rewrite, &RewriteBasis::none())
                .await
                .unwrap();
            assert_eq!(
                w.get_passthrough("b", "p", "a").await.unwrap(),
                body,
                "{mode}"
            );
        }
    }

    // ── Fail closed ──

    /// Issue #120: an object without a seal in a bucket that encrypts is
    /// refused on every read (a writer with backend access could have
    /// planted it), unless the config says `plaintext_reads: serve`.
    #[tokio::test]
    async fn an_unmarked_object_in_an_encrypting_bucket_is_refused_unless_serve() {
        let r = fs(keyed(key(1), "kid-1")).await;
        let data = body_of(F + 10);
        let w = &r.w;
        w.inner
            .put_passthrough(
                "b",
                "p",
                "plain",
                Bytes::from(data.clone()),
                &pmeta("plain", &data),
            )
            .await
            .unwrap();
        w.inner
            .put_delta(
                "b",
                "p",
                "d",
                Bytes::from(data.clone()),
                &dmeta("d", &data),
                RefWriteProof::for_tests(),
            )
            .await
            .unwrap();
        w.inner
            .put_reference(
                "b",
                "p",
                Bytes::from(data.clone()),
                &rmeta(&data),
                RefWriteProof::for_tests(),
            )
            .await
            .unwrap();
        async fn reads(
            w: &EncryptingBackend<crate::storage::FilesystemBackend>,
        ) -> Vec<Result<(), StorageError>> {
            vec![
                w.get_passthrough("b", "p", "plain").await.map(|_| ()),
                w.get_passthrough_metadata("b", "p", "plain")
                    .await
                    .map(|_| ()),
                read_range(w, "b", "p", "plain", 0, 9).await.map(|_| ()),
                read_range(w, "b", "p", "plain", F as u64, F as u64 + 5)
                    .await
                    .map(|_| ()),
                w.get_delta("b", "p", "d").await.map(|_| ()),
                w.get_delta_metadata("b", "p", "d").await.map(|_| ()),
                w.get_reference("b", "p").await.map(|_| ()),
                w.get_reference_metadata("b", "p").await.map(|_| ()),
                w.open_reference_source("b", "p").await.map(|_| ()),
            ]
        }
        for (i, res) in reads(w).await.into_iter().enumerate() {
            let err = res.expect_err("plaintext served in an encrypting bucket");
            assert!(err.to_string().contains("plaintext_reads"), "{i}: {err}");
        }
        r.cfg.store(Arc::new(EncryptionConfig {
            serve_plaintext: true,
            ..keyed(key(1), "kid-1")
        }));
        for (i, res) in reads(w).await.into_iter().enumerate() {
            res.unwrap_or_else(|e| panic!("{i}: {e}"));
        }
        assert_eq!(w.get_passthrough("b", "p", "plain").await.unwrap(), data);
        // A bucket that refuses writes (no key) refuses plaintext reads too.
        r.cfg.store(Arc::new(EncryptionConfig {
            write_mode: WriteMode::Refuse,
            ..Default::default()
        }));
        assert!(w.get_passthrough("b", "p", "plain").await.is_err());
    }

    /// A folder marker (a zero-byte key ending in `/`) is never sealed and
    /// always reads; any other unsealed object follows the bucket's rule.
    #[test]
    fn a_folder_marker_always_reads() {
        let w = EncryptingBackend::new(CountingBackend::new(), shared(keyed(key(1), "kid-1")));
        let raw = |size| {
            FileMetadata::fallback(
                "dir/".into(),
                size,
                String::new(),
                chrono::Utc::now(),
                None,
                crate::types::StorageInfo::Passthrough,
            )
        };
        let read = |name, size| {
            w.present_raw("b", "p", StoredObject::Passthrough(name), &raw(size))
                .is_ok()
        };
        assert!(read("dir/", 0));
        assert!(read("", 0));
        assert!(!read("dir/", 1), "a folder marker has no bytes");
        assert!(!read("x", 0), "an empty object is not a folder marker");
        let mut marked = raw(0);
        marked
            .user_metadata
            .insert(ENCRYPTION_MARKER_KEY.into(), seal::DISK_MARKER.into());
        assert!(
            w.present_raw("b", "p", StoredObject::Passthrough("dir/"), &marked)
                .is_err(),
            "a marker without a seal is refused, also on a folder"
        );
    }

    /// The seal binds the location: a sealed object copied at the storage
    /// level (body and metadata) to another key, prefix, bucket or variant
    /// fails closed.
    #[tokio::test]
    async fn a_sealed_object_moved_on_the_backend_fails() {
        let r = fs(keyed(key(1), "kid-1")).await;
        let w = &r.w;
        r.put("p", "a.bin", b"sealed body").await;
        let body = Bytes::from(w.inner.get_passthrough("b", "p", "a.bin").await.unwrap());
        let raw = w
            .inner
            .get_passthrough_metadata("b", "p", "a.bin")
            .await
            .unwrap();
        for (bucket, prefix, name) in [
            ("b", "p", "c.bin"),
            ("b2", "p", "a.bin"),
            ("b", "q", "a.bin"),
        ] {
            w.inner
                .put_passthrough(bucket, prefix, name, body.clone(), &raw)
                .await
                .unwrap();
            let at = format!("{bucket}/{prefix}/{name}");
            assert!(
                w.get_passthrough(bucket, prefix, name).await.is_err(),
                "{at}"
            );
            assert!(
                w.get_passthrough_metadata(bucket, prefix, name)
                    .await
                    .is_err(),
                "{at}"
            );
            assert!(
                read_range(w, bucket, prefix, name, 0, 3).await.is_err(),
                "{at}"
            );
        }
        assert_eq!(
            w.get_passthrough("b", "p", "a.bin").await.unwrap(),
            b"sealed body"
        );

        // A delta's stored form moved to another delta key.
        let delta = b"delta body".to_vec();
        w.put_delta(
            "b",
            "p",
            "x",
            Bytes::from(delta.clone()),
            &dmeta("x", &delta),
            RefWriteProof::for_tests(),
        )
        .await
        .unwrap();
        let body = w.inner.get_delta("b", "p", "x").await.unwrap();
        let raw = w.inner.get_delta_metadata("b", "p", "x").await.unwrap();
        w.inner
            .put_delta(
                "b",
                "p",
                "y",
                Bytes::from(body),
                &raw,
                RefWriteProof::for_tests(),
            )
            .await
            .unwrap();
        assert!(w.get_delta("b", "p", "y").await.is_err());
        assert_eq!(w.get_delta("b", "p", "x").await.unwrap(), delta);
    }

    /// The seal covers the clear headers: a backend writer that edits the
    /// content type or the client's user metadata makes the object fail.
    #[tokio::test]
    async fn an_edited_clear_header_fails() {
        let r = fs(keyed(key(1), "kid-1")).await;
        let w = &r.w;
        let mut meta = pmeta("a.bin", b"body");
        meta.content_type = Some("application/zip".into());
        meta.user_metadata.insert("build".into(), "42".into());
        w.put_passthrough("b", "p", "a.bin", Bytes::from_static(b"body"), &meta)
            .await
            .unwrap();
        let raw = w
            .inner
            .get_passthrough_metadata("b", "p", "a.bin")
            .await
            .unwrap();
        let edits: [fn(&mut FileMetadata); 4] = [
            |m| m.content_type = Some("text/html".into()),
            |m| {
                m.user_metadata.insert("build".into(), "43".into());
            },
            |m| {
                m.user_metadata.remove("build");
            },
            |m| {
                m.user_metadata.insert("extra".into(), "1".into());
            },
        ];
        for (i, edit) in edits.into_iter().enumerate() {
            let mut edited = raw.clone();
            edit(&mut edited);
            w.inner
                .put_passthrough_metadata("b", "p", "a.bin", &edited, &RewriteBasis::none())
                .await
                .unwrap();
            let err = w.get_passthrough("b", "p", "a.bin").await.unwrap_err();
            assert!(err.to_string().contains("clear metadata"), "{i}: {err}");
            assert!(w.get_passthrough_metadata("b", "p", "a.bin").await.is_err());
        }
        w.inner
            .put_passthrough_metadata("b", "p", "a.bin", &raw, &RewriteBasis::none())
            .await
            .unwrap();
        assert_eq!(w.get_passthrough("b", "p", "a.bin").await.unwrap(), b"body");
    }

    // ── copy_within ──

    /// A storage-level copy under the same key re-seals for the destination
    /// (the seal binds the location) and keeps the body. A stale pin is
    /// `PreconditionFailed`. A plaintext source into an encrypting bucket and
    /// a sealed source into a plaintext bucket are `Unsupported` (the caller
    /// copies through).
    #[tokio::test]
    async fn copy_within_reseals_for_the_destination() {
        use crate::storage::{CopyOutcome, ObjectAt, ObjectVariant};
        let mut cfg = keyed(key(1), "kid-1");
        cfg.buckets.insert("b2".into(), BucketKeys::default());
        let r = fs(cfg).await;
        let w = &r.w;
        let data = body_of(2 * F + 3);
        r.put("p", "a.bin", &data).await;
        let source = w.get_passthrough_metadata("b", "p", "a.bin").await.unwrap();
        let src = ObjectAt {
            bucket: "b",
            prefix: "p",
            filename: "a.bin",
        };
        let to = |bucket, filename| ObjectAt {
            bucket,
            prefix: "q",
            filename,
        };
        let mut dst_meta = source.clone();
        dst_meta.original_name = "c.bin".into();
        let copied = w
            .copy_within(
                ObjectVariant::Passthrough,
                src,
                to("b", "c.bin"),
                &source,
                &dst_meta,
            )
            .await
            .unwrap();
        assert_eq!(copied, CopyOutcome::Copied);
        assert_eq!(w.get_passthrough("b", "q", "c.bin").await.unwrap(), data);
        let raw_src = w
            .inner
            .get_passthrough_metadata("b", "p", "a.bin")
            .await
            .unwrap();
        let raw_dst = w
            .inner
            .get_passthrough_metadata("b", "q", "c.bin")
            .await
            .unwrap();
        assert_ne!(
            raw_src.user_metadata[seal::SEALED_KEY],
            raw_dst.user_metadata[seal::SEALED_KEY]
        );
        assert_eq!(
            w.get_passthrough_metadata("b", "q", "c.bin")
                .await
                .unwrap()
                .md5,
            md5_hex(&data)
        );

        let mut stale = source.clone();
        stale.created_at += chrono::Duration::microseconds(1);
        let changed = w
            .copy_within(
                ObjectVariant::Passthrough,
                src,
                to("b", "s.bin"),
                &stale,
                &dst_meta,
            )
            .await;
        assert!(
            matches!(changed, Err(StorageError::PreconditionFailed(_))),
            "{changed:?}"
        );
        let other_key = w
            .copy_within(
                ObjectVariant::Passthrough,
                src,
                to("b2", "c.bin"),
                &source,
                &dst_meta,
            )
            .await
            .unwrap();
        assert_eq!(other_key, CopyOutcome::Unsupported);
        w.inner
            .put_passthrough(
                "b",
                "p",
                "plain",
                Bytes::from_static(b"plain"),
                &pmeta("plain", b"plain"),
            )
            .await
            .unwrap();
        let plain = pmeta("plain", b"plain");
        let res = w
            .copy_within(
                ObjectVariant::Passthrough,
                ObjectAt {
                    filename: "plain",
                    ..src
                },
                to("b", "plain"),
                &plain,
                &plain,
            )
            .await
            .unwrap();
        assert_eq!(res, CopyOutcome::Unsupported);
    }

    /// A plaintext copy checks the caller's pin too: a source overwritten
    /// after the caller read it is `PreconditionFailed`, never a copy of the
    /// new body under the old body's facts.
    #[tokio::test]
    async fn a_plaintext_copy_with_a_stale_pin_fails() {
        use crate::storage::{ObjectAt, ObjectVariant};
        let r = fs(EncryptionConfig::default()).await;
        let w = &r.w;
        r.put("p", "x", b"old body").await;
        let pin = w.get_passthrough_metadata("b", "p", "x").await.unwrap();
        r.put("p", "x", b"NEW BODY, longer").await;
        let res = w
            .copy_within(
                ObjectVariant::Passthrough,
                ObjectAt {
                    bucket: "b",
                    prefix: "p",
                    filename: "x",
                },
                ObjectAt {
                    bucket: "b",
                    prefix: "p",
                    filename: "z",
                },
                &pin,
                &pmeta("z", b"old body"),
            )
            .await;
        assert!(
            matches!(res, Err(StorageError::PreconditionFailed(_))),
            "{res:?}"
        );
    }

    // ── Listings ──

    /// One object whose seal does not open (here: its key left the config)
    /// does not fail the listing or the scan of its folder. It is listed
    /// as stored and marked unopened; its neighbour opens; a read of it
    /// still fails.
    #[tokio::test]
    async fn a_listing_survives_an_entry_whose_seal_does_not_open() {
        let r = fs(keyed(key(0xa1), "kid-a")).await;
        r.put("d", "x.jpg", b"under key a").await;
        r.cfg.store(Arc::new(keyed(key(0xb2), "kid-b")));
        r.put("d", "y.jpg", b"under key b").await;

        let listed = r.w.bulk_list_objects("b", "d").await.unwrap();
        let scanned = r.w.scan_deltaspace("b", "d").await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(scanned.len(), 2);
        for meta in listed.iter().map(|(_, m)| m).chain(&scanned) {
            let opened = meta.original_name == "y.jpg";
            assert_eq!(!meta.is_unopened_seal(), opened, "{}", meta.original_name);
            assert!(!meta.user_metadata.contains_key(seal::SEALED_KEY));
        }
        assert!(r.w.get_passthrough("b", "d", "x.jpg").await.is_err());
        assert_eq!(
            r.w.get_passthrough("b", "d", "y.jpg").await.unwrap(),
            b"under key b"
        );
    }

    /// With a key, a listed entry's logical size and ETag come only from a
    /// listing token that opens under a key of this backend for exactly
    /// that stored object; a forged, foreign or mismatched token is ignored.
    #[tokio::test]
    async fn resolve_listed_sizes_applies_only_a_valid_listing_token() {
        let site = v2::SiteKey::new(&key(0xa1));
        let foreign = v2::SiteKey::new(&key(0xb2));
        let entry = |key: &str, stored: u64| {
            (
                key.to_string(),
                FileMetadata::fallback(
                    key.into(),
                    stored,
                    "e1".into(),
                    chrono::Utc::now(),
                    None,
                    crate::types::StorageInfo::Passthrough,
                ),
            )
        };
        let stored = v2::stored_len(1000).unwrap();
        let token = |site: &v2::SiteKey, key: &str, stored_size: u64, size: u64, parts: u16| {
            let lf = v2::ListingFacts {
                size,
                etag_digest: [3; 16],
                parts,
            };
            let at = v2::ListedAt {
                bucket: "b",
                stored_key: key,
                stored_etag: "e1",
                stored_size,
            };
            Some(crate::storage::ListedToken {
                token: site.listing_token(&at, &lf).unwrap(),
                stored_key: key.into(),
                stored_etag: "e1".into(),
                stored_size,
            })
        };
        let mut moved = token(&site, "p/good.bin", stored, 1000, 0);
        moved.as_mut().unwrap().stored_key = "p/moved.bin".into();
        let tokens = vec![
            token(&site, "p/good.bin", stored, 1000, 0),
            token(&foreign, "p/forged.bin", stored, 1000, 0),
            moved,
            token(&site, "p/size.bin", stored, 999, 0),
            token(&site, "p/multi.bin", stored, 1000, 3),
            None,
            None,
        ];
        let listing = || {
            vec![
                entry("p/good.bin", stored),
                entry("p/forged.bin", stored),
                entry("p/moved.bin", stored),
                entry("p/size.bin", stored),
                entry("p/multi.bin", stored),
                entry("p/none.bin", stored),
                entry("p/dir/", 0),
            ]
        };
        let w = EncryptingBackend::new(CountingBackend::new(), shared(keyed(key(0xa1), "kid-a")));
        *w.inner.tokens.lock().unwrap() = tokens.clone();
        let mut objects = listing();
        let sizes = w
            .resolve_listed_sizes("b", &mut objects, ResolveListed::default())
            .await;
        use ListedSize::{Cached, Listed, SizeOnly};
        // An entry without a valid token lists the plaintext size of its
        // ciphertext (the bucket serves no plaintext), with its stored ETag.
        assert_eq!(
            sizes,
            [Cached, SizeOnly, SizeOnly, SizeOnly, Cached, SizeOnly, Listed]
        );
        assert_eq!(objects[0].1.file_size, 1000);
        assert_eq!(
            objects[0].1.etag(),
            format!("\"{}\"", hex::encode([3u8; 16]))
        );
        assert_eq!(objects[1].1.etag(), "\"e1\"", "a forged token applied");
        assert_eq!(objects[3].1.file_size, 1000, "the token's size applied");
        assert_eq!(objects[5].1.file_size, 1000);
        assert_eq!(
            objects[4].1.etag(),
            format!("\"{}-3\"", hex::encode([3u8; 16]))
        );

        // A bucket that serves plaintext may hold a plaintext object of
        // that size: the stored size stands.
        let mut serving = keyed(key(0xa1), "kid-a");
        serving.serve_plaintext = true;
        let w = EncryptingBackend::new(CountingBackend::new(), shared(serving));
        let mut objects = listing();
        let sizes = w
            .resolve_listed_sizes("b", &mut objects, ResolveListed::default())
            .await;
        assert_eq!(sizes[5], ListedSize::StoredOnly);
        assert_eq!(objects[5].1.file_size, stored);

        // Without any key, the inner listing stands.
        let plain =
            EncryptingBackend::new(CountingBackend::new(), shared(EncryptionConfig::default()));
        *plain.inner.tokens.lock().unwrap() = tokens;
        let mut objects = listing();
        let sizes = plain
            .resolve_listed_sizes("b", &mut objects, ResolveListed::default())
            .await;
        assert!(sizes.iter().all(|s| *s == Listed), "{sizes:?}");
        assert_eq!(objects[0].1.file_size, stored);
    }

    // ── Spool budget ──

    fn test_spool(dir: &tempfile::TempDir) -> crate::deltaglider::spool::SpoolDir {
        crate::deltaglider::spool::SpoolDir::new(dir.path().join("spool"), 64 * 1024 * 1024)
            .unwrap()
    }

    fn unheld(sd: &crate::deltaglider::spool::SpoolDir) -> SpoolBudget<'_> {
        SpoolBudget::new(sd, None, None)
    }

    fn probe_wrapper(
        sd: &crate::deltaglider::spool::SpoolDir,
    ) -> EncryptingBackend<CountingBackend> {
        let mut inner = CountingBackend::new();
        inner.probe_spool = Some(sd.clone());
        EncryptingBackend::new(inner, shared(keyed(key(1), "kid-1")))
    }

    fn meta_of(len: usize) -> FileMetadata {
        FileMetadata::new_passthrough("o".into(), "0".repeat(64), "0".repeat(32), len as u64, None)
    }

    const MIB: usize = 1024 * 1024;

    /// The ciphertext temp file of an encrypted file PUT lives in the spool
    /// dir and holds spool budget while the inner backend reads it. It was
    /// a `NamedTempFile::new()` in the system temp dir, outside the budget,
    /// so N large encrypted PUTs could fill the disk.
    #[tokio::test]
    async fn encrypted_file_put_temp_is_inside_the_spool_budget() {
        let dir = tempfile::tempdir().unwrap();
        let sd = test_spool(&dir);
        let wrapper = probe_wrapper(&sd);
        let body = vec![7u8; 3 * MIB];
        let src = dir.path().join("src");
        std::fs::write(&src, &body).unwrap();

        wrapper
            .put_passthrough_file("b", "p", "o", &src, &meta_of(body.len()), unheld(&sd))
            .await
            .unwrap();
        let puts = wrapper.inner.file_puts.lock().unwrap().clone();
        assert_eq!(puts.len(), 1);
        assert_eq!(
            puts[0].0.parent(),
            Some(sd.dir()),
            "ciphertext temp must be in the spool dir"
        );
        assert!(
            puts[0].1 <= 64 - 4,
            "ciphertext temp must hold its budget (3 MiB body -> 4 MiB), free {} MiB",
            puts[0].1
        );
        assert_eq!(sd.free_mib(), 64, "budget released after the write");
    }

    /// Relayed parts are encrypted straight into ONE ciphertext file (no
    /// joined plaintext copy), which uses the engine's reservation when
    /// there is one, else free budget.
    #[tokio::test]
    async fn encrypted_parts_put_temp_is_inside_the_spool_budget() {
        let dir = tempfile::tempdir().unwrap();
        let sd = test_spool(&dir);
        let wrapper = probe_wrapper(&sd);
        let parts: Vec<std::path::PathBuf> = (0..3)
            .map(|i| {
                let p = dir.path().join(format!("part{i}"));
                std::fs::write(&p, vec![i as u8; MIB]).unwrap();
                p
            })
            .collect();

        let need = wrapper
            .file_put_spool_bytes("b", 3 * MIB as u64, true)
            .await;
        assert_eq!(
            need,
            v2::stored_len(3 * MIB as u64).unwrap(),
            "the ciphertext file only"
        );
        let reserved = sd.reserve_beside(0, need).await.unwrap();
        let free_after_reserve = sd.free_mib();
        wrapper
            .put_passthrough_parts(
                "b",
                "p",
                "o",
                &parts,
                &meta_of(3 * MIB),
                SpoolBudget::new(&sd, None, Some(&reserved)),
            )
            .await
            .unwrap();
        let puts = wrapper.inner.file_puts.lock().unwrap().clone();
        assert_eq!(puts[0].0.parent(), Some(sd.dir()));
        assert_eq!(
            puts[0].1, free_after_reserve,
            "the file uses the reservation, not more budget"
        );
        drop(reserved);
        assert_eq!(sd.free_mib(), 64);

        // Without a reservation: the wrapper takes free budget now.
        wrapper
            .put_passthrough_parts("b", "p", "o", &parts, &meta_of(3 * MIB), unheld(&sd))
            .await
            .unwrap();
        let puts = wrapper.inner.file_puts.lock().unwrap().clone();
        assert_eq!(puts[1].1, 64 - 4, "the ciphertext (3 MiB + overhead) only");
        assert_eq!(sd.free_mib(), 64);
    }

    /// The review2 rule: a storage write never waits for spool budget. It
    /// runs under the engine's deltaspace lock, and other ops hold budget
    /// while they wait for that lock. With the budget taken, the write fails
    /// retryably (503 SlowDown) at once.
    #[tokio::test]
    async fn encrypted_file_put_never_waits_for_budget() {
        let dir = tempfile::tempdir().unwrap();
        let sd = crate::deltaglider::spool::SpoolDir::new(dir.path().join("spool"), 4 * MIB as u64)
            .unwrap();
        let wrapper = probe_wrapper(&sd);
        let src = dir.path().join("src");
        std::fs::write(&src, vec![1u8; 2 * MIB]).unwrap();

        // Another op holds the whole budget.
        let other = sd.acquire(4 * MIB as u64).await.unwrap();
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            wrapper.put_passthrough_file("b", "p", "o", &src, &meta_of(2 * MIB), unheld(&sd)),
        )
        .await
        .expect("a storage write must not wait for spool budget");
        assert!(matches!(r, Err(StorageError::Throttled(_))), "got {r:?}");
        drop(other);

        // Two holders, each with half the budget, write at the same time:
        // neither waits for the other's half.
        let a = sd.acquire(2 * MIB as u64).await.unwrap();
        let b = sd.acquire(2 * MIB as u64).await.unwrap();
        let (ra, rb) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            futures::future::join(
                wrapper.put_passthrough_file(
                    "b",
                    "p",
                    "a",
                    &src,
                    &meta_of(2 * MIB),
                    SpoolBudget::new(&sd, Some(&a), None),
                ),
                wrapper.put_passthrough_file(
                    "b",
                    "p",
                    "b",
                    &src,
                    &meta_of(2 * MIB),
                    SpoolBudget::new(&sd, Some(&b), None),
                ),
            ),
        )
        .await
        .expect("two holders deadlocked on each other's budget");
        for r in [ra, rb] {
            assert!(matches!(r, Err(StorageError::Throttled(_))), "got {r:?}");
        }
        drop((a, b));
        assert_eq!(sd.free_mib(), 4);
    }

    #[tokio::test]
    async fn file_put_spool_bytes_is_zero_without_a_write_key() {
        let plain =
            EncryptingBackend::new(CountingBackend::new(), shared(EncryptionConfig::default()));
        assert_eq!(plain.file_put_spool_bytes("b", 1 << 30, true).await, 0);
        let keyed = EncryptingBackend::new(CountingBackend::new(), shared(keyed(key(1), "kid-1")));
        assert_eq!(
            keyed.file_put_spool_bytes("b", 0, false).await,
            v2::stored_len(0).unwrap()
        );
    }
}

/// storage-7: a read decides sealed-or-plaintext from the metadata of its
/// own backend response, so it sends no separate metadata HEAD.
#[cfg(test)]
mod read_request_tests {
    use super::*;
    use crate::storage::fake_s3;

    fn cfg(key: Option<EncryptionKey>, serve: bool) -> Arc<ArcSwap<EncryptionConfig>> {
        shared(EncryptionConfig {
            key_id: key.as_ref().map(|_| "kid-1".to_string()),
            key,
            serve_plaintext: serve,
            ..Default::default()
        })
    }

    fn k() -> EncryptionKey {
        key(0x42)
    }

    async fn wrapped(
        key: Option<EncryptionKey>,
        serve: bool,
    ) -> (
        EncryptingBackend<crate::storage::S3Backend>,
        Arc<fake_s3::FakeS3>,
        String,
    ) {
        let (ep, fake) = fake_s3::start().await;
        let s3 = crate::storage::s3::test_support::for_test_endpoint(&ep);
        (EncryptingBackend::new(s3, cfg(key, serve)), fake, ep)
    }

    /// The object reads, not the background listing-token writes.
    fn reads(fake: &fake_s3::FakeS3) -> Vec<String> {
        fake.requests()
            .into_iter()
            .filter(|r| !r.contains(".dg/facts") && !r.contains(".dg%2Ffacts"))
            .collect()
    }

    fn one_get(fake: &fake_s3::FakeS3) -> bool {
        let r = reads(fake);
        r.len() == 1 && r[0].starts_with("GET ")
    }

    #[tokio::test]
    async fn a_passthrough_stream_read_is_one_get() {
        for key in [None, Some(k())] {
            let (w, fake, _) = wrapped(key.clone(), false).await;
            let body = b"hello passthrough".to_vec();
            w.put_passthrough(
                "b",
                "p",
                "a.bin",
                Bytes::from(body.clone()),
                &pmeta("a.bin", &body),
            )
            .await
            .unwrap();
            fake.clear();
            let read = w
                .open_object("b", "p", StoredObject::Passthrough("a.bin"))
                .await
                .unwrap();
            let got = collect(read.stream).await.unwrap();
            assert_eq!(got, body, "key: {}", key.is_some());
            assert!(one_get(&fake), "{:?}", fake.requests());
            fake.clear();
            assert_eq!(w.get_passthrough("b", "p", "a.bin").await.unwrap(), body);
            assert!(one_get(&fake), "{:?}", fake.requests());
        }
    }

    /// P3.5 / v2: a range read of a sealed object is ONE ranged GET of the
    /// frames that hold it; the response carries the metadata (no HEAD) and
    /// the frame offsets need no header (no header GET), at any offset.
    #[tokio::test]
    async fn a_sealed_range_is_one_get() {
        let (w, fake, _) = wrapped(Some(k()), false).await;
        let body = body_of(200_000);
        w.put_passthrough_chunked(
            "b",
            "p",
            "a.bin",
            &[Bytes::from(body.clone())],
            &pmeta("a.bin", &body),
        )
        .await
        .unwrap();
        let last = body.len() as u64 - 1;
        for (start, end) in [
            (0, u64::MAX - 1),
            (0, 9),
            (10, 70_000),
            (0, last),
            (70_000, 70_100),
            (150_000, u64::MAX - 1),
            (last, last),
        ] {
            let e = end.min(last);
            let expected = &body[start as usize..=e as usize];
            fake.clear();
            let RangeRead {
                stream,
                len,
                metadata: m,
                ..
            } = w
                .open_passthrough_range("b", "p", "a.bin", start, end)
                .await
                .unwrap();
            assert_eq!(
                collect(stream).await.unwrap(),
                expected,
                "open {start}-{end}"
            );
            assert_eq!(len, expected.len() as u64);
            assert_eq!(m.file_size, body.len() as u64);
            assert!(one_get(&fake), "open {start}-{end}: {:?}", reads(&fake));
            fake.clear();
            let crate::storage::RangeRead { stream, len, .. } = w
                .open_passthrough_range("b", "p", "a.bin", start, end)
                .await
                .unwrap();
            assert_eq!(
                collect(stream).await.unwrap(),
                expected,
                "range {start}-{end}"
            );
            assert_eq!(len, expected.len() as u64);
            assert!(one_get(&fake), "{:?}", reads(&fake));
        }
        // A range past the end is refused.
        for start in [body.len() as u64, 4 * F as u64, 10 * F as u64] {
            assert!(
                matches!(
                    w.open_passthrough_range("b", "p", "a.bin", start, u64::MAX - 1)
                        .await,
                    Err(StorageError::InvalidRange(_))
                ),
                "{start}"
            );
        }
    }

    /// The forms a keyed bucket can hold: sealed (buffered or chunked
    /// write), and plaintext, which is served only under `serve`. A sealed
    /// body whose markers were stripped is refused, never served.
    #[tokio::test]
    async fn keyed_range_reads_serve_every_stored_form() {
        for serve in [false, true] {
            let (w, _fake, _) = wrapped(Some(k()), serve).await;
            let body = body_of(1000);
            w.put_passthrough(
                "b",
                "p",
                "buffered",
                Bytes::from(body.clone()),
                &pmeta("x", &body),
            )
            .await
            .unwrap();
            w.put_passthrough_chunked(
                "b",
                "p",
                "chunked",
                &[Bytes::from(body.clone())],
                &pmeta("x", &body),
            )
            .await
            .unwrap();
            w.inner
                .put_passthrough(
                    "b",
                    "p",
                    "plain",
                    Bytes::from(body.clone()),
                    &pmeta("x", &body),
                )
                .await
                .unwrap();
            for name in ["buffered", "chunked", "plain"] {
                for (start, end) in [(0, u64::MAX - 1), (0, 9), (500, 600)] {
                    let got = read_range(&w, "b", "p", name, start, end).await;
                    if name == "plain" && !serve {
                        assert!(got.is_err(), "plaintext served without serve");
                        continue;
                    }
                    let e = end.min(body.len() as u64 - 1) as usize;
                    assert_eq!(
                        got.unwrap(),
                        &body[start as usize..=e],
                        "{name} {start}-{end}"
                    );
                }
            }
            // Ciphertext with its markers stripped (a foreign rewrite).
            let big = body_of(100_000);
            w.put_passthrough_chunked(
                "b",
                "p",
                "c.bin",
                &[Bytes::from(big.clone())],
                &pmeta("c.bin", &big),
            )
            .await
            .unwrap();
            let wire = w.inner.get_passthrough("b", "p", "c.bin").await.unwrap();
            w.inner
                .put_passthrough("b", "p", "c.bin", Bytes::from(wire), &pmeta("c.bin", &big))
                .await
                .unwrap();
            for (start, end) in [(0, 99), (500, 600), (70_000, 70_100)] {
                assert!(
                    read_range(&w, "b", "p", "c.bin", start, end).await.is_err(),
                    "serve {serve}: stripped ciphertext served {start}-{end}"
                );
            }
        }
    }

    /// A ranged read whose follow-up starts past a plaintext object's end
    /// gets a 416 from the backend: `InvalidRange`, never a 500.
    #[tokio::test]
    async fn a_plaintext_range_past_the_end_on_a_keyed_backend_is_invalid_range() {
        let (w, _fake, _) = wrapped(Some(k()), true).await;
        let small = body_of(1000);
        w.inner
            .put_passthrough(
                "b",
                "p",
                "plain.bin",
                Bytes::from(small.clone()),
                &pmeta("x", &small),
            )
            .await
            .unwrap();
        for start in [1000, 70_000u64, F as u64] {
            let res = w
                .open_passthrough_range("b", "p", "plain.bin", start, u64::MAX - 1)
                .await;
            assert!(
                matches!(res, Err(StorageError::InvalidRange(_))),
                "start {start}: {:?}",
                res.map(|r| r.len)
            );
        }
    }

    #[tokio::test]
    async fn delta_and_reference_reads_are_one_get() {
        for key in [None, Some(k())] {
            let (w, fake, _) = wrapped(key.clone(), false).await;
            let body = b"delta bytes".to_vec();
            w.put_delta(
                "b",
                "p",
                "a.bin",
                Bytes::from(body.clone()),
                &dmeta("a.bin", &body),
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .unwrap();
            w.put_reference(
                "b",
                "p",
                Bytes::from(body.clone()),
                &rmeta(&body),
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .unwrap();
            fake.clear();
            assert_eq!(w.get_delta("b", "p", "a.bin").await.unwrap(), body);
            assert!(one_get(&fake), "{:?}", fake.requests());
            fake.clear();
            assert_eq!(w.get_reference("b", "p").await.unwrap(), body);
            assert!(one_get(&fake), "{:?}", fake.requests());
        }
    }

    /// The S3 write path keeps a `!!2` listing token, made by the wrapper's
    /// maker (`FileMetadata::listing_token`) from the stored ETag and size;
    /// a LIST on another node (no shared process cache) opens it and lists
    /// the logical size and ETag without a HEAD.
    #[tokio::test]
    async fn an_s3_put_keeps_a_listing_token_that_a_list_applies() {
        let (w, fake, ep) = wrapped(Some(k()), false).await;
        let body = body_of(3000);
        w.put_passthrough(
            "b",
            "p",
            "a.bin",
            Bytes::from(body.clone()),
            &pmeta("a.bin", &body),
        )
        .await
        .unwrap();
        assert!(
            fake.wait_for(|r| r.starts_with("PUT /b/.dg/facts/")).await,
            "no token write: {:?}",
            fake.requests()
        );
        let token_put = fake
            .requests()
            .into_iter()
            .find(|r| r.starts_with("PUT /b/.dg/facts/"))
            .unwrap();
        assert!(
            token_put.contains("!!2.") || token_put.contains("%21%212."),
            "{token_put}"
        );
        let other = EncryptingBackend::new(
            crate::storage::s3::test_support::for_test_endpoint_as_node(&ep, "n2"),
            cfg(Some(k()), false),
        );
        let mut objects = other.bulk_list_objects("b", "p/").await.unwrap();
        assert_eq!(objects.len(), 1, "{objects:?}");
        fake.clear();
        let sizes = other
            .resolve_listed_sizes("b", &mut objects, ResolveListed::default())
            .await;
        assert_eq!(sizes, [ListedSize::Cached]);
        assert_eq!(objects[0].1.file_size, body.len() as u64);
        assert_eq!(objects[0].1.etag(), format!("\"{}\"", md5_hex(&body)));
        assert!(
            !fake.requests().iter().any(|r| r.starts_with("HEAD ")),
            "{:?}",
            fake.requests()
        );
    }
}

#[cfg(test)]
mod pinned_follow_up_tests {
    use super::*;
    use crate::storage::fake_s3;

    #[derive(Clone, Copy, Debug)]
    enum Form {
        Plain,
        Sealed,
    }

    #[test]
    fn same_stored_version_truth_table() {
        use crate::storage::traits::same_stored_version;
        let a = pmeta("x.bin", b"hello");
        assert!(same_stored_version(&a, &a.clone()));
        let mut later = a.clone();
        later.created_at += chrono::Duration::microseconds(1);
        assert!(!same_stored_version(&a, &later), "another write");
        let mut other = a.clone();
        other.md5 = "00".repeat(16);
        assert!(!same_stored_version(&a, &other), "other content");
        let mut sized = a.clone();
        sized.file_size += 1;
        assert!(!same_stored_version(&a, &sized), "other size");
        let mut sealed = a.clone();
        sealed
            .user_metadata
            .insert(seal::SEALED_KEY.into(), "2.one".into());
        assert!(!same_stored_version(&a, &sealed), "other format");
        let mut resealed = sealed.clone();
        resealed
            .user_metadata
            .insert(seal::SEALED_KEY.into(), "2.two".into());
        assert!(!same_stored_version(&sealed, &resealed), "another seal");
    }

    /// A keyed range read takes the object's metadata from its first GET.
    /// A sealed object needs no second request. A plaintext one (under
    /// `serve`) does: when another version replaced the object between the
    /// two, the read fails with a retryable 503 SlowDown, so the bytes of
    /// one version never go out with the metadata of another.
    #[tokio::test]
    async fn a_follow_up_read_of_another_version_is_slow_down() {
        let k = key(0xaa);
        let a = body_of(200_000);
        let b: Vec<u8> = (0..200_000u32).map(|i| (i % 13) as u8).collect();
        for (from, to, start) in [
            (Form::Plain, Form::Sealed, 70_000),
            (Form::Plain, Form::Plain, 70_000),
            (Form::Plain, Form::Plain, 0),
            (Form::Sealed, Form::Plain, 70_000),
            (Form::Sealed, Form::Sealed, 140_000),
        ] {
            let (ep, fake) = fake_s3::start().await;
            let mk = || crate::storage::s3::test_support::for_test_endpoint(&ep);
            let plain = EncryptingBackend::new(mk(), shared(EncryptionConfig::default()));
            let keyed = EncryptingBackend::new(
                mk(),
                shared(EncryptionConfig {
                    serve_plaintext: true,
                    ..keyed(k.clone(), "kid-a")
                }),
            );
            for (form, name, data) in [(from, "x.bin", &a), (to, "y.bin", &b)] {
                let w = match form {
                    Form::Plain => &plain,
                    Form::Sealed => &keyed,
                };
                w.put_passthrough(
                    "b",
                    "p",
                    name,
                    Bytes::from(data.clone()),
                    &pmeta(name, data),
                )
                .await
                .unwrap();
            }
            fake.overwrite_after_reads("b/p/x.bin", "b/p/y.bin", 1);
            let label = format!("{from:?} -> {to:?} at {start}");
            let read = async {
                let RangeRead {
                    stream, metadata, ..
                } = keyed
                    .open_passthrough_range("b", "p", "x.bin", start, start + 99)
                    .await?;
                Ok::<_, StorageError>((collect(stream).await?, metadata))
            };
            let want_a = &a[start as usize..start as usize + 100];
            match (from, read.await) {
                (Form::Plain, Err(StorageError::Throttled(_))) => {}
                (Form::Sealed, Ok((body, m))) => {
                    assert_eq!(body, want_a, "{label}");
                    assert_eq!(m.md5, md5_hex(&a), "{label}");
                }
                (_, Ok((body, m))) => panic!(
                    "{label}: served {} bytes, A's: {}, etag {}",
                    body.len(),
                    body == want_a,
                    m.etag()
                ),
                (_, Err(e)) => panic!("{label}: {e}"),
            }
        }
    }
}

/// THE rule: a range read of this wrapper that needs a second request
/// reads it with `pinned_range` (the response's own metadata checked
/// against the first read's), never with a bare read of the inner backend
/// that carries no metadata and so cannot tell another version.
#[test]
fn follow_up_reads_are_pinned_to_the_first_read() {
    let text = std::fs::read_to_string("src/storage/encrypting/mod.rs").unwrap();
    let prod = crate::source_scan::prod_text(&text);
    let squashed: String = prod.split_whitespace().collect();
    assert!(
        !squashed.contains("inner.get_passthrough(") && !squashed.contains("inner.get_object("),
        "encrypting/mod.rs reads the inner backend without metadata: use pinned_range"
    );
    // The plaintext follow-up goes through the pinned helper, which is
    // the one place that checks the version.
    let helper = squashed
        .split("asyncfnplaintext_range(")
        .nth(1)
        .expect("plaintext_range is gone: update this guard");
    let helper = &helper[..helper.find("asyncfn").unwrap_or(helper.len())];
    assert!(
        helper.contains(".pinned_range("),
        "plaintext_range must pin"
    );
    assert!(squashed.contains("same_stored_version(pin,&read.metadata)"));
}

#[cfg(test)]
mod range_matrix_tests {
    use super::*;
    use crate::storage::fake_s3;

    fn cfg(
        key: Option<EncryptionKey>,
        kid: Option<&str>,
        legacy: Option<(EncryptionKey, &str)>,
        serve: bool,
    ) -> Arc<ArcSwap<EncryptionConfig>> {
        shared(EncryptionConfig {
            key,
            key_id: kid.map(str::to_string),
            legacy_key: legacy.as_ref().map(|l| l.0.clone()),
            legacy_key_id: legacy.map(|l| l.1.to_string()),
            serve_plaintext: serve,
            ..Default::default()
        })
    }

    async fn outcome<B: StorageBackend + 'static>(
        w: &EncryptingBackend<B>,
        name: &str,
        start: u64,
        end: u64,
    ) -> String {
        match w.open_passthrough_range("b", "p", name, start, end).await {
            Ok(r) => match collect(r.stream).await {
                Ok(body) => {
                    format!(
                        "OK len={} bodylen={} body_md5={} size={} etag={}",
                        r.len,
                        body.len(),
                        md5_hex(&body),
                        r.metadata.file_size,
                        r.metadata.etag()
                    )
                }
                Err(e) => format!("STREAM-ERR {e}"),
            },
            Err(StorageError::InvalidRange(_)) => "ERR InvalidRange".to_string(),
            Err(e) => format!("ERR {e}"),
        }
    }

    async fn run<B: StorageBackend + 'static, F: Fn() -> B>(mk: F) -> Vec<String> {
        let (a, p) = (key(0xaa), key(0xbb));
        let writer_a =
            EncryptingBackend::new(mk(), cfg(Some(a.clone()), Some("kid-a"), None, false));
        let writer_nokid = EncryptingBackend::new(mk(), cfg(Some(a.clone()), None, None, false));
        let writer_prev =
            EncryptingBackend::new(mk(), cfg(Some(p.clone()), Some("kid-p"), None, false));
        let plain_w = EncryptingBackend::new(mk(), cfg(None, None, None, false));
        let reader = EncryptingBackend::new(
            mk(),
            cfg(
                Some(a.clone()),
                Some("kid-a"),
                Some((p.clone(), "kid-p")),
                true,
            ),
        );
        let unkeyed = EncryptingBackend::new(mk(), cfg(None, None, None, false));
        let _ = plain_w.create_bucket("b").await;
        let mut mismatches = Vec::new();
        // The frame edges: empty, one byte, one frame and one byte more,
        // two frames and one byte more.
        let sizes: [u64; 6] = [0, 1, 76, 65536, 65537, 131073];
        for size in sizes {
            let body = body_of(size as usize);
            let m = pmeta("a.bin", &body);
            let b = Bytes::from(body.clone());
            plain_w
                .put_passthrough("b", "p", "plain", b.clone(), &m)
                .await
                .unwrap();
            for (w, prefix) in [
                (&writer_a, ""),
                (&writer_nokid, "nokid-"),
                (&writer_prev, "prev-"),
            ] {
                w.put_passthrough_chunked(
                    "b",
                    "p",
                    &format!("{prefix}chunked"),
                    std::slice::from_ref(&b),
                    &m,
                )
                .await
                .unwrap();
                w.put_passthrough("b", "p", &format!("{prefix}buffered"), b.clone(), &m)
                    .await
                    .unwrap();
            }
            let last = size.saturating_sub(1);
            let max = u64::MAX - 1;
            let mut ranges: Vec<(u64, u64)> = vec![
                (0, 0),
                (0, max),
                (0, last),
                (last, last),
                (last, max),
                (65530, 65545),
                (65535, 65536),
                (65536, 65536),
                (65536, max),
                (131071, 131072),
                (70000, max),
                (size, max),
                (size + 10, size + 20),
                (1, 1),
                (75, 76),
                (76, 76),
            ];
            // suffix bytes=-N as the adapter resolves it (HEAD-first)
            for n in [1u64, 10, 65536, 70000] {
                if size > 0 {
                    ranges.push((size.saturating_sub(n), last));
                }
            }
            for (s, e) in ranges {
                let twin = outcome(&unkeyed, "plain", s, e).await;
                for name in [
                    "plain",
                    "chunked",
                    "buffered",
                    "nokid-chunked",
                    "nokid-buffered",
                    "prev-chunked",
                    "prev-buffered",
                ] {
                    let got = outcome(&reader, name, s, e).await;
                    if got != twin {
                        mismatches.push(format!(
                            "size={size} range={s}-{e} {name}: twin=[{twin}] got=[{got}]"
                        ));
                    }
                }
            }
        }
        mismatches
    }

    /// Every stored form behind a keyed reader (plaintext under `serve`,
    /// sealed chunked and buffered writes, with and without a key id, and
    /// under the previous key) answers every range shape exactly as an
    /// unkeyed read of the plaintext twin: the same length, bytes and
    /// metadata, or the same error. On S3 also with a store that sends GET
    /// bodies chunked (no Content-Length).
    #[tokio::test]
    async fn every_form_answers_ranges_as_its_plaintext_twin_on_s3() {
        for chunked in [false, true] {
            let (ep, fake) = fake_s3::start().await;
            fake.set_chunked_gets(chunked);
            let mm = run(|| crate::storage::s3::test_support::for_test_endpoint(&ep)).await;
            assert!(mm.is_empty(), "chunked GETs {chunked}: {mm:#?}");
        }
    }

    #[tokio::test]
    async fn every_form_answers_ranges_as_its_plaintext_twin_on_the_filesystem() {
        use crate::storage::filesystem::FilesystemBackend;
        let dir = tempfile::tempdir().unwrap();
        let mut backends = Vec::new();
        for _ in 0..6 {
            backends.push(
                FilesystemBackend::new(dir.path().to_path_buf())
                    .await
                    .unwrap(),
            );
        }
        let cell = std::sync::Mutex::new(backends);
        let mm = run(|| cell.lock().unwrap().pop().unwrap()).await;
        assert!(mm.is_empty(), "{mm:#?}");
    }
}
