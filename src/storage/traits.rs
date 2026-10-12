// SPDX-License-Identifier: BUSL-1.1

//! Storage backend trait definitions

use crate::deltaglider::spool::SpoolBudget;
use crate::storage::list_size_cache::ListedSize;
use crate::types::FileMetadata;
use bytes::Bytes;
use futures::stream::BoxStream;
use std::future::Future;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Bucket listing entry with optional routing-origin metadata.
#[derive(Debug, Clone)]
pub struct BucketListing {
    pub name: String,
    /// `None` for a synthesized placeholder (unreachable backend) — never a
    /// fabricated timestamp.
    pub creation_date: Option<chrono::DateTime<chrono::Utc>>,
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

/// The write precondition on `reference.bin` that fences the cross-instance
/// reference lock: the engine observes the reference under the lock, and every
/// reference write of that hold is conditional on the observation. A writer
/// whose lock lapsed (a peer stole it and wrote) gets a precondition failure
/// instead of overwriting the peer's baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefFence {
    /// No condition (single instance, or a backend that does not fence).
    Unfenced,
    /// The reference must not exist (`If-None-Match: *`).
    Absent,
    /// The reference must still carry this ETag (`If-Match`).
    ETag(String),
}

/// Which stored variant of an object a conditional delete names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectVariant {
    Delta,
    Passthrough,
}

/// One stored object of a deltaspace, for [`StorageBackend::open_object`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredObject<'a> {
    Reference,
    Delta(&'a str),
    Passthrough(&'a str),
}

/// A stream of object bytes.
pub type ByteStream = BoxStream<'static, Result<Bytes, StorageError>>;

/// A deltaspace's `reference.bin`, opened for a copy to a local file
/// ([`StorageBackend::open_reference_source`]): its plaintext size is
/// known BEFORE a byte is written, so the caller reserves spool budget for
/// it from the same read (no HEAD for the size).
pub struct ReferenceSource {
    /// The plaintext size: what [`Self::write_to`] writes.
    pub size: u64,
    /// The reference's stored metadata (raw, markers included).
    pub metadata: FileMetadata,
    content: ReferenceBody,
}

enum ReferenceBody {
    /// Bytes to write (an S3 GET body, a decrypted buffer).
    Stream(ByteStream),
    /// A local file to hardlink, or copy where a hardlink fails (the
    /// filesystem backend).
    Local(PathBuf),
}

impl ReferenceSource {
    /// A source that streams `size` bytes.
    pub fn stream(stream: ByteStream, size: u64, metadata: FileMetadata) -> Self {
        Self {
            size,
            metadata,
            content: ReferenceBody::Stream(stream),
        }
    }

    /// A source that is a local file of `size` bytes.
    pub fn local(path: PathBuf, size: u64, metadata: FileMetadata) -> Self {
        Self {
            size,
            metadata,
            content: ReferenceBody::Local(path),
        }
    }

    /// The whole body in memory (the encrypting wrapper decrypts it).
    pub async fn read_all(self) -> Result<Vec<u8>, StorageError> {
        use futures::TryStreamExt;
        match self.content {
            ReferenceBody::Stream(stream) => {
                let chunks: Vec<Bytes> = stream.try_collect().await?;
                Ok(chunks.concat())
            }
            ReferenceBody::Local(path) => Ok(tokio::fs::read(&path).await?),
        }
    }

    /// Write the reference to `dest` (a hardlink for a local file). Never
    /// writes more than `size` bytes: a body longer than its declared size
    /// fails, so the spool reservation bounds the disk use. Returns the
    /// bytes written.
    pub async fn write_to(self, dest: &Path) -> Result<u64, StorageError> {
        use futures::StreamExt;
        use tokio::io::AsyncWriteExt;
        match self.content {
            ReferenceBody::Local(src) => {
                let _ = tokio::fs::remove_file(dest).await;
                if tokio::fs::hard_link(&src, dest).await.is_err() {
                    tokio::fs::copy(&src, dest).await?;
                }
                Ok(tokio::fs::metadata(dest).await?.len())
            }
            ReferenceBody::Stream(mut stream) => {
                let mut file = tokio::fs::File::create(dest).await?;
                let mut written: u64 = 0;
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk?;
                    written += chunk.len() as u64;
                    if written > self.size {
                        return Err(StorageError::Other(format!(
                            "reference body is longer than its declared {} bytes",
                            self.size
                        )));
                    }
                    file.write_all(&chunk).await?;
                }
                file.flush().await?;
                Ok(written)
            }
        }
    }
}

/// `open_passthrough_range` from the metadata read, then the range read.
pub async fn open_passthrough_range_by_parts<B: StorageBackend + ?Sized>(
    backend: &B,
    bucket: &str,
    prefix: &str,
    filename: &str,
    start: u64,
    end: u64,
) -> Result<(ByteStream, u64, FileMetadata), StorageError> {
    let metadata = backend
        .get_passthrough_metadata(bucket, prefix, filename)
        .await?;
    let (stream, len) = backend
        .get_passthrough_stream_range(bucket, prefix, filename, start, end)
        .await?;
    Ok((stream, len, metadata))
}

/// `open_reference_source` from the metadata read and the data read, for a
/// test double whose data read does not carry the metadata.
pub async fn open_reference_source_by_parts<B: StorageBackend + ?Sized>(
    backend: &B,
    bucket: &str,
    prefix: &str,
) -> Result<ReferenceSource, StorageError> {
    let metadata = backend.get_reference_metadata(bucket, prefix).await?;
    let data = backend.get_reference(bucket, prefix).await?;
    let size = data.len() as u64;
    let stream: ByteStream = Box::pin(futures::stream::once(async move { Ok(Bytes::from(data)) }));
    Ok(ReferenceSource::stream(stream, size, metadata))
}

/// Where one stored object (a delta or a passthrough file) is, for
/// [`StorageBackend::copy_within`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectAt<'a> {
    pub bucket: &'a str,
    pub prefix: &'a str,
    pub filename: &'a str,
}

/// What [`StorageBackend::copy_within`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyOutcome {
    /// The backend copied the object; no byte went through the proxy.
    Copied,
    /// The backend cannot copy this object inside itself (another backend
    /// or encryption key at the destination, a provider without
    /// CopyObject). Nothing was written: the caller copies the bytes.
    Unsupported,
}

/// Pure: does `current` (the source as the backend holds it now) still
/// hold the generation that `pin` (the caller's read of it) saw? The
/// logical content must match (`DeltaGliderEngine::same_generation`),
/// and so must the created-at and, for a delta, the reference and the
/// delta size: a delta re-encoded against another reference has the same
/// content but other bytes. Copying such bytes under the pin's metadata
/// would make the destination unreadable.
pub fn same_stored_generation(pin: &FileMetadata, current: &FileMetadata) -> bool {
    use crate::types::StorageInfo;
    if !crate::deltaglider::DynEngine::same_generation(pin, current)
        || pin.created_at != current.created_at
    {
        return false;
    }
    match (&pin.storage_info, &current.storage_info) {
        (
            StorageInfo::Delta {
                ref_sha256: a,
                delta_size: da,
                ..
            },
            StorageInfo::Delta {
                ref_sha256: b,
                delta_size: db,
                ..
            },
        ) => a == b && da == db,
        (StorageInfo::Passthrough, StorageInfo::Passthrough) => true,
        _ => false,
    }
}

/// One fenced write to `reference.bin` (see
/// [`StorageBackend::write_reference_fenced`]).
#[derive(Debug, Clone, Copy)]
pub enum RefWrite<'a> {
    /// The body is `Bytes`: a write (and each retry of it) shares the
    /// caller's buffer instead of copying it.
    Put {
        data: &'a Bytes,
        metadata: &'a FileMetadata,
    },
    PutFile {
        path: &'a Path,
        metadata: &'a FileMetadata,
    },
    Metadata {
        metadata: &'a FileMetadata,
    },
    Delete,
}

/// The message of a fenced reference write that lost its precondition.
pub fn reference_fence_lost(bucket: &str, prefix: &str) -> StorageError {
    StorageError::Throttled(format!(
        "reference.bin of {bucket}/{prefix} changed after this request took the \
         deltaspace lock (another instance wrote it); retry the request"
    ))
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

    /// The backend refused an object-level request (403), or sends every
    /// request to another endpoint (301 PermanentRedirect). Classified where
    /// the status is known (`S3Backend::classify_s3_error`), so no caller
    /// reads the status out of the text. Same Display and the same wire
    /// answer as `S3` (500, sanitised): only the classification is new.
    #[error("S3 error: {0}")]
    AccessDenied(String),

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

    /// The key cannot be stored or read on this backend as written (for
    /// example a `.` segment on the filesystem). Maps to 400 InvalidArgument.
    #[error("Invalid key: {0}")]
    InvalidKey(String),

    /// The key is longer than this backend can store (the filesystem limits
    /// one path segment to 255 bytes, `.delta` included; S3 limits a key to
    /// 1024 bytes). Maps to 400 KeyTooLongError, which clients do not retry.
    #[error("KeyTooLong: {0}")]
    KeyTooLong(String),

    /// The object's metadata is larger than this backend can store (the
    /// filesystem keeps it in one xattr, and JSON escaping can double user
    /// metadata that is within the S3 limit). Maps to 400 MetadataTooLarge.
    #[error("MetadataTooLarge: {0}")]
    MetadataTooLarge(String),

    /// The backend did not answer: the request timed out, or the
    /// connection failed. Maps to 503 ServiceUnavailable.
    #[error("Backend unavailable (timed out or unreachable): {0}")]
    Unavailable(String),

    /// The backend answered with a fault that a retry can clear: a 5xx
    /// other than 503 (500, 502, 504), or the response body broke off
    /// while it was read. Same Display as `S3`. Maps to 503
    /// ServiceUnavailable with `Retry-After`: the client gets a generic
    /// text, and the cause goes to the log.
    #[error("S3 error: {0}")]
    Transient(String),

    /// Another writer holds the resource past the wait (the cross-instance
    /// reference lock). A retry can succeed. Same Display and wire answer
    /// (500) as `Other`.
    #[error("Storage error: {0}")]
    Contended(String),

    /// The object is not the generation the caller pinned: it changed
    /// after the caller's HEAD (a copy source overwritten during the copy).
    /// Same Display and wire answer (500) as `Other`.
    #[error("Storage error: {0}")]
    PreconditionFailed(String),

    /// The backend refuses writes because a storage cap or quota is used
    /// up (507, `QuotaExceeded`, B2 `cap_exceeded`). Same Display and wire
    /// answer (500) as `S3`.
    #[error("S3 error: {0}")]
    QuotaExceeded(String),

    /// The requested byte range starts past the object's end (the caller
    /// resolved it against a stale size). Maps to 416 InvalidRange.
    #[error("Invalid range: {0}")]
    InvalidRange(String),

    #[error("Storage error: {0}")]
    Other(String),
}

/// `reference_fence` of a backend that does not fence: existence only.
pub async fn unfenced_reference_fence<B: StorageBackend + ?Sized>(
    backend: &B,
    bucket: &str,
    prefix: &str,
) -> Result<RefFence, StorageError> {
    Ok(if backend.has_reference(bucket, prefix).await? {
        RefFence::ETag(String::new())
    } else {
        RefFence::Absent
    })
}

/// `reference_fence_with_metadata` of a backend that does not fence: the
/// metadata read, and the fence [`unfenced_reference_fence`] gives
/// (existence only).
pub async fn unfenced_reference_fence_with_metadata<B: StorageBackend + ?Sized>(
    backend: &B,
    bucket: &str,
    prefix: &str,
) -> Result<(RefFence, Option<FileMetadata>), StorageError> {
    match backend.get_reference_metadata(bucket, prefix).await {
        Ok(meta) => Ok((RefFence::ETag(String::new()), Some(meta))),
        Err(StorageError::NotFound(_)) => Ok((RefFence::Absent, None)),
        Err(e) => Err(e),
    }
}

/// `write_reference_fenced` of a backend that does not fence: the plain
/// write, and `Unfenced`.
pub async fn unfenced_reference_write<B: StorageBackend + ?Sized>(
    backend: &B,
    bucket: &str,
    prefix: &str,
    op: RefWrite<'_>,
    proof: &crate::deltaglider::RefWriteProof,
) -> Result<RefFence, StorageError> {
    match op {
        RefWrite::Put { data, metadata } => {
            backend
                .put_reference(bucket, prefix, data.clone(), metadata, proof)
                .await?
        }
        RefWrite::PutFile { path, metadata } => {
            backend
                .put_reference_from_file(bucket, prefix, path, metadata, proof)
                .await?
        }
        RefWrite::Metadata { metadata } => {
            backend
                .put_reference_metadata(bucket, prefix, metadata, proof)
                .await?
        }
        RefWrite::Delete => backend.delete_reference(bucket, prefix, proof).await?,
    }
    Ok(RefFence::Unfenced)
}

/// `open_object` from two reads: the metadata read, then the data read. For
/// a backend (or a test double) whose data read does not carry the
/// metadata.
pub async fn open_object_by_parts<B: StorageBackend + ?Sized>(
    backend: &B,
    bucket: &str,
    prefix: &str,
    object: StoredObject<'_>,
) -> Result<(ByteStream, FileMetadata), StorageError> {
    let once = |data: Vec<u8>| -> ByteStream {
        Box::pin(futures::stream::once(async move { Ok(Bytes::from(data)) }))
    };
    Ok(match object {
        StoredObject::Reference => {
            let meta = backend.get_reference_metadata(bucket, prefix).await?;
            (once(backend.get_reference(bucket, prefix).await?), meta)
        }
        StoredObject::Delta(f) => {
            let meta = backend.get_delta_metadata(bucket, prefix, f).await?;
            (once(backend.get_delta(bucket, prefix, f).await?), meta)
        }
        StoredObject::Passthrough(f) => {
            let meta = backend.get_passthrough_metadata(bucket, prefix, f).await?;
            (
                backend.get_passthrough_stream(bucket, prefix, f).await?,
                meta,
            )
        }
    })
}

/// Pure: clamp an inclusive byte range `start..=end` to an object of `len`
/// bytes. A range that starts at or past the end, or with `start > end`, is
/// [`StorageError::InvalidRange`]; an `end` past the object is clamped.
/// Every `get_passthrough_stream_range` resolves its range through it.
pub fn clamp_range(start: u64, end: u64, len: u64) -> Result<(u64, u64), StorageError> {
    let clamped = end.min(len.saturating_sub(1));
    if len == 0 || start > clamped {
        return Err(StorageError::InvalidRange(format!(
            "bytes {start}-{end} of an object of {len} bytes"
        )));
    }
    Ok((start, clamped))
}

/// Pure: whether an I/O error says that a file name or path is too long
/// (`ENAMETOOLONG`). The filesystem backend's keys become path segments, so
/// this is a client error, never a 500.
pub fn io_error_is_name_too_long(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(libc::ENAMETOOLONG) || e.kind() == std::io::ErrorKind::InvalidFilename
}

/// Pure: whether an I/O error says that a path component is a directory
/// where a file must be, or the reverse (`EISDIR`, `ENOTDIR`). The filesystem
/// backend stores key `a` as a file and key `a/b` under a directory `a`, so
/// the two keys cannot coexist there.
pub fn io_error_is_path_type_conflict(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EISDIR) | Some(libc::ENOTDIR))
        || matches!(
            e.kind(),
            std::io::ErrorKind::IsADirectory | std::io::ErrorKind::NotADirectory
        )
}

/// Abstract storage backend for S3-like object storage
/// Uses per-file metadata following DeltaGlider schema (xattr on filesystem, S3 user metadata headers on S3)
///
/// Async methods are declared `-> impl Future<Output = _> + Send` so every
/// caller can rely on `Send` futures; impls write plain `async fn`. Dynamic
/// dispatch goes through [`DynStorageBackend`] (generated by `dynosaur`, which
/// boxes the futures), e.g. `Box<DynStorageBackend<'static>>`; static dispatch
/// does not box.
///
/// All methods take a `bucket` parameter which maps to a real storage bucket
/// (S3 bucket or filesystem directory).
#[dynosaur::dynosaur(pub DynStorageBackend = dyn(box) StorageBackend)]
pub trait StorageBackend: Send + Sync {
    // === Bucket operations ===

    /// Create a new bucket
    fn create_bucket(&self, bucket: &str) -> impl Future<Output = Result<(), StorageError>> + Send;

    /// Ensure a bucket DECLARED in config exists. Filesystem backends `mkdir`
    /// the bucket dir (#63); S3 backends create a missing bucket (browser
    /// review #24: both kinds of backend behave the same, so a declared
    /// bucket works on its first write). A bucket is never created implicitly
    /// on the WRITE path — that's a deliberate refusal, see
    /// `filesystem::require_bucket_exists`. Called once at startup for each
    /// `storage.buckets` entry unless `DGP_BOOT_CREATE_DECLARED_BUCKETS=false`;
    /// idempotent. The default no-op is for wrappers and test doubles.
    fn ensure_declared_bucket(
        &self,
        _bucket: &str,
    ) -> impl Future<Output = Result<(), StorageError>> + Send {
        async move { Ok(()) }
    }

    /// Delete a bucket (must be empty)
    fn delete_bucket(&self, bucket: &str) -> impl Future<Output = Result<(), StorageError>> + Send;

    /// List all buckets
    fn list_buckets(&self) -> impl Future<Output = Result<Vec<String>, StorageError>> + Send;

    /// List all buckets with their creation dates.
    /// Default implementation falls back to `list_buckets()` with current time.
    fn list_buckets_with_dates(
        &self,
    ) -> impl Future<Output = Result<Vec<(String, chrono::DateTime<chrono::Utc>)>, StorageError>> + Send
    {
        async move {
            let names = self.list_buckets().await?;
            Ok(names.into_iter().map(|n| (n, chrono::Utc::now())).collect())
        }
    }

    /// List buckets with optional backend-origin metadata.
    ///
    /// Concrete single backends usually don't know their configured name, so
    /// the default leaves origin fields empty. `RoutingBackend` overrides this
    /// to preserve the backend that produced each bucket.
    fn list_bucket_origins(
        &self,
    ) -> impl Future<Output = Result<Vec<BucketListing>, StorageError>> + Send {
        async move {
            Ok(self
                .list_buckets_with_dates()
                .await?
                .into_iter()
                .map(|(name, creation_date)| BucketListing {
                    name,
                    creation_date: Some(creation_date),
                    backend_name: None,
                    real_bucket: None,
                    unavailable: None,
                })
                .collect())
        }
    }

    /// Check if a bucket exists
    fn head_bucket(&self, bucket: &str) -> impl Future<Output = Result<bool, StorageError>> + Send;

    // === Reference file operations ===

    /// Get the reference file for a deltaspace
    fn get_reference(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> impl Future<Output = Result<Vec<u8>, StorageError>> + Send;

    /// Materialise the reference file at `dest` as a seekable local file, WITHOUT
    /// heap-loading it — the streaming codec (`encode_from_reader` /
    /// `decode_to_writer`) needs the reference as a file xdelta3 can mmap, and a
    /// multi-GB reference must never be buffered in RAM (adversarial blocker 10).
    ///
    /// Returns the number of bytes written. The default impl falls back to
    /// `get_reference` + write (fine for small references / backends that don't
    /// override); the filesystem backend hardlinks (near-zero) and the S3 backend
    /// streams the GET body to the file.
    fn get_reference_to_file(
        &self,
        bucket: &str,
        prefix: &str,
        dest: &std::path::Path,
    ) -> impl Future<Output = Result<u64, StorageError>> + Send {
        async move {
            let data = self.get_reference(bucket, prefix).await?;
            tokio::fs::write(dest, &data).await?;
            Ok(data.len() as u64)
        }
    }

    /// Open the reference for a copy to a local file, with its plaintext
    /// size from the SAME read (S3: the GET response's Content-Length; the
    /// encrypting wrapper: the decrypted length; filesystem: the file,
    /// which [`ReferenceSource::write_to`] hardlinks). The caller reserves
    /// spool budget for that size before it writes, with no HEAD. A missing
    /// reference is `NotFound`. No default: a wrapper that forgot it would
    /// hand out the inner backend's bytes without its own transform.
    fn open_reference_source(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> impl Future<Output = Result<ReferenceSource, StorageError>> + Send;

    /// Store a reference file with its metadata
    fn put_reference(
        &self,
        bucket: &str,
        prefix: &str,
        data: Bytes,
        metadata: &FileMetadata,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> impl Future<Output = Result<(), StorageError>> + Send;

    /// Store a reference whose data is on a local file, WITHOUT heap-loading it
    /// (review M1.4: the streaming baseline path must not read a multi-GB first
    /// member into RAM). Default reads the file then delegates to `put_reference`
    /// (correct for any backend); the filesystem backend hardlinks and S3 streams
    /// the upload from the file — mirrors `get_reference_to_file`.
    fn put_reference_from_file(
        &self,
        bucket: &str,
        prefix: &str,
        source_path: &std::path::Path,
        metadata: &FileMetadata,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> impl Future<Output = Result<(), StorageError>> + Send {
        async move {
            let data = tokio::fs::read(source_path).await?;
            self.put_reference(bucket, prefix, Bytes::from(data), metadata, proof)
                .await
        }
    }

    /// Store/update reference metadata without rewriting reference data.
    fn put_reference_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        metadata: &FileMetadata,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> impl Future<Output = Result<(), StorageError>> + Send;

    /// [`Self::get_passthrough_metadata`], and the [`RewriteBasis`] that a
    /// later [`Self::put_passthrough_metadata`] of the same object takes
    /// instead of reading the object again. One request (S3: one HEAD).
    fn get_passthrough_metadata_for_rewrite(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> impl Future<Output = Result<(FileMetadata, RewriteBasis), StorageError>> + Send;

    /// Update a passthrough object's DG metadata IN PLACE, without
    /// transferring the object's bytes through the proxy (S3: server-side
    /// self-copy with `MetadataDirective: REPLACE`; filesystem: xattr
    /// rewrite). Same (bucket, prefix, filename) addressing as
    /// `get_passthrough_metadata`.
    ///
    /// `basis`: what [`Self::get_passthrough_metadata_for_rewrite`] read of
    /// the object, or [`RewriteBasis::none`]. With a read, the rewrite
    /// sends no read of its own, and on S3 it is fenced on the read's ETag
    /// (`x-amz-copy-source-if-match`): an object that changed since the
    /// read fails the rewrite instead of getting metadata of another
    /// version.
    ///
    /// Used by the metadata-backfill maintenance job. Backends that cannot
    /// do this keep the default `Other` error; the job records it as a
    /// per-object failure instead of aborting.
    fn put_passthrough_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        metadata: &FileMetadata,
        basis: &RewriteBasis,
    ) -> impl Future<Output = Result<(), StorageError>> + Send {
        async move {
            let _ = (bucket, prefix, filename, metadata, basis);
            Err(StorageError::Other(
                "put_passthrough_metadata is not supported by this backend".to_string(),
            ))
        }
    }

    /// Get reference file metadata
    fn get_reference_metadata(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> impl Future<Output = Result<FileMetadata, StorageError>> + Send;

    /// Check if a reference exists for this deltaspace.
    ///
    /// `Ok(true)` = present, `Ok(false)` = genuinely absent (a real 404),
    /// `Err(_)` = the backend could not answer (503/timeout/5xx). Callers on
    /// the WRITE path MUST propagate the error — treating "couldn't check" as
    /// "absent" would overwrite a live `reference.bin` and orphan every
    /// sibling delta (the has_reference transient-error corruption class).
    fn has_reference(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> impl Future<Output = Result<bool, StorageError>> + Send;

    /// Delete a reference file and its metadata
    fn delete_reference(
        &self,
        bucket: &str,
        prefix: &str,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> impl Future<Output = Result<(), StorageError>> + Send;

    /// Make durable every object write that this backend deferred (see
    /// [`crate::storage::with_deferred_fsync`]). A backend whose writes are
    /// durable when they return (S3) answers `Ok` at once. No default:
    /// every wrapper MUST forward it.
    fn flush_pending(&self) -> impl Future<Output = Result<(), StorageError>> + Send;

    /// The fence for the reference as it is now: `ETag(..)` when it exists,
    /// `Absent` when it does not. Same error contract as `has_reference`.
    /// No default: a wrapper that forgot it would drop the fence below it.
    fn reference_fence(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> impl Future<Output = Result<RefFence, StorageError>> + Send;

    /// [`Self::reference_fence`] and the reference's metadata, from ONE
    /// read (one HEAD on S3): `(Absent, None)` when it does not exist. The
    /// cross-instance lock reads both at acquire; two HEADs could also
    /// see two different objects. No default: a wrapper that forgot it
    /// would drop the fence below it.
    fn reference_fence_with_metadata(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> impl Future<Output = Result<(RefFence, Option<FileMetadata>), StorageError>> + Send;

    /// Write `reference.bin` only if `fence` still holds, and return the
    /// fence after the write. A lost precondition is
    /// [`reference_fence_lost`] (retryable), never an overwrite. No default:
    /// every wrapper must forward it, or the fence silently disappears
    /// below it; a backend that does not fence says so in its own impl.
    fn write_reference_fenced(
        &self,
        bucket: &str,
        prefix: &str,
        op: RefWrite<'_>,
        fence: &RefFence,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> impl Future<Output = Result<RefFence, StorageError>> + Send;

    /// Copy the stored object `src` (form `variant`) to `dst` inside this
    /// backend: the bytes never pass through the proxy (S3: CopyObject, or
    /// UploadPartCopy above 5 GiB; filesystem: a kernel file copy). The
    /// destination gets `metadata` (the caller's, without at-rest
    /// encryption markers: a wrapper restates the markers that it owns).
    ///
    /// `source` is the caller's read of the source, markers included: the
    /// generation pin. A source that no longer holds that generation
    /// ([`same_stored_generation`]) is `PreconditionFailed`, and nothing is
    /// written. [`CopyOutcome::Unsupported`] writes nothing either. Not
    /// for `reference.bin`: a reference write needs the lock proof.
    ///
    /// No default: a wrapper that forgot it would copy below its own
    /// transform (an encrypting wrapper with another key, a routing
    /// wrapper across two backends).
    fn copy_within(
        &self,
        variant: ObjectVariant,
        src: ObjectAt<'_>,
        dst: ObjectAt<'_>,
        source: &FileMetadata,
        metadata: &FileMetadata,
    ) -> impl Future<Output = Result<CopyOutcome, StorageError>> + Send;

    // === Delta file operations ===

    /// Get a delta file
    fn get_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> impl Future<Output = Result<Vec<u8>, StorageError>> + Send;

    /// Store a delta file with its metadata
    fn put_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: Bytes,
        metadata: &FileMetadata,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> impl Future<Output = Result<(), StorageError>> + Send;

    /// Get delta file metadata
    fn get_delta_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> impl Future<Output = Result<FileMetadata, StorageError>> + Send;

    /// Delete a delta file and its metadata
    fn delete_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> impl Future<Output = Result<(), StorageError>> + Send;

    // === Passthrough file operations (stored as-is with original filename) ===

    /// Get a passthrough (non-delta) file
    fn get_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> impl Future<Output = Result<Vec<u8>, StorageError>> + Send;

    /// Store a passthrough (non-delta) file with its metadata
    fn put_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: Bytes,
        metadata: &FileMetadata,
    ) -> impl Future<Output = Result<(), StorageError>> + Send;

    /// Store a passthrough file from an on-disk source path.
    /// Default implementation reads the full file and delegates to `put_passthrough`.
    ///
    /// `spool` is where the write puts any temp file of its own (the
    /// encrypting wrapper's ciphertext). It never waits for budget: the
    /// caller reserves up front with [`Self::file_put_spool_bytes`].
    fn put_passthrough_file(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        source_path: &Path,
        metadata: &FileMetadata,
        _spool: SpoolBudget<'_>,
    ) -> impl Future<Output = Result<(), StorageError>> + Send {
        async move {
            let data = tokio::fs::read(source_path).await?;
            self.put_passthrough(bucket, prefix, filename, Bytes::from(data), metadata)
                .await
        }
    }

    /// Store a passthrough file from ordered relay part paths.
    /// Default implementation materializes an intermediate file in-memory.
    /// `spool`: as for [`Self::put_passthrough_file`].
    fn put_passthrough_parts(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        part_paths: &[PathBuf],
        metadata: &FileMetadata,
        _spool: SpoolBudget<'_>,
    ) -> impl Future<Output = Result<(), StorageError>> + Send {
        async move {
            let mut assembled = Vec::new();
            for path in part_paths {
                let part = tokio::fs::read(path).await?;
                assembled.extend_from_slice(&part);
            }
            self.put_passthrough(bucket, prefix, filename, Bytes::from(assembled), metadata)
                .await
        }
    }

    /// Spool bytes that `put_passthrough_file` (`parts = false`) or
    /// `put_passthrough_parts` (`parts = true`) needs for its own temp files
    /// when it stores `bytes` into `bucket`. The engine reserves this much
    /// before it takes the deltaspace lock. Default: none.
    fn file_put_spool_bytes(
        &self,
        _bucket: &str,
        _bytes: u64,
        _parts: bool,
    ) -> impl Future<Output = u64> + Send {
        async move { 0 }
    }

    /// Get passthrough file metadata
    fn get_passthrough_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> impl Future<Output = Result<FileMetadata, StorageError>> + Send;

    /// Delete a passthrough (non-delta) file and its metadata
    fn delete_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> impl Future<Output = Result<(), StorageError>> + Send;

    /// The stored version (the backend's ETag) of one variant, for a later
    /// [`Self::delete_variant_if`]. `Ok(None)`: this backend has no
    /// conditional delete; the engine's in-process deltaspace lock is the
    /// only guard (enough single-instance: every PUT takes it). A missing
    /// object is `Err(NotFound)`.
    fn variant_version(
        &self,
        _bucket: &str,
        _prefix: &str,
        _filename: &str,
        _variant: ObjectVariant,
    ) -> impl Future<Output = Result<Option<String>, StorageError>> + Send {
        async move { Ok(None) }
    }

    /// Delete one variant only while it is still `version` (from
    /// [`Self::variant_version`]). `Ok(false)`: it changed or is gone, and
    /// nothing is deleted. The default deletes unconditionally.
    fn delete_variant_if(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        variant: ObjectVariant,
        _version: &str,
    ) -> impl Future<Output = Result<bool, StorageError>> + Send {
        async move {
            match variant {
                ObjectVariant::Delta => self.delete_delta(bucket, prefix, filename).await?,
                ObjectVariant::Passthrough => {
                    self.delete_passthrough(bucket, prefix, filename).await?
                }
            }
            Ok(true)
        }
    }

    // === Streaming operations ===

    /// Stream a passthrough file's contents without buffering the entire file in
    /// memory. REQUIRED (no default) — a buffering default would silently defeat
    /// the memory bound for large objects on a backend that forgot to override
    /// it. The type system enforces every backend provides a real streaming read
    /// (this used to be a buffering default guarded by a grep conformance test).
    fn get_passthrough_stream(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> impl Future<Output = Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError>> + Send;

    /// Stream one stored object together with the metadata stored with it,
    /// both from ONE read of the backend (S3: the GET response's headers;
    /// filesystem: the file and its xattr). The metadata is the raw stored
    /// one, markers included, so the encrypting wrapper decides
    /// encrypted-or-plaintext from it and sends no separate HEAD
    /// (storage-7). A missing object is `NotFound`. No default: a wrapper
    /// that forgot it would read the object through the inner backend
    /// without its own transform.
    fn open_object(
        &self,
        bucket: &str,
        prefix: &str,
        object: StoredObject<'_>,
    ) -> impl Future<Output = Result<(ByteStream, FileMetadata), StorageError>> + Send;

    /// A byte range of a passthrough object and the metadata stored with
    /// it, from ONE read where the backend can (S3: the ranged GET, whose
    /// Content-Range gives the object's total size). Returns `(stream,
    /// content_length, metadata)`. An `end` past the object is clamped; a
    /// `start` at or past it is [`StorageError::InvalidRange`]; a missing
    /// object is `NotFound`. No default: a wrapper that forgot it would
    /// read the inner backend's bytes without its own transform.
    fn open_passthrough_range(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        start: u64,
        end: u64,
    ) -> impl Future<Output = Result<(ByteStream, u64, FileMetadata), StorageError>> + Send;

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
    fn get_passthrough_stream_range(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        start: u64,
        end: u64, // inclusive
    ) -> impl Future<
        Output = Result<(BoxStream<'static, Result<Bytes, StorageError>>, u64), StorageError>,
    > + Send;

    /// Store a passthrough file from pre-split chunks without assembling into a contiguous buffer.
    /// Default implementation collects chunks and delegates to `put_passthrough()`.
    fn put_passthrough_chunked(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        chunks: &[Bytes],
        metadata: &FileMetadata,
    ) -> impl Future<Output = Result<(), StorageError>> + Send {
        async move {
            let total_len: usize = chunks.iter().map(|c| c.len()).sum();
            let mut buf = Vec::with_capacity(total_len);
            for chunk in chunks {
                buf.extend_from_slice(chunk);
            }
            self.put_passthrough(bucket, prefix, filename, Bytes::from(buf), metadata)
                .await
        }
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
    fn create_multipart_upload(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        metadata: &FileMetadata,
    ) -> impl Future<Output = Result<MultipartUpload, StorageError>> + Send {
        async move {
            let _ = (prefix, filename, metadata);
            Ok(MultipartUpload {
                bucket: bucket.to_string(),
                upload_id: "buffered-default".to_string(),
                native: false,
                backend: None,
            })
        }
    }

    /// Upload one part (1-indexed `part_number`), returning its ETag.
    /// The default is a no-op that derives an ETag from the part bytes;
    /// the caller keeps the bytes for `complete` because `native: false`.
    fn upload_part(
        &self,
        upload: &MultipartUpload,
        prefix: &str,
        filename: &str,
        part_number: i32,
        data: Bytes,
    ) -> impl Future<Output = Result<UploadedPart, StorageError>> + Send {
        async move {
            // The buffering default ignores part ETags in `complete`, so a
            // placeholder is sufficient; the caller retains `data` itself.
            let _ = (upload, prefix, filename, &data);
            Ok(UploadedPart {
                part_number,
                etag: String::new(),
            })
        }
    }

    /// Complete a multipart upload. `assembled` carries every part's bytes
    /// in order (populated by the caller for `native: false` backends);
    /// native backends ignore it and finalize from the part ETags. Returns
    /// the final multipart ETag.
    fn complete_multipart_upload(
        &self,
        upload: &MultipartUpload,
        prefix: &str,
        filename: &str,
        parts: &[UploadedPart],
        assembled: &[Bytes],
        metadata: &FileMetadata,
    ) -> impl Future<Output = Result<String, StorageError>> + Send {
        async move {
            let _ = parts;
            self.put_passthrough_chunked(&upload.bucket, prefix, filename, assembled, metadata)
                .await?;
            Ok(format!("\"{}\"", metadata.md5))
        }
    }

    /// Abort an in-progress multipart upload (best-effort cleanup).
    fn abort_multipart_upload(
        &self,
        upload: &MultipartUpload,
        prefix: &str,
        filename: &str,
    ) -> impl Future<Output = Result<(), StorageError>> + Send {
        async move {
            let _ = (upload, prefix, filename);
            Ok(())
        }
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

    /// Name of the backend this bucket's requests go to, when the backend
    /// knows without I/O (an explicit route, or where it last FOUND an
    /// unrouted bucket). `None` = unknown here; the caller falls back to
    /// `Config::effective_backend_for_bucket`. Only `RoutingBackend` knows.
    fn resolved_backend_name(&self, _bucket: &str) -> Option<String> {
        None
    }

    /// A name for the STORAGE behind `bucket`: two bucket names that store
    /// into the same real bucket (policy aliases) get the same identity, so
    /// the engine's deltaspace locks serialise them. An unrouted bucket is
    /// its own name.
    fn storage_identity(&self, bucket: &str) -> String {
        bucket.to_string()
    }

    /// The identity that the previous release used for `bucket`, when it
    /// differs from [`Self::storage_identity`]. The reference lock takes the
    /// lock object of both during this release, so a node of the previous
    /// release and a node of this one exclude each other in a rolling
    /// upgrade. Remove in the next release (CHANGELOG).
    fn previous_storage_identity(&self, _bucket: &str) -> Option<String> {
        None
    }

    // === Scanning operations ===

    /// Scan a deltaspace directory and return all file metadata
    /// This replaces the centralized index - state is derived from files
    fn scan_deltaspace(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> impl Future<Output = Result<Vec<FileMetadata>, StorageError>> + Send;

    /// Cheap variant of [`scan_deltaspace`](Self::scan_deltaspace) for diagnostics-only callers
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
    fn scan_deltaspace_lite(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> impl Future<Output = Result<LiteScanResult, StorageError>> + Send {
        async move {
            // Default: HEAD-based path supplies real originals, so the
            // caller can trust file_size on deltas.
            let metadata = self.scan_deltaspace(bucket, prefix).await?;
            Ok(LiteScanResult {
                metadata,
                originals_estimated: false,
            })
        }
    }

    /// True when the deltaspace `prefix` holds reference.bin and no
    /// object at its own level: the reference can be reclaimed. A
    /// sub-folder is another deltaspace (its deltas use its own
    /// reference.bin), so its objects do not count. The read stops at the
    /// first object: on S3 one LIST page of `prefix/` with delimiter `/`
    /// answers it in the common case, never a listing of a sub-folder or
    /// of the bucket. The default reads the whole level through
    /// [`Self::scan_deltaspace_lite`].
    fn holds_only_reference(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> impl Future<Output = Result<bool, StorageError>> + Send {
        async move {
            let listed = self.scan_deltaspace_lite(bucket, prefix).await?;
            let mut reference = false;
            for m in &listed.metadata {
                match m.storage_info {
                    crate::types::StorageInfo::Reference { .. } => reference = true,
                    _ => return Ok(false),
                }
            }
            Ok(reference)
        }
    }

    /// List all deltaspace prefixes within a bucket: every directory, with
    /// or without a reference. A caller that wants the references uses
    /// [`Self::list_reference_prefixes`].
    fn list_deltaspaces(
        &self,
        bucket: &str,
    ) -> impl Future<Output = Result<Vec<String>, StorageError>> + Send;

    /// The deltaspaces that hold a `reference.bin`: `scope` itself and every
    /// deltaspace below it (`scope/...`), in no order. `scope` has no
    /// trailing `/`; `""` is the whole bucket. One listing of the scope
    /// finds them, with no request per deltaspace: a folder of screenshots
    /// costs no HEAD. No default: every wrapper must forward it.
    fn list_reference_prefixes(
        &self,
        bucket: &str,
        scope: &str,
    ) -> impl Future<Output = Result<Vec<String>, StorageError>> + Send;

    /// Get total storage size used (for metrics), optionally scoped to a bucket
    fn total_size(
        &self,
        bucket: Option<&str>,
    ) -> impl Future<Output = Result<u64, StorageError>> + Send;

    /// Store a zero-byte S3 directory marker (key ending with '/').
    /// Used by Cyberduck, AWS Console, etc. to create "folders".
    /// Default: no-op (directories are implicit in S3).
    fn put_directory_marker(
        &self,
        _bucket: &str,
        _key: &str,
    ) -> impl Future<Output = Result<(), StorageError>> + Send {
        async move { Ok(()) }
    }

    /// List all objects in a bucket matching a prefix, in a single pass.
    /// Returns `(user_visible_key, FileMetadata)` pairs — references are excluded,
    /// directory markers are included. This replaces the three-step
    /// list_deltaspaces → scan_deltaspace × N → list_directory_markers dance.
    fn bulk_list_objects(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> impl Future<Output = Result<Vec<(String, FileMetadata)>, StorageError>> + Send;

    /// [`Self::bulk_list_objects`] plus the delta baselines the same listing
    /// saw, as `(stored key, stored size)` (for example `fw/v1/reference.bin`).
    /// Callers that report stored bytes (the folder-size scan) get the
    /// baselines at no extra request, from the same prefix listing, so objects
    /// and baselines always follow the same prefix rule. The default lists no
    /// baselines; every production backend overrides it.
    ///
    /// `start_after` and `max_listed` page a long listing: list the keys
    /// after `start_after`, and stop once `max_listed` keys are listed
    /// (objects, baselines and folder markers all count; the S3 backend
    /// sends no request past that number). The answer's `next_start_after`
    /// is where the next page starts, `None` at the end of the prefix. A
    /// backend that lists the whole prefix in one pass (filesystem) may
    /// ignore both and answer everything, so a caller with a cap checks the
    /// number of keys it got.
    fn bulk_list_objects_with_baselines(
        &self,
        bucket: &str,
        prefix: &str,
        _start_after: Option<&str>,
        _max_listed: Option<usize>,
    ) -> impl Future<Output = Result<BulkListing, StorageError>> + Send {
        async move {
            Ok(BulkListing {
                objects: self.bulk_list_objects(bucket, prefix).await?,
                ..Default::default()
            })
        }
    }

    /// Replace each listed entry's STORED size and ETag with the logical ones
    /// for exactly that stored object: from this process's
    /// [`crate::storage::list_size_cache`], else (S3) from the durable
    /// [`crate::storage::listing_facts`], read with one LIST per page. Never
    /// fails: an entry without known facts keeps its stored size. Returns,
    /// per entry, how much of its size is known. `created_at` (the listed
    /// LastModified) is never changed.
    ///
    /// `passthrough_may_differ`: a passthrough entry may be stored in another
    /// form (the proxy-encryption wrapper sets it), so look its facts up too.
    ///
    /// The default suits a backend whose listing already carries logical
    /// sizes (filesystem: the xattrs are read during the listing): only a
    /// delta stub is `StoredOnly`.
    fn resolve_listed_sizes(
        &self,
        _bucket: &str,
        objects: &mut [(String, FileMetadata)],
        _passthrough_may_differ: bool,
    ) -> impl Future<Output = Vec<ListedSize>> + Send {
        async move {
            objects
                .iter()
                .map(|(_, m)| {
                    if m.is_unresolved_delta_stub() {
                        ListedSize::StoredOnly
                    } else {
                        ListedSize::Listed
                    }
                })
                .collect()
        }
    }

    /// Enrich listed objects with full metadata from HEAD calls.
    /// Used by the `metadata=true` MinIO ListObjectsV2 extension.
    ///
    /// The default implementation returns objects unchanged (suitable for
    /// backends like filesystem that already populate full metadata in
    /// `bulk_list_objects`).
    fn enrich_list_metadata(
        &self,
        _bucket: &str,
        objects: Vec<(String, FileMetadata)>,
    ) -> impl Future<Output = Result<Vec<(String, FileMetadata)>, StorageError>> + Send {
        async move { Ok(objects) }
    }

    /// Optimised listing, natively paged by the underlying store.
    ///
    /// Backends that can delegate listing to the underlying store (e.g. S3)
    /// override this to avoid materialising every key under the prefix.
    /// `delimiter: Some(_)` additionally collapses CommonPrefixes natively.
    /// Returns `None` when the backend can't delegate this shape, and the
    /// engine falls back to `bulk_list_objects` + in-memory paging.
    fn list_objects_delegated(
        &self,
        _bucket: &str,
        _prefix: &str,
        _delimiter: Option<&str>,
        _max_keys: u32,
        _continuation_token: Option<&str>,
    ) -> impl Future<Output = Result<Option<DelegatedListResult>, StorageError>> + Send {
        async move { Ok(None) }
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

/// What a read of an object for an in-place metadata rewrite returned
/// ([`StorageBackend::get_passthrough_metadata_for_rewrite`]), for the
/// rewrite of the same object ([`StorageBackend::put_passthrough_metadata`]):
/// the metadata as the backend stores it (the encrypting wrapper takes its
/// markers from it), and on S3 the HEAD response, whose headers the
/// self-copy restates and whose ETag fences it.
#[derive(Debug, Clone, Default)]
pub struct RewriteBasis {
    pub(crate) stored: Option<FileMetadata>,
    pub(crate) s3_head:
        Option<std::sync::Arc<aws_sdk_s3::operation::head_object::HeadObjectOutput>>,
}

impl RewriteBasis {
    /// No earlier read: the rewrite reads the object itself.
    pub fn none() -> Self {
        Self::default()
    }

    /// A read that returned `stored` (a backend without response headers).
    pub(crate) fn stored(stored: FileMetadata) -> Self {
        Self {
            stored: Some(stored),
            s3_head: None,
        }
    }
}

/// Result from `list_objects_delegated` when the backend handles delimiter
/// collapsing natively.
pub struct DelegatedListResult {
    pub objects: Vec<(String, FileMetadata)>,
    pub common_prefixes: Vec<String>,
    pub is_truncated: bool,
    pub next_continuation_token: Option<String>,
    /// The requests this call sent to the backend: its LIST pages and its
    /// probes. A backend that sends none (the filesystem) reports 0. The
    /// scan budget of a filtered LIST counts them.
    pub upstream_requests: u32,
    /// The stored forms that the page saw for its objects.
    pub forms: ListedForms,
    /// The deltaspaces (`""`: the bucket root) whose `reference.bin` the
    /// page read, possibly with ones of the neighbour pages. A walk of
    /// every page of a delimiter-less listing from its start reads every
    /// reference of the prefix. `None`: not reported (a delimiter listing,
    /// or a backend that does not track them).
    pub reference_prefixes: Option<Vec<String>>,
}

/// The stored forms (`k`, `k.delta`) that a listing page saw for its
/// objects ([`DelegatedListResult::forms`]). A page reads every stored
/// form of each key that it lists (the S3 page confirms a late `.delta`
/// with a probe), so a key that it saw in one form has no other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListedForms {
    /// The listing did not track the forms: a reader HEADs both.
    Unreported,
    /// Every object of the page has exactly the stored form that its entry
    /// names (`StorageInfo::Delta`: `k.delta`, else `k`), except the keys
    /// in `ambiguous` (sorted): both forms, or a stored name that is
    /// neither form of the key.
    Reported { ambiguous: Vec<String> },
}

impl ListedForms {
    /// The one stored form of `key`, listed with `meta`. `None` when the
    /// page did not report it, or the key is ambiguous.
    pub fn single_form(&self, key: &str, meta: &FileMetadata) -> Option<ObjectVariant> {
        match self {
            ListedForms::Unreported => None,
            ListedForms::Reported { ambiguous } => {
                if ambiguous.binary_search_by(|k| k.as_str().cmp(key)).is_ok() {
                    return None;
                }
                Some(if meta.is_delta() {
                    ObjectVariant::Delta
                } else {
                    ObjectVariant::Passthrough
                })
            }
        }
    }
}

/// Result of [`StorageBackend::bulk_list_objects_with_baselines`].
#[derive(Debug, Default)]
pub struct BulkListing {
    pub objects: Vec<(String, FileMetadata)>,
    /// `(stored key, stored size)` of every `reference.bin` in the listing.
    pub baselines: Vec<(String, u64)>,
    /// The `start_after` of the next page when the listing stopped at
    /// `max_listed` keys; `None` when it reached the end of the prefix.
    pub next_start_after: Option<String>,
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
