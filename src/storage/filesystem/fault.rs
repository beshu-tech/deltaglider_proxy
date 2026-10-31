// SPDX-License-Identifier: BUSL-1.1

//! A filesystem backend that fails chosen reads on demand, for tests of the
//! "a failed read is not an absence" class: an S3 HEAD that answers 503 must
//! never pass for "not found" on a path that writes.

use super::*;
use crate::storage::{ObjectVariant, PutBody, StoredWrite};

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
    /// The key is `"bucket/prefix/"`: the reference.bin metadata read.
    GetReferenceMetadata,
    /// A conditional delete of either variant (versioned [`FaultyFs`]).
    DeleteVariantIf,
    /// The key is `"bucket//"`: the call names no prefix and no file.
    ListDeltaspaces,
    /// The key is `"bucket/scope/"`.
    ListReferencePrefixes,
    /// A delta write, before it writes: the writer holds its deltaspace
    /// lock and encoded against reference.bin.
    PutDelta,
    /// The key is `"bucket//"`: the read of the bucket's key claim.
    ReadKeyClaim,
    /// The key is `"bucket//"`: a write of the bucket's key claim.
    WriteKeyClaim,
    /// The key is `"bucket/prefix/"`: a delegated listing page.
    ListObjectsDelegated,
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
    /// Every call at each point and key, armed or not (cost tests).
    calls: std::collections::HashMap<(FaultPoint, String), usize>,
    /// `Some`: each passthrough write waits this long before it writes,
    /// and the gauge counts the writes in flight.
    slow_passthrough: Option<std::time::Duration>,
    in_flight: usize,
    max_in_flight: usize,
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

    /// Make every passthrough write wait `delay` before it writes, and
    /// count how many run at once ([`Self::max_passthrough_in_flight`]).
    pub(crate) fn slow_passthrough_writes(&self, delay: std::time::Duration) {
        let mut state = self.0.lock().unwrap();
        state.slow_passthrough = Some(delay);
        state.max_in_flight = 0;
    }

    /// The most passthrough writes that ran at once since
    /// [`Self::slow_passthrough_writes`].
    pub(crate) fn max_passthrough_in_flight(&self) -> usize {
        self.0.lock().unwrap().max_in_flight
    }

    /// One passthrough write: wait and count, when armed.
    async fn passthrough_write<T>(&self, write: impl std::future::Future<Output = T>) -> T {
        let delay = {
            let mut state = self.0.lock().unwrap();
            let delay = state.slow_passthrough;
            if delay.is_some() {
                state.in_flight += 1;
                state.max_in_flight = state.max_in_flight.max(state.in_flight);
            }
            delay
        };
        let Some(delay) = delay else {
            return write.await;
        };
        tokio::time::sleep(delay).await;
        let out = write.await;
        self.0.lock().unwrap().in_flight -= 1;
        out
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

    /// How many calls reached `point` on `key`, armed or not.
    pub(crate) fn calls(&self, point: FaultPoint, key: &str) -> usize {
        let state = self.0.lock().unwrap();
        state
            .calls
            .get(&(point, key.to_string()))
            .copied()
            .unwrap_or(0)
    }

    async fn trip(&self, point: FaultPoint, key: String) -> Result<(), StorageError> {
        let arm = {
            let mut state = self.0.lock().unwrap();
            let k = (point, key);
            *state.calls.entry(k.clone()).or_default() += 1;
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

    async fn copy_within(
        &self,
        variant: crate::storage::ObjectVariant,
        src: crate::storage::ObjectAt<'_>,
        dst: crate::storage::ObjectAt<'_>,
        source: &FileMetadata,
        metadata: &FileMetadata,
    ) -> Result<crate::storage::CopyOutcome, StorageError> {
        self.inner
            .copy_within(variant, src, dst, source, metadata)
            .await
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

    async fn get_reference_to_file(
        &self,
        bucket: &str,
        prefix: &str,
        dest: &Path,
    ) -> Result<u64, StorageError> {
        self.inner.get_reference_to_file(bucket, prefix, dest).await
    }

    async fn get_passthrough_metadata_for_rewrite(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(FileMetadata, crate::storage::RewriteBasis), StorageError> {
        self.armed(FaultPoint::GetPassthroughMetadata, bucket, prefix, filename)
            .await?;
        self.inner
            .get_passthrough_metadata_for_rewrite(bucket, prefix, filename)
            .await
    }

    async fn put_passthrough_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        metadata: &FileMetadata,
        basis: &crate::storage::RewriteBasis,
    ) -> Result<(), StorageError> {
        self.inner
            .put_passthrough_metadata(bucket, prefix, filename, metadata, basis)
            .await
    }

    async fn reference_fence(&self, bucket: &str, prefix: &str) -> Result<RefFence, StorageError> {
        self.inner.reference_fence(bucket, prefix).await
    }

    async fn reference_fence_with_metadata(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<(RefFence, Option<FileMetadata>), StorageError> {
        self.armed(FaultPoint::GetReferenceMetadata, bucket, prefix, "")
            .await?;
        self.inner
            .reference_fence_with_metadata(bucket, prefix)
            .await
    }

    async fn write_reference(
        &self,
        bucket: &str,
        prefix: &str,
        op: RefWrite<'_>,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.inner.write_reference(bucket, prefix, op, proof).await
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

    async fn get_object(
        &self,
        bucket: &str,
        prefix: &str,
        object: StoredObject<'_>,
    ) -> Result<Vec<u8>, StorageError> {
        self.inner.get_object(bucket, prefix, object).await
    }

    async fn put_object(
        &self,
        bucket: &str,
        prefix: &str,
        target: StoredWrite<'_>,
        body: PutBody<'_>,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        match target {
            StoredWrite::Delta { name, .. } => {
                self.armed(FaultPoint::PutDelta, bucket, prefix, name)
                    .await?;
                self.inner
                    .put_object(bucket, prefix, target, body, metadata)
                    .await
            }
            StoredWrite::Passthrough { .. } => {
                self.faults
                    .passthrough_write(
                        self.inner
                            .put_object(bucket, prefix, target, body, metadata),
                    )
                    .await
            }
        }
    }

    async fn head_object(
        &self,
        bucket: &str,
        prefix: &str,
        object: StoredObject<'_>,
    ) -> Result<FileMetadata, StorageError> {
        let (point, filename) = match object {
            StoredObject::Reference => (FaultPoint::GetReferenceMetadata, ""),
            StoredObject::Delta(f) => (FaultPoint::GetDeltaMetadata, f),
            StoredObject::Passthrough(f) => (FaultPoint::GetPassthroughMetadata, f),
        };
        self.armed(point, bucket, prefix, filename).await?;
        self.inner.head_object(bucket, prefix, object).await
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

    async fn delete_object(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        variant: ObjectVariant,
    ) -> Result<(), StorageError> {
        let point = match variant {
            ObjectVariant::Delta => FaultPoint::DeleteDelta,
            ObjectVariant::Passthrough => FaultPoint::DeletePassthrough,
        };
        self.armed(point, bucket, prefix, filename).await?;
        self.inner
            .delete_object(bucket, prefix, filename, variant)
            .await
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
        self.delete_object(bucket, prefix, filename, variant)
            .await?;
        Ok(true)
    }

    async fn open_object(
        &self,
        bucket: &str,
        prefix: &str,
        object: StoredObject<'_>,
    ) -> Result<crate::storage::ObjectRead, StorageError> {
        self.inner.open_object(bucket, prefix, object).await
    }

    async fn open_reference_source(
        &self,
        b: &str,
        p: &str,
    ) -> Result<crate::storage::ReferenceSource, crate::storage::StorageError> {
        self.inner.open_reference_source(b, p).await
    }

    async fn open_passthrough_range(
        &self,
        b: &str,
        p: &str,
        f: &str,
        start: u64,
        end: u64,
    ) -> Result<crate::storage::RangeRead, crate::storage::StorageError> {
        self.inner.open_passthrough_range(b, p, f, start, end).await
    }
    async fn listing_tokens(
        &self,
        b: &str,
        objects: &[(String, crate::types::FileMetadata)],
    ) -> Vec<Option<crate::storage::ListedToken>> {
        self.inner.listing_tokens(b, objects).await
    }
    async fn keep_listing_token(&self, b: &str, token: &crate::storage::ListedToken) {
        self.inner.keep_listing_token(b, token).await
    }

    async fn read_key_claim(
        &self,
        bucket: &str,
    ) -> Result<Option<crate::storage::KeyClaim>, crate::storage::StorageError> {
        self.armed(FaultPoint::ReadKeyClaim, bucket, "", "").await?;
        self.inner.read_key_claim(bucket).await
    }
    async fn write_key_claim(
        &self,
        bucket: &str,
        op: crate::storage::ClaimWrite<'_>,
    ) -> Result<crate::storage::ClaimOutcome, crate::storage::StorageError> {
        self.armed(FaultPoint::WriteKeyClaim, bucket, "", "")
            .await?;
        self.inner.write_key_claim(bucket, op).await
    }

    async fn scan_deltaspace(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<FileMetadata>, StorageError> {
        self.inner.scan_deltaspace(bucket, prefix).await
    }

    async fn holds_only_reference(&self, bucket: &str, prefix: &str) -> Result<bool, StorageError> {
        self.inner.holds_only_reference(bucket, prefix).await
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
        start_after: Option<&str>,
        max_listed: Option<usize>,
    ) -> Result<BulkListing, StorageError> {
        self.inner
            .bulk_list_objects_with_baselines(bucket, prefix, start_after, max_listed)
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
        self.armed(FaultPoint::ListObjectsDelegated, bucket, prefix, "")
            .await?;
        self.inner
            .list_objects_delegated(bucket, prefix, delimiter, max_keys, continuation_token)
            .await
    }
}
