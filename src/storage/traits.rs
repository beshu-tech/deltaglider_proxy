// SPDX-License-Identifier: GPL-3.0-only

//! Storage backend trait definitions

use crate::types::FileMetadata;
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Bucket listing entry with optional routing-origin metadata.
#[derive(Debug, Clone)]
pub struct BucketListing {
    pub name: String,
    pub creation_date: chrono::DateTime<chrono::Utc>,
    /// Configured backend name when known (for `RoutingBackend` listings).
    pub backend_name: Option<String>,
    /// Real bucket name on that backend, when it differs from the visible name.
    pub real_bucket: Option<String>,
    /// `Some(origin_error)` when this bucket's backend could NOT be listed
    /// (503/throttle/connection). The bucket is config-declared so we still
    /// surface it — flagged unavailable, carrying the VERBATIM backend error so
    /// an operator sees exactly why it's dark. `None` = listed live, reachable.
    pub unavailable: Option<String>,
}

/// Errors that can occur during storage operations
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("Object not found: {0}")]
    NotFound(String),

    #[error("Object already exists: {0}")]
    AlreadyExists(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Disk full: insufficient storage space")]
    DiskFull,

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Object too large: {size} bytes (max: {max} bytes)")]
    TooLarge { size: u64, max: u64 },

    #[error("S3 error: {0}")]
    S3(String),

    #[error("Bucket not found: {0}")]
    BucketNotFound(String),

    #[error("Encryption error: {0}")]
    Encryption(String),

    #[error("Bucket not empty: {0}")]
    BucketNotEmpty(String),

    /// Backend-side throttling — the upstream returned 503 SlowDown
    /// (or equivalent transient pressure signal). E-P1-1: distinct
    /// from `S3(...)` so the API layer can surface this as
    /// `S3Error::SlowDown` (which the AWS SDK retry contract treats
    /// as transient) rather than `S3Error::InternalError` (permanent
    /// 500). See `From<StorageError> for S3Error` in `api/errors.rs`.
    #[error("Backend throttled: {0}")]
    Throttled(String),

    #[error("Storage error: {0}")]
    Other(String),
}

/// Abstract storage backend for S3-like object storage
/// Uses per-file metadata following DeltaGlider schema (xattr on filesystem, S3 user metadata headers on S3)
///
/// This trait is object-safe and can be used with `Box<dyn StorageBackend>`.
///
/// All methods take a `bucket` parameter which maps to a real storage bucket
/// (S3 bucket or filesystem directory).
#[async_trait]
pub trait StorageBackend: Send + Sync {
    // === Bucket operations ===

    /// Create a new bucket
    async fn create_bucket(&self, bucket: &str) -> Result<(), StorageError>;

    /// Delete a bucket (must be empty)
    async fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError>;

    /// List all buckets
    async fn list_buckets(&self) -> Result<Vec<String>, StorageError>;

    /// List all buckets with their creation dates.
    /// Default implementation falls back to `list_buckets()` with current time.
    async fn list_buckets_with_dates(
        &self,
    ) -> Result<Vec<(String, chrono::DateTime<chrono::Utc>)>, StorageError> {
        let names = self.list_buckets().await?;
        Ok(names.into_iter().map(|n| (n, chrono::Utc::now())).collect())
    }

    /// List buckets with optional backend-origin metadata.
    ///
    /// Concrete single backends usually don't know their configured name, so
    /// the default leaves origin fields empty. `RoutingBackend` overrides this
    /// to preserve the backend that produced each bucket.
    async fn list_bucket_origins(&self) -> Result<Vec<BucketListing>, StorageError> {
        Ok(self
            .list_buckets_with_dates()
            .await?
            .into_iter()
            .map(|(name, creation_date)| BucketListing {
                name,
                creation_date,
                backend_name: None,
                real_bucket: None,
                unavailable: None,
            })
            .collect())
    }

    /// Check if a bucket exists
    async fn head_bucket(&self, bucket: &str) -> Result<bool, StorageError>;

    // === Reference file operations ===

    /// Get the reference file for a deltaspace
    async fn get_reference(&self, bucket: &str, prefix: &str) -> Result<Vec<u8>, StorageError>;

    /// Materialise the reference file at `dest` as a seekable local file, WITHOUT
    /// heap-loading it — the streaming codec (`encode_from_reader` /
    /// `decode_to_writer`) needs the reference as a file xdelta3 can mmap, and a
    /// multi-GB reference must never be buffered in RAM (adversarial blocker 10).
    ///
    /// Returns the number of bytes written. The default impl falls back to
    /// `get_reference` + write (fine for small references / backends that don't
    /// override); the filesystem backend hardlinks (near-zero) and the S3 backend
    /// streams the GET body to the file.
    async fn get_reference_to_file(
        &self,
        bucket: &str,
        prefix: &str,
        dest: &std::path::Path,
    ) -> Result<u64, StorageError> {
        let data = self.get_reference(bucket, prefix).await?;
        tokio::fs::write(dest, &data).await?;
        Ok(data.len() as u64)
    }

    /// Store a reference file with its metadata
    async fn put_reference(
        &self,
        bucket: &str,
        prefix: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError>;

    /// Store a reference whose data is on a local file, WITHOUT heap-loading it
    /// (review M1.4: the streaming baseline path must not read a multi-GB first
    /// member into RAM). Default reads the file then delegates to `put_reference`
    /// (correct for any backend); the filesystem backend hardlinks and S3 streams
    /// the upload from the file — mirrors `get_reference_to_file`.
    async fn put_reference_from_file(
        &self,
        bucket: &str,
        prefix: &str,
        source_path: &std::path::Path,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let data = tokio::fs::read(source_path).await?;
        self.put_reference(bucket, prefix, &data, metadata).await
    }

    /// Store/update reference metadata without rewriting reference data.
    async fn put_reference_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError>;

    /// Get reference file metadata
    async fn get_reference_metadata(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<FileMetadata, StorageError>;

    /// Check if reference exists
    async fn has_reference(&self, bucket: &str, prefix: &str) -> bool;

    /// Delete a reference file and its metadata
    async fn delete_reference(&self, bucket: &str, prefix: &str) -> Result<(), StorageError>;

    // === Delta file operations ===

    /// Get a delta file
    async fn get_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError>;

    /// Store a delta file with its metadata
    async fn put_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError>;

    /// Get delta file metadata
    async fn get_delta_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError>;

    /// Delete a delta file and its metadata
    async fn delete_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError>;

    // === Passthrough file operations (stored as-is with original filename) ===

    /// Get a passthrough (non-delta) file
    async fn get_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError>;

    /// Store a passthrough (non-delta) file with its metadata
    async fn put_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError>;

    /// Store a passthrough file from an on-disk source path.
    /// Default implementation reads the full file and delegates to `put_passthrough`.
    async fn put_passthrough_file(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        source_path: &Path,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let data = tokio::fs::read(source_path).await?;
        self.put_passthrough(bucket, prefix, filename, &data, metadata)
            .await
    }

    /// Store a passthrough file from ordered relay part paths.
    /// Default implementation materializes an intermediate file in-memory.
    async fn put_passthrough_parts(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        part_paths: &[PathBuf],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let mut assembled = Vec::new();
        for path in part_paths {
            let part = tokio::fs::read(path).await?;
            assembled.extend_from_slice(&part);
        }
        self.put_passthrough(bucket, prefix, filename, &assembled, metadata)
            .await
    }

    /// Get passthrough file metadata
    async fn get_passthrough_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError>;

    /// Delete a passthrough (non-delta) file and its metadata
    async fn delete_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError>;

    // === Streaming operations ===

    /// Stream a passthrough file's contents without buffering the entire file in
    /// memory. REQUIRED (no default) — a buffering default would silently defeat
    /// the memory bound for large objects on a backend that forgot to override
    /// it. The type system enforces every backend provides a real streaming read
    /// (this used to be a buffering default guarded by a grep conformance test).
    async fn get_passthrough_stream(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError>;

    /// Stream a byte range of a passthrough file without buffering the entire
    /// object. REQUIRED (no default) — same memory-bound rationale as
    /// [`get_passthrough_stream`](Self::get_passthrough_stream): a buffering
    /// fallback would fetch the whole object first, defeating the bound a ranged
    /// GET promises (the E2 invariant). Backends with native range reads (S3,
    /// filesystem, the EncryptingBackend chunked wrapper) implement it directly;
    /// a backend with no native range support must still provide an explicit
    /// (even if full-stream) impl so the choice is VISIBLE, not silently inherited.
    ///
    /// Returns `(stream, content_length)` where `content_length` is the number of
    /// bytes in the range (0 signals "full stream, not a range").
    async fn get_passthrough_stream_range(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        start: u64,
        end: u64, // inclusive
    ) -> Result<(BoxStream<'static, Result<Bytes, StorageError>>, u64), StorageError>;

    /// Store a passthrough file from pre-split chunks without assembling into a contiguous buffer.
    /// Default implementation collects chunks and delegates to `put_passthrough()`.
    async fn put_passthrough_chunked(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        chunks: &[Bytes],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let total_len: usize = chunks.iter().map(|c| c.len()).sum();
        let mut buf = Vec::with_capacity(total_len);
        for chunk in chunks {
            buf.extend_from_slice(chunk);
        }
        self.put_passthrough(bucket, prefix, filename, &buf, metadata)
            .await
    }

    // === Multipart upload operations (Phase B: streaming large-object copy) ===
    //
    // These four methods let the transfer layer drive a multipart upload
    // to the backend with bounded memory. `S3Backend` overrides them with
    // native aws-sdk multipart so the prod S3→S3 path stays O(part_size).
    // The DEFAULT impls (filesystem + test spies) report `native: false`,
    // which makes the transfer layer retain each part's bytes and hand the
    // assembled body to `complete_multipart_upload` — correct but O(object)
    // in memory. Proxy-AES-encrypting backends are gated OFF at the
    // transfer layer (`backend_supports_native_multipart`), so multipart is
    // never invoked on them — their default impl just needs to compile.

    /// Begin a multipart upload. `native: false` (the default) tells the
    /// caller this backend has no incremental part durability — it must
    /// retain part bytes and pass them to `complete_multipart_upload`.
    async fn create_multipart_upload(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        metadata: &FileMetadata,
    ) -> Result<MultipartUpload, StorageError> {
        let _ = (prefix, filename, metadata);
        Ok(MultipartUpload {
            bucket: bucket.to_string(),
            upload_id: "buffered-default".to_string(),
            native: false,
            backend: None,
        })
    }

    /// Upload one part (1-indexed `part_number`), returning its ETag.
    /// The default is a no-op that derives an ETag from the part bytes;
    /// the caller keeps the bytes for `complete` because `native: false`.
    async fn upload_part(
        &self,
        upload: &MultipartUpload,
        prefix: &str,
        filename: &str,
        part_number: i32,
        data: Bytes,
    ) -> Result<UploadedPart, StorageError> {
        // The buffering default ignores part ETags in `complete`, so a
        // placeholder is sufficient; the caller retains `data` itself.
        let _ = (upload, prefix, filename, &data);
        Ok(UploadedPart {
            part_number,
            etag: String::new(),
        })
    }

    /// Complete a multipart upload. `assembled` carries every part's bytes
    /// in order (populated by the caller for `native: false` backends);
    /// native backends ignore it and finalize from the part ETags. Returns
    /// the final multipart ETag.
    async fn complete_multipart_upload(
        &self,
        upload: &MultipartUpload,
        prefix: &str,
        filename: &str,
        parts: &[UploadedPart],
        assembled: &[Bytes],
        metadata: &FileMetadata,
    ) -> Result<String, StorageError> {
        let _ = parts;
        self.put_passthrough_chunked(&upload.bucket, prefix, filename, assembled, metadata)
            .await?;
        Ok(format!("\"{}\"", metadata.md5))
    }

    /// Abort an in-progress multipart upload (best-effort cleanup).
    async fn abort_multipart_upload(
        &self,
        upload: &MultipartUpload,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        let _ = (upload, prefix, filename);
        Ok(())
    }

    /// Encryption-mode label for the backend serving `bucket`, matching
    /// `BackendEncryptionConfig::mode_tag` (`"none"` / `"aes256-gcm-proxy"`
    /// / `"sse-kms"` / `"sse-s3"`). Feeds the pure
    /// `transfer_plan::backend_supports_native_multipart` capability gate.
    /// Default is `"none"` (plaintext); the `EncryptingBackend` wrapper and
    /// `RoutingBackend` override it.
    fn multipart_storage_label(&self, _bucket: &str) -> &'static str {
        "none"
    }

    /// Does the backend serving `bucket` write multipart parts DURABLY and
    /// incrementally (S3 `native: true`)? The streaming copy path is only
    /// memory-bounded on such backends — a `false` backend forces the caller
    /// to retain every part's bytes for `complete` (O(object_size) RAM), so
    /// the streaming gate must route those to the spooled/buffered path
    /// instead. Default `false` (the buffering default impl + filesystem).
    fn supports_native_multipart(&self, _bucket: &str) -> bool {
        false
    }

    /// Does the LITE list (`bulk_list_objects` / `scan_deltaspace_lite`) for
    /// `bucket` carry trustworthy LOGICAL facts — real user_metadata (for
    /// replication-provenance ownership) AND plaintext size/etag? Filesystem
    /// stamps xattr metadata into the lite entry (true); S3 LIST returns
    /// neither user metadata nor plaintext size on an encrypted backend
    /// (false → parity must resolve every key via HEAD). Default `true` (the
    /// filesystem default); S3 + actively-encrypting wrapper override `false`.
    fn lite_list_carries_logical_facts(&self, _bucket: &str) -> bool {
        true
    }

    // === Scanning operations ===

    /// Scan a deltaspace directory and return all file metadata
    /// This replaces the centralized index - state is derived from files
    async fn scan_deltaspace(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<FileMetadata>, StorageError>;

    /// Cheap variant of [`scan_deltaspace`] for diagnostics-only callers
    /// that don't need exact `original_size` for delta files.
    ///
    /// On S3, `scan_deltaspace` fires a bounded-parallel HEAD storm to
    /// recover the original-file size of every `.delta` from user
    /// metadata. For a bucket with N prefixes × M deltas/prefix, that's
    /// N×M HEADs on top of N LISTs. The delta-efficiency scanner only
    /// needs delta sizes (already in the listing) + the reference size
    /// (also in the listing), not original sizes — so it can skip the
    /// HEAD storm entirely.
    ///
    /// Backends that don't HEAD anyway (e.g. filesystem reads xattr
    /// inline) inherit the default impl which delegates to
    /// `scan_deltaspace` and reports `originals_estimated: false`
    /// because xattr already supplies the original sizes.
    ///
    /// `originals_estimated: true` in the returned struct means that
    /// for delta entries `file_size` is the **on-disk delta size**, not
    /// the original — callers should not derive "savings" or "original
    /// total" from it. The classifier's median-ratio verdict is
    /// unaffected either way (it uses `StorageInfo::Delta.delta_size`,
    /// which is the same number across both shapes).
    async fn scan_deltaspace_lite(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<LiteScanResult, StorageError> {
        // Default: HEAD-based path supplies real originals, so the
        // caller can trust file_size on deltas.
        let metadata = self.scan_deltaspace(bucket, prefix).await?;
        Ok(LiteScanResult {
            metadata,
            originals_estimated: false,
        })
    }

    /// List all deltaspace prefixes within a bucket
    async fn list_deltaspaces(&self, bucket: &str) -> Result<Vec<String>, StorageError>;

    /// Get total storage size used (for metrics), optionally scoped to a bucket
    async fn total_size(&self, bucket: Option<&str>) -> Result<u64, StorageError>;

    /// Store a zero-byte S3 directory marker (key ending with '/').
    /// Used by Cyberduck, AWS Console, etc. to create "folders".
    /// Default: no-op (directories are implicit in S3).
    async fn put_directory_marker(&self, _bucket: &str, _key: &str) -> Result<(), StorageError> {
        Ok(())
    }

    /// List all objects in a bucket matching a prefix, in a single pass.
    /// Returns `(user_visible_key, FileMetadata)` pairs — references are excluded,
    /// directory markers are included. This replaces the three-step
    /// list_deltaspaces → scan_deltaspace × N → list_directory_markers dance.
    async fn bulk_list_objects(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<(String, FileMetadata)>, StorageError>;

    /// Enrich listed objects with full metadata from HEAD calls.
    /// Used by the `metadata=true` MinIO ListObjectsV2 extension.
    ///
    /// The default implementation returns objects unchanged (suitable for
    /// backends like filesystem that already populate full metadata in
    /// `bulk_list_objects`).
    async fn enrich_list_metadata(
        &self,
        _bucket: &str,
        objects: Vec<(String, FileMetadata)>,
    ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
        Ok(objects)
    }

    /// Optimised listing with delimiter support.
    ///
    /// Backends that can delegate delimiter collapsing to the underlying store
    /// (e.g. S3) override this to avoid fetching every object just to collapse
    /// them into CommonPrefixes.  Returns `None` by default so the engine
    /// falls back to `bulk_list_objects` + in-memory collapsing.
    async fn list_objects_delegated(
        &self,
        _bucket: &str,
        _prefix: &str,
        _delimiter: &str,
        _max_keys: u32,
        _continuation_token: Option<&str>,
    ) -> Result<Option<DelegatedListResult>, StorageError> {
        Ok(None)
    }
}

/// Handle for an in-progress multipart upload (Phase B streaming copy).
#[derive(Debug, Clone)]
pub struct MultipartUpload {
    /// Real bucket the upload targets (may differ from the visible bucket
    /// for routing backends).
    pub bucket: String,
    /// Backend-assigned upload id (S3 `UploadId`; synthetic for the
    /// buffering default).
    pub upload_id: String,
    /// True when each `upload_part` is durably written incrementally (S3):
    /// the caller may drop part bytes after upload. False (the default)
    /// means the caller must retain part bytes for `complete`.
    pub native: bool,
    /// Routing attribution: the configured backend name this upload was
    /// resolved to. Set by `RoutingBackend::create_multipart_upload` so
    /// `upload_part`/`complete`/`abort` re-target the SAME backend without
    /// re-probing. `None` for single-backend deployments.
    pub backend: Option<String>,
}

/// One completed part of a multipart upload (1-indexed number + ETag).
#[derive(Debug, Clone)]
pub struct UploadedPart {
    pub part_number: i32,
    pub etag: String,
}

/// Result from `list_objects_delegated` when the backend handles delimiter
/// collapsing natively.
pub struct DelegatedListResult {
    pub objects: Vec<(String, FileMetadata)>,
    pub common_prefixes: Vec<String>,
    pub is_truncated: bool,
    pub next_continuation_token: Option<String>,
}

/// Result from [`StorageBackend::scan_deltaspace_lite`].
///
/// `originals_estimated` tells the caller whether `file_size` on the
/// returned `FileMetadata` entries with `StorageInfo::Delta` is the
/// **original-file size** (false — caller can compute true savings) or
/// the **on-disk delta size** (true — caller must suppress
/// "savings"/"original total" displays to avoid misleading negative
/// numbers).
///
/// The classifier verdict and median-ratio are unaffected either way:
/// they're computed from `StorageInfo::Delta.delta_size`, which both
/// shapes populate identically from listing data.
pub struct LiteScanResult {
    pub metadata: Vec<FileMetadata>,
    pub originals_estimated: bool,
}

/// Generate the blanket `impl StorageBackend for Box<dyn StorageBackend>`
/// that forwards every method through dynamic dispatch.
macro_rules! impl_storage_backend_for_box {
    () => {
        #[async_trait]
        impl StorageBackend for Box<dyn StorageBackend> {
            async fn create_bucket(&self, bucket: &str) -> Result<(), StorageError> {
                (**self).create_bucket(bucket).await
            }
            async fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError> {
                (**self).delete_bucket(bucket).await
            }
            async fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
                (**self).list_buckets().await
            }
            async fn list_buckets_with_dates(
                &self,
            ) -> Result<Vec<(String, chrono::DateTime<chrono::Utc>)>, StorageError> {
                (**self).list_buckets_with_dates().await
            }
            // MUST forward to the inner backend's override — otherwise a
            // `Box<dyn StorageBackend>` (the engine's storage type) would fall
            // through to the trait DEFAULT impl, which returns
            // `backend_name: None` for every bucket and erases the routing
            // layer's per-bucket backend attribution. That made the admin
            // bucket-origins API mis-report every bucket as the default backend.
            async fn list_bucket_origins(&self) -> Result<Vec<BucketListing>, StorageError> {
                (**self).list_bucket_origins().await
            }
            async fn head_bucket(&self, bucket: &str) -> Result<bool, StorageError> {
                (**self).head_bucket(bucket).await
            }

            async fn get_reference(
                &self,
                bucket: &str,
                prefix: &str,
            ) -> Result<Vec<u8>, StorageError> {
                (**self).get_reference(bucket, prefix).await
            }
            async fn put_reference(
                &self,
                bucket: &str,
                prefix: &str,
                data: &[u8],
                metadata: &FileMetadata,
            ) -> Result<(), StorageError> {
                (**self).put_reference(bucket, prefix, data, metadata).await
            }
            // MUST forward: these two are the bounded-memory backbone (stream the
            // reference to/from a local file). Falling through to the trait
            // DEFAULT heap-loads the whole baseline on every streaming-delta op.
            async fn get_reference_to_file(
                &self,
                bucket: &str,
                prefix: &str,
                dest: &std::path::Path,
            ) -> Result<u64, StorageError> {
                (**self).get_reference_to_file(bucket, prefix, dest).await
            }
            async fn put_reference_from_file(
                &self,
                bucket: &str,
                prefix: &str,
                source_path: &std::path::Path,
                metadata: &FileMetadata,
            ) -> Result<(), StorageError> {
                (**self)
                    .put_reference_from_file(bucket, prefix, source_path, metadata)
                    .await
            }
            async fn put_passthrough_parts(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
                part_paths: &[std::path::PathBuf],
                metadata: &FileMetadata,
            ) -> Result<(), StorageError> {
                (**self)
                    .put_passthrough_parts(bucket, prefix, filename, part_paths, metadata)
                    .await
            }
            async fn put_reference_metadata(
                &self,
                bucket: &str,
                prefix: &str,
                metadata: &FileMetadata,
            ) -> Result<(), StorageError> {
                (**self)
                    .put_reference_metadata(bucket, prefix, metadata)
                    .await
            }
            async fn get_reference_metadata(
                &self,
                bucket: &str,
                prefix: &str,
            ) -> Result<FileMetadata, StorageError> {
                (**self).get_reference_metadata(bucket, prefix).await
            }
            async fn has_reference(&self, bucket: &str, prefix: &str) -> bool {
                (**self).has_reference(bucket, prefix).await
            }
            async fn delete_reference(
                &self,
                bucket: &str,
                prefix: &str,
            ) -> Result<(), StorageError> {
                (**self).delete_reference(bucket, prefix).await
            }

            async fn get_delta(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
            ) -> Result<Vec<u8>, StorageError> {
                (**self).get_delta(bucket, prefix, filename).await
            }
            async fn put_delta(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
                data: &[u8],
                metadata: &FileMetadata,
            ) -> Result<(), StorageError> {
                (**self)
                    .put_delta(bucket, prefix, filename, data, metadata)
                    .await
            }
            async fn get_delta_metadata(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
            ) -> Result<FileMetadata, StorageError> {
                (**self).get_delta_metadata(bucket, prefix, filename).await
            }
            async fn delete_delta(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
            ) -> Result<(), StorageError> {
                (**self).delete_delta(bucket, prefix, filename).await
            }

            async fn get_passthrough(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
            ) -> Result<Vec<u8>, StorageError> {
                (**self).get_passthrough(bucket, prefix, filename).await
            }
            async fn put_passthrough(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
                data: &[u8],
                metadata: &FileMetadata,
            ) -> Result<(), StorageError> {
                (**self)
                    .put_passthrough(bucket, prefix, filename, data, metadata)
                    .await
            }
            async fn put_passthrough_file(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
                source_path: &Path,
                metadata: &FileMetadata,
            ) -> Result<(), StorageError> {
                (**self)
                    .put_passthrough_file(bucket, prefix, filename, source_path, metadata)
                    .await
            }
            async fn get_passthrough_metadata(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
            ) -> Result<FileMetadata, StorageError> {
                (**self)
                    .get_passthrough_metadata(bucket, prefix, filename)
                    .await
            }
            async fn delete_passthrough(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
            ) -> Result<(), StorageError> {
                (**self).delete_passthrough(bucket, prefix, filename).await
            }

            async fn get_passthrough_stream(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
            ) -> Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError> {
                (**self)
                    .get_passthrough_stream(bucket, prefix, filename)
                    .await
            }

            async fn get_passthrough_stream_range(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
                start: u64,
                end: u64,
            ) -> Result<(BoxStream<'static, Result<Bytes, StorageError>>, u64), StorageError> {
                (**self)
                    .get_passthrough_stream_range(bucket, prefix, filename, start, end)
                    .await
            }

            async fn put_passthrough_chunked(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
                chunks: &[Bytes],
                metadata: &FileMetadata,
            ) -> Result<(), StorageError> {
                (**self)
                    .put_passthrough_chunked(bucket, prefix, filename, chunks, metadata)
                    .await
            }

            async fn create_multipart_upload(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
                metadata: &FileMetadata,
            ) -> Result<MultipartUpload, StorageError> {
                (**self)
                    .create_multipart_upload(bucket, prefix, filename, metadata)
                    .await
            }
            async fn upload_part(
                &self,
                upload: &MultipartUpload,
                prefix: &str,
                filename: &str,
                part_number: i32,
                data: Bytes,
            ) -> Result<UploadedPart, StorageError> {
                (**self)
                    .upload_part(upload, prefix, filename, part_number, data)
                    .await
            }
            async fn complete_multipart_upload(
                &self,
                upload: &MultipartUpload,
                prefix: &str,
                filename: &str,
                parts: &[UploadedPart],
                assembled: &[Bytes],
                metadata: &FileMetadata,
            ) -> Result<String, StorageError> {
                (**self)
                    .complete_multipart_upload(upload, prefix, filename, parts, assembled, metadata)
                    .await
            }
            async fn abort_multipart_upload(
                &self,
                upload: &MultipartUpload,
                prefix: &str,
                filename: &str,
            ) -> Result<(), StorageError> {
                (**self)
                    .abort_multipart_upload(upload, prefix, filename)
                    .await
            }
            fn multipart_storage_label(&self, bucket: &str) -> &'static str {
                (**self).multipart_storage_label(bucket)
            }
            fn supports_native_multipart(&self, bucket: &str) -> bool {
                (**self).supports_native_multipart(bucket)
            }
            fn lite_list_carries_logical_facts(&self, bucket: &str) -> bool {
                (**self).lite_list_carries_logical_facts(bucket)
            }

            async fn scan_deltaspace(
                &self,
                bucket: &str,
                prefix: &str,
            ) -> Result<Vec<FileMetadata>, StorageError> {
                (**self).scan_deltaspace(bucket, prefix).await
            }
            async fn scan_deltaspace_lite(
                &self,
                bucket: &str,
                prefix: &str,
            ) -> Result<LiteScanResult, StorageError> {
                (**self).scan_deltaspace_lite(bucket, prefix).await
            }
            async fn list_deltaspaces(&self, bucket: &str) -> Result<Vec<String>, StorageError> {
                (**self).list_deltaspaces(bucket).await
            }
            async fn total_size(&self, bucket: Option<&str>) -> Result<u64, StorageError> {
                (**self).total_size(bucket).await
            }
            async fn put_directory_marker(
                &self,
                bucket: &str,
                key: &str,
            ) -> Result<(), StorageError> {
                (**self).put_directory_marker(bucket, key).await
            }
            async fn bulk_list_objects(
                &self,
                bucket: &str,
                prefix: &str,
            ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
                (**self).bulk_list_objects(bucket, prefix).await
            }
            async fn enrich_list_metadata(
                &self,
                bucket: &str,
                objects: Vec<(String, FileMetadata)>,
            ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
                (**self).enrich_list_metadata(bucket, objects).await
            }
            async fn list_objects_delegated(
                &self,
                bucket: &str,
                prefix: &str,
                delimiter: &str,
                max_keys: u32,
                continuation_token: Option<&str>,
            ) -> Result<Option<DelegatedListResult>, StorageError> {
                (**self)
                    .list_objects_delegated(bucket, prefix, delimiter, max_keys, continuation_token)
                    .await
            }
        }
    };
}

impl_storage_backend_for_box!();
