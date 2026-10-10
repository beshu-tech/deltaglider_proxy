// SPDX-License-Identifier: BUSL-1.1

//! A filesystem backend that fails chosen reads on demand, for tests of the
//! "a failed read is not an absence" class: an S3 HEAD that answers 503 must
//! never pass for "not found" on a path that writes.

use super::*;
use crate::storage::ObjectVariant;

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
    /// A conditional delete of either variant (versioned [`FaultyFs`]).
    DeleteVariantIf,
    /// The key is `"bucket//"`: the call names no prefix and no file.
    ListDeltaspaces,
    /// The key is `"bucket/scope/"`.
    ListReferencePrefixes,
}

/// Work a test runs inside a storage call, before the call goes on: a
/// peer's write that lands at that exact point.
pub(crate) type Hook =
    std::sync::Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

#[derive(Clone)]
enum Arm {
    /// The call fails, each time.
    Fail(Fault),
    /// The hook runs once, then the call goes on.
    Run(Hook),
}

#[derive(Default)]
struct FaultState {
    armed: std::collections::HashMap<(FaultPoint, String), Arm>,
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
        state
            .armed
            .insert((point, key.to_string()), Arm::Fail(fault));
    }

    /// Run `hook` once, at the next call at `point` on `key`.
    pub(crate) fn hook(&self, point: FaultPoint, key: &str, hook: Hook) {
        let mut state = self.0.lock().unwrap();
        state.armed.insert((point, key.to_string()), Arm::Run(hook));
    }

    /// Disarm every fault. The fire counts stay.
    pub(crate) fn disarm_all(&self) {
        self.0.lock().unwrap().armed.clear();
    }

    /// How many calls met the arm at `point` on `key` (failed, or ran the
    /// hook).
    pub(crate) fn fired(&self, point: FaultPoint, key: &str) -> usize {
        let state = self.0.lock().unwrap();
        state
            .fired
            .get(&(point, key.to_string()))
            .copied()
            .unwrap_or(0)
    }

    async fn trip(&self, point: FaultPoint, key: String) -> Result<(), StorageError> {
        let arm = {
            let mut state = self.0.lock().unwrap();
            let k = (point, key);
            let Some(arm) = state.armed.get(&k).cloned() else {
                return Ok(());
            };
            if matches!(arm, Arm::Run(_)) {
                state.armed.remove(&k);
            }
            *state.fired.entry(k).or_default() += 1;
            arm
        };
        match arm {
            Arm::Fail(fault) => Err(fault.error()),
            Arm::Run(hook) => {
                hook().await;
                Ok(())
            }
        }
    }
}

/// I/O faults by path, for the `fsio` probes (stat, read, open, remove):
/// a failing disk under one file. Process-wide, because the backend has
/// no handle to carry them; each test's paths are under its own temp
/// dir, so tests do not meet.
static IO_FAULTS: parking_lot::Mutex<Vec<(PathBuf, i32, usize)>> =
    parking_lot::Mutex::new(Vec::new());

/// Every `fsio` probe of `path` fails with `errno` until the guard drops.
pub(crate) fn fail_io(path: &Path, errno: i32) -> IoFault {
    let path = path.to_path_buf();
    let mut faults = IO_FAULTS.lock();
    faults.retain(|(p, _, _)| p != &path);
    faults.push((path.clone(), errno, 0));
    IoFault(path)
}

/// An armed [`fail_io`] fault; disarmed on drop.
pub(crate) struct IoFault(PathBuf);

impl IoFault {
    /// How many probes the fault failed.
    pub(crate) fn fired(&self) -> usize {
        IO_FAULTS
            .lock()
            .iter()
            .find(|(p, _, _)| p == &self.0)
            .map_or(0, |(_, _, n)| *n)
    }
}

impl Drop for IoFault {
    fn drop(&mut self) {
        IO_FAULTS.lock().retain(|(p, _, _)| p != &self.0);
    }
}

pub(super) fn io_fault(path: &Path) -> Option<std::io::Error> {
    let mut faults = IO_FAULTS.lock();
    let (_, errno, fired) = faults.iter_mut().find(|(p, _, _)| p == path)?;
    *fired += 1;
    Some(std::io::Error::from_raw_os_error(*errno))
}

/// [`FilesystemBackend`] that forwards every call, except the faults armed
/// on it.
pub(crate) struct FaultyFs {
    pub(crate) inner: FilesystemBackend,
    pub(crate) faults: Faults,
    /// Answer `variant_version` and honour `delete_variant_if` like S3
    /// `If-Match` (the filesystem backend has no conditional delete).
    versioned: bool,
}

impl FaultyFs {
    pub(crate) fn new(inner: FilesystemBackend) -> Self {
        Self {
            inner,
            faults: Faults::default(),
            versioned: false,
        }
    }

    /// A backend with S3's conditional delete: the version of a variant is
    /// its metadata's creation time and hash.
    pub(crate) fn versioned(inner: FilesystemBackend) -> Self {
        Self {
            versioned: true,
            ..Self::new(inner)
        }
    }

    async fn armed(
        &self,
        point: FaultPoint,
        b: &str,
        p: &str,
        f: &str,
    ) -> Result<(), StorageError> {
        self.faults.trip(point, format!("{b}/{p}/{f}")).await
    }

    async fn version_of(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        variant: ObjectVariant,
    ) -> Result<String, StorageError> {
        let meta = match variant {
            ObjectVariant::Delta => {
                self.inner
                    .get_delta_metadata(bucket, prefix, filename)
                    .await
            }
            ObjectVariant::Passthrough => {
                self.inner
                    .get_passthrough_metadata(bucket, prefix, filename)
                    .await
            }
        }?;
        Ok(format!(
            "{}/{}",
            meta.created_at.to_rfc3339(),
            meta.file_sha256
        ))
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
        self.armed(FaultPoint::HasReference, bucket, prefix, "")
            .await?;
        self.inner.has_reference(bucket, prefix).await
    }

    async fn variant_version(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        variant: ObjectVariant,
    ) -> Result<Option<String>, StorageError> {
        if !self.versioned {
            return Ok(None);
        }
        self.version_of(bucket, prefix, filename, variant)
            .await
            .map(Some)
    }

    async fn delete_variant_if(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        variant: ObjectVariant,
        version: &str,
    ) -> Result<bool, StorageError> {
        self.armed(FaultPoint::DeleteVariantIf, bucket, prefix, filename)
            .await?;
        if self.versioned {
            match self.version_of(bucket, prefix, filename, variant).await {
                Ok(now) if now == version => {}
                Ok(_) | Err(StorageError::NotFound(_)) => return Ok(false),
                Err(e) => return Err(e),
            }
        }
        match variant {
            ObjectVariant::Delta => self.delete_delta(bucket, prefix, filename).await?,
            ObjectVariant::Passthrough => self.delete_passthrough(bucket, prefix, filename).await?,
        }
        Ok(true)
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
        self.armed(FaultPoint::GetDeltaMetadata, bucket, prefix, filename)
            .await?;
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
        self.armed(FaultPoint::DeleteDelta, bucket, prefix, filename)
            .await?;
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
        self.armed(FaultPoint::GetPassthroughMetadata, bucket, prefix, filename)
            .await?;
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
        self.armed(FaultPoint::DeletePassthrough, bucket, prefix, filename)
            .await?;
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
        self.armed(FaultPoint::ListDeltaspaces, bucket, "", "")
            .await?;
        self.inner.list_deltaspaces(bucket).await
    }

    async fn list_reference_prefixes(
        &self,
        bucket: &str,
        scope: &str,
    ) -> Result<Vec<String>, StorageError> {
        self.armed(FaultPoint::ListReferencePrefixes, bucket, scope, "")
            .await?;
        self.inner.list_reference_prefixes(bucket, scope).await
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
