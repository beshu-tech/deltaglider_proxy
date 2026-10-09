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

/// A storage call that a test can fail. An enum, so a misspelt point is a
/// compile error and not a fault that never fires (review C3). Add a point
/// when a test needs one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FaultPoint {
    GetDeltaMetadata,
    GetPassthroughMetadata,
    DeleteDelta,
    DeletePassthrough,
    /// The key is `"bucket/prefix/"`: the call names no file.
    HasReference,
}

#[derive(Default)]
struct FaultState {
    armed: std::collections::HashMap<(FaultPoint, String), Fault>,
    fired: std::collections::HashMap<(FaultPoint, String), usize>,
}

/// The faults armed on a [`FaultyFs`], by point and `"bucket/prefix/filename"`.
/// Shared, so a test arms them after the engine owns the backend. A test
/// asserts [`Faults::fired`]: a fault that never fires proves nothing.
#[derive(Clone, Default)]
pub(crate) struct Faults(std::sync::Arc<std::sync::Mutex<FaultState>>);

impl Faults {
    pub(crate) fn arm(&self, point: FaultPoint, key: &str, fault: Fault) {
        let mut state = self.0.lock().unwrap();
        state.armed.insert((point, key.to_string()), fault);
    }

    /// Disarm every fault. The fire counts stay.
    pub(crate) fn disarm_all(&self) {
        self.0.lock().unwrap().armed.clear();
    }

    /// How many calls the fault at `point` on `key` failed.
    pub(crate) fn fired(&self, point: FaultPoint, key: &str) -> usize {
        let state = self.0.lock().unwrap();
        state
            .fired
            .get(&(point, key.to_string()))
            .copied()
            .unwrap_or(0)
    }

    fn check(&self, point: FaultPoint, key: String) -> Result<(), StorageError> {
        let mut state = self.0.lock().unwrap();
        let Some(fault) = state.armed.get(&(point, key.clone())).copied() else {
            return Ok(());
        };
        *state.fired.entry((point, key)).or_default() += 1;
        Err(fault.error())
    }
}

/// [`FilesystemBackend`] that forwards every call, except the faults armed
/// on it.
pub(crate) struct FaultyFs {
    pub(crate) inner: FilesystemBackend,
    pub(crate) faults: Faults,
}

impl FaultyFs {
    pub(crate) fn new(inner: FilesystemBackend) -> Self {
        Self {
            inner,
            faults: Faults::default(),
        }
    }

    fn armed(&self, point: FaultPoint, b: &str, p: &str, f: &str) -> Result<(), StorageError> {
        self.faults.check(point, format!("{b}/{p}/{f}"))
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
        self.armed(FaultPoint::HasReference, bucket, prefix, "")?;
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
        self.armed(FaultPoint::GetDeltaMetadata, bucket, prefix, filename)?;
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
        self.armed(FaultPoint::DeleteDelta, bucket, prefix, filename)?;
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
        self.armed(FaultPoint::GetPassthroughMetadata, bucket, prefix, filename)?;
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
        self.armed(FaultPoint::DeletePassthrough, bucket, prefix, filename)?;
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
