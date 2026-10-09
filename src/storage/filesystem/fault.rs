// SPDX-License-Identifier: BUSL-1.1

//! A filesystem backend that fails chosen reads on demand, for tests of the
//! "a failed read is not an absence" class: an S3 HEAD that answers 503 must
//! never pass for "not found" on a path that writes.

use super::*;

/// The error an armed call returns.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Fault {
    /// An I/O error (EIO), as a failing disk or an unreadable xattr gives.
    Io,
    /// A retryable backend refusal (S3 503 SlowDown).
    Throttled,
}

impl Fault {
    fn error(self) -> StorageError {
        match self {
            Fault::Io => StorageError::Io(std::io::Error::other("injected EIO")),
            Fault::Throttled => StorageError::Throttled("injected SlowDown".into()),
        }
    }
}

/// `(method, "bucket/prefix/filename")` → the fault that call returns.
pub(crate) type FaultTable =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<(&'static str, String), Fault>>>;

/// [`FilesystemBackend`] that forwards every call, except the faults armed
/// on it.
pub(crate) struct FaultyFs {
    pub(crate) inner: FilesystemBackend,
    /// Armed per call: `get_delta_metadata`, `get_passthrough_metadata`,
    /// `delete_delta`, `delete_passthrough`. Shared, like the field below.
    pub(crate) faults: FaultTable,
    /// `"bucket/prefix"` whose `has_reference` answers `Throttled`. Shared,
    /// so a test arms it after the engine owns the backend.
    pub(crate) fail_has_reference: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl FaultyFs {
    pub(crate) fn new(inner: FilesystemBackend) -> Self {
        Self {
            inner,
            fail_has_reference: Default::default(),
            faults: Default::default(),
        }
    }
}

impl FaultyFs {
    fn armed(&self, method: &'static str, b: &str, p: &str, f: &str) -> Result<(), StorageError> {
        match self
            .faults
            .lock()
            .unwrap()
            .get(&(method, format!("{b}/{p}/{f}")))
        {
            Some(fault) => Err(fault.error()),
            None => Ok(()),
        }
    }
}

impl StorageBackend for FaultyFs {
    async fn flush_pending(&self) -> Result<(), StorageError> {
        self.inner.flush_pending().await
    }

    async fn create_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        self.inner.create_bucket(bucket).await
    }

    async fn ensure_declared_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        self.inner.ensure_declared_bucket(bucket).await
    }

    async fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        self.inner.delete_bucket(bucket).await
    }

    async fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
        self.inner.list_buckets().await
    }

    async fn list_buckets_with_dates(
        &self,
    ) -> Result<Vec<(String, chrono::DateTime<chrono::Utc>)>, StorageError> {
        self.inner.list_buckets_with_dates().await
    }

    async fn head_bucket(&self, bucket: &str) -> Result<bool, StorageError> {
        self.inner.head_bucket(bucket).await
    }

    async fn get_reference(&self, bucket: &str, prefix: &str) -> Result<Vec<u8>, StorageError> {
        self.inner.get_reference(bucket, prefix).await
    }

    async fn get_reference_to_file(
        &self,
        bucket: &str,
        prefix: &str,
        dest: &Path,
    ) -> Result<u64, StorageError> {
        self.inner.get_reference_to_file(bucket, prefix, dest).await
    }

    async fn put_reference(
        &self,
        bucket: &str,
        prefix: &str,
        data: &[u8],
        metadata: &FileMetadata,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.inner
            .put_reference(bucket, prefix, data, metadata, proof)
            .await
    }

    async fn put_reference_from_file(
        &self,
        bucket: &str,
        prefix: &str,
        source_path: &Path,
        metadata: &FileMetadata,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.inner
            .put_reference_from_file(bucket, prefix, source_path, metadata, proof)
            .await
    }

    async fn put_reference_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        metadata: &FileMetadata,
        _proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.inner
            .put_reference_metadata(bucket, prefix, metadata, _proof)
            .await
    }

    async fn put_passthrough_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.inner
            .put_passthrough_metadata(bucket, prefix, filename, metadata)
            .await
    }

    async fn get_reference_metadata(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<FileMetadata, StorageError> {
        self.inner.get_reference_metadata(bucket, prefix).await
    }

    async fn reference_fence(&self, bucket: &str, prefix: &str) -> Result<RefFence, StorageError> {
        self.inner.reference_fence(bucket, prefix).await
    }

    async fn write_reference_fenced(
        &self,
        bucket: &str,
        prefix: &str,
        op: RefWrite<'_>,
        _fence: &RefFence,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<RefFence, StorageError> {
        self.inner
            .write_reference_fenced(bucket, prefix, op, _fence, proof)
            .await
    }

    async fn has_reference(&self, bucket: &str, prefix: &str) -> Result<bool, StorageError> {
        if self.fail_has_reference.lock().unwrap().as_deref() == Some(&format!("{bucket}/{prefix}"))
        {
            return Err(StorageError::Throttled("injected SlowDown".into()));
        }
        self.inner.has_reference(bucket, prefix).await
    }

    async fn delete_reference(
        &self,
        bucket: &str,
        prefix: &str,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.inner.delete_reference(bucket, prefix, proof).await
    }

    async fn get_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
        self.inner.get_delta(bucket, prefix, filename).await
    }

    async fn put_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.inner
            .put_delta(bucket, prefix, filename, data, metadata, proof)
            .await
    }

    async fn get_delta_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        self.armed("get_delta_metadata", bucket, prefix, filename)?;
        self.inner
            .get_delta_metadata(bucket, prefix, filename)
            .await
    }

    async fn delete_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        self.armed("delete_delta", bucket, prefix, filename)?;
        self.inner.delete_delta(bucket, prefix, filename).await
    }

    async fn get_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
        self.inner.get_passthrough(bucket, prefix, filename).await
    }

    async fn put_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.inner
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
        _spool: crate::deltaglider::spool::SpoolBudget<'_>,
    ) -> Result<(), StorageError> {
        self.inner
            .put_passthrough_file(bucket, prefix, filename, source_path, metadata, _spool)
            .await
    }

    async fn put_passthrough_parts(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        part_paths: &[PathBuf],
        metadata: &FileMetadata,
        _spool: crate::deltaglider::spool::SpoolBudget<'_>,
    ) -> Result<(), StorageError> {
        self.inner
            .put_passthrough_parts(bucket, prefix, filename, part_paths, metadata, _spool)
            .await
    }

    async fn get_passthrough_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        self.armed("get_passthrough_metadata", bucket, prefix, filename)?;
        self.inner
            .get_passthrough_metadata(bucket, prefix, filename)
            .await
    }

    async fn delete_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        self.armed("delete_passthrough", bucket, prefix, filename)?;
        self.inner
            .delete_passthrough(bucket, prefix, filename)
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
        self.inner
            .put_passthrough_chunked(bucket, prefix, filename, chunks, metadata)
            .await
    }

    async fn get_passthrough_stream(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError> {
        self.inner
            .get_passthrough_stream(bucket, prefix, filename)
            .await
    }

    async fn open_object(
        &self,
        bucket: &str,
        prefix: &str,
        object: StoredObject<'_>,
    ) -> Result<(crate::storage::ByteStream, FileMetadata), StorageError> {
        self.inner.open_object(bucket, prefix, object).await
    }

    async fn get_passthrough_stream_range(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        start: u64,
        end: u64,
    ) -> Result<(BoxStream<'static, Result<Bytes, StorageError>>, u64), StorageError> {
        self.inner
            .get_passthrough_stream_range(bucket, prefix, filename, start, end)
            .await
    }

    async fn scan_deltaspace(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<FileMetadata>, StorageError> {
        self.inner.scan_deltaspace(bucket, prefix).await
    }

    async fn list_deltaspaces(&self, bucket: &str) -> Result<Vec<String>, StorageError> {
        self.inner.list_deltaspaces(bucket).await
    }

    async fn put_directory_marker(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        self.inner.put_directory_marker(bucket, key).await
    }

    async fn total_size(&self, bucket: Option<&str>) -> Result<u64, StorageError> {
        self.inner.total_size(bucket).await
    }

    async fn bulk_list_objects(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
        self.inner.bulk_list_objects(bucket, prefix).await
    }

    async fn bulk_list_objects_with_baselines(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<BulkListing, StorageError> {
        self.inner
            .bulk_list_objects_with_baselines(bucket, prefix)
            .await
    }

    async fn list_objects_delegated(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys: u32,
        continuation_token: Option<&str>,
    ) -> Result<Option<DelegatedListResult>, StorageError> {
        self.inner
            .list_objects_delegated(bucket, prefix, delimiter, max_keys, continuation_token)
            .await
    }
}
