// SPDX-License-Identifier: BUSL-1.1

//! The re-encrypt job's in-place rewrites: one stored object (a passthrough
//! object, a delta, or reference.bin) read and stored again through the
//! storage stack, so the encrypting wrapper seals it with the bucket's
//! write key (or writes it as plaintext where the bucket writes plaintext).
//! The plaintext bytes and the metadata do not change.
//!
//! Every rewrite is CONDITIONAL on the version that the job read
//! ([`StorageBackend::put_if_unchanged`]): a client write that lands
//! between the job's read and its write, on any instance, is never
//! overwritten. The rewrite then ends as [`Rewritten::Changed`], and the
//! object stays as the client wrote it. That is safe because a client write
//! through a proxy that encrypts the bucket is sealed already.
//!
//! The rewrite stores the same stored form that it read (never a delta for
//! a passthrough object or the reverse): one conditional write per object,
//! and no delete of another form, which no condition could cover.

use super::conditional::ObjectWriteGuard;
use super::*;
use crate::config::test_seams::RewriteStall;
use crate::storage::{ObjectVariant, PutBody, RewriteTarget, StoredObject, StoredWrite, WriteIf};

/// What a rewrite does on a backend without conditional writes (S3 501).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unconditional {
    /// Write without the condition. Only for a job whose bucket write gate
    /// is armed: no client write of this node lands meanwhile.
    Allowed,
    /// Fail the rewrite: the job runs without the gate.
    Refused,
}

/// What one in-place rewrite ended in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rewritten {
    /// Stored again; `bytes`: the plaintext size.
    Done { bytes: u64 },
    /// Not stored: the object is gone or is another version than the one
    /// the caller read (a client wrote it meanwhile). It stays as it is.
    Changed,
}

/// Pure: what a conditional rewrite's answer means. `Ok(None)`: the
/// backend has no conditional writes and the job holds the write gate, so
/// the caller writes without the condition. One rule for every form.
fn conditional_outcome(
    out: WriteIf,
    bytes: u64,
    unconditional: Unconditional,
    bucket: &str,
) -> Result<Option<Rewritten>, EngineError> {
    match out {
        WriteIf::Written => Ok(Some(Rewritten::Done { bytes })),
        WriteIf::Changed => Ok(Some(Rewritten::Changed)),
        WriteIf::Unsupported if unconditional == Unconditional::Allowed => Ok(None),
        WriteIf::Unsupported => Err(no_conditional_writes(bucket)),
    }
}

/// A passthrough body that a rewrite read: in memory, or in a spool file.
struct ReadBody {
    hashes: BodyHashes,
    data: Option<Bytes>,
}

/// The hashes of a rewrite's plaintext body.
struct BodyHashes {
    sha256: String,
    md5: String,
}

impl BodyHashes {
    fn of(data: &[u8]) -> Self {
        use md5::Digest as _;
        Self {
            sha256: hex::encode(sha2::Sha256::digest(data)),
            md5: hex::encode(md5::Md5::digest(data)),
        }
    }
}

/// Pure: is `s` a bare MD5 in hex (32 hex digits)? A foreign object's
/// fallback metadata carries its backend ETag there, which for a multipart
/// upload is `<hex>-<parts>`.
fn is_md5_hex(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The metadata that a rewrite stores: the read's metadata without the
/// stored encryption markers (the wrapper stamps its own), with the content
/// hashes of the body. `Err` when the body is not the content the
/// metadata names (a corrupt object is never sealed as good).
fn rewrite_metadata(
    read: &FileMetadata,
    hashes: Option<&BodyHashes>,
) -> Result<FileMetadata, EngineError> {
    let mut meta = read.clone();
    crate::storage::encrypting::strip_encryption_markers(&mut meta.user_metadata);
    if let Some(h) = hashes {
        if !meta.file_sha256.is_empty() && meta.file_sha256 != h.sha256 {
            return Err(EngineError::Storage(StorageError::Other(format!(
                "{}: the stored body does not match its SHA-256; not rewritten",
                meta.original_name
            ))));
        }
        meta.file_sha256 = h.sha256.clone();
        if !is_md5_hex(&meta.md5) {
            meta.md5 = h.md5.clone();
        }
    }
    Ok(meta)
}

impl<S: StorageBackend> DeltaGliderEngine<S> {
    /// The metadata of ONE stored form of `key` (a HEAD on S3), `None` when
    /// it is absent.
    pub async fn stored_form(
        &self,
        bucket: &str,
        key: &str,
        form: ObjectVariant,
    ) -> Result<Option<FileMetadata>, EngineError> {
        let (obj_key, deltaspace_id) = self.validated_key(bucket, key)?;
        Ok(self
            .stored_form_metadata(bucket, &deltaspace_id, &obj_key.filename, form)
            .await?)
    }

    /// Rewrite the stored form `form` of `key` in place, conditional on the
    /// version that `read` (the caller's HEAD of that form) saw. Takes the
    /// key's object write lock, and for a delta the deltaspace lock (and
    /// the cross-instance reference lock), as every write of the object
    /// does. The body is read in ONE request, which also names its version:
    /// a version other than `read`'s is `Changed` without a write.
    pub async fn rewrite_object_if_unchanged(
        &self,
        bucket: &str,
        key: &str,
        form: ObjectVariant,
        read: &FileMetadata,
        unconditional: Unconditional,
    ) -> Result<Rewritten, EngineError> {
        let Some(version) = read.backend_version.clone() else {
            return Err(EngineError::Storage(StorageError::Other(format!(
                "{bucket}/{key}: the read named no stored version, and a rewrite \
                 is conditional on one"
            ))));
        };
        let (obj_key, deltaspace_id) = self.validated_key(bucket, key)?;
        let name = obj_key.filename.clone();
        test_stall(RewriteStall::BeforeLock, key).await;
        let lock = self.lock_object_write(bucket, key).await;
        let out = match form {
            ObjectVariant::Passthrough => {
                let at = (bucket, deltaspace_id.as_str(), name.as_str());
                self.rewrite_passthrough(&lock, at, read, &version, unconditional)
                    .await?
            }
            ObjectVariant::Delta => {
                let at = (bucket, deltaspace_id.as_str(), name.as_str());
                let run = || self.rewrite_delta(&lock, at, &version, unconditional);
                self.with_dest_prefix_lock(bucket, &deltaspace_id, run)
                    .await?
            }
        };
        if matches!(out, Rewritten::Done { .. }) {
            self.metadata_cache.invalidate(bucket, key);
        }
        drop(lock);
        Ok(out)
    }

    async fn rewrite_passthrough(
        &self,
        lock: &ObjectWriteGuard,
        (bucket, deltaspace_id, name): (&str, &str, &str),
        read: &FileMetadata,
        version: &str,
        unconditional: Unconditional,
    ) -> Result<Rewritten, EngineError> {
        lock.covers(bucket, deltaspace_id, name)?;
        let size = read.file_size;
        // Large: the body goes through a spool file (bounded memory).
        // Taken before the GET, so no response waits for budget.
        let spool = if size > self.spool_threshold() {
            Some(self.spool_acquire(size).await?)
        } else {
            None
        };
        let opened = match self
            .storage
            .open_object(bucket, deltaspace_id, StoredObject::Passthrough(name))
            .await
        {
            Ok(o) => o,
            Err(StorageError::NotFound(_)) => return Ok(Rewritten::Changed),
            Err(e) => return Err(e.into()),
        };
        if opened.metadata.backend_version.as_deref() != Some(version) {
            return Ok(Rewritten::Changed);
        }
        let body_meta = opened.metadata.clone();
        let read_body = match &spool {
            Some(spool) => {
                Self::read_into_spool(opened.stream, spool, size, (bucket, name)).await?
            }
            None => Self::read_into_memory(opened.stream, size).await?,
        };
        // A body of another size than the HEAD said: another version.
        let Some(ReadBody { hashes, data }) = read_body else {
            return Ok(Rewritten::Changed);
        };
        let meta = rewrite_metadata(&body_meta, Some(&hashes))?;
        // The encrypting wrapper's ciphertext file, reserved now (a storage
        // write never waits for budget).
        let reserved = match &spool {
            Some(s) => {
                self.reserve_storage_spool(bucket, size, false, s.reserved_mib())
                    .await?
            }
            None => None,
        };
        let body = || match (&spool, &data) {
            (Some(s), _) => PutBody::File {
                path: s.path(),
                spool: crate::deltaglider::spool::SpoolBudget::new(
                    &self.spool,
                    Some(s),
                    reserved.as_ref(),
                ),
            },
            (None, Some(d)) => PutBody::Bytes(d.clone()),
            (None, None) => unreachable!("a body in memory or in a spool file"),
        };
        test_stall(RewriteStall::BeforeWrite, name).await;
        let target = RewriteTarget::Passthrough { name };
        let out = self
            .storage
            .put_if_unchanged(bucket, deltaspace_id, target, body(), &meta, version)
            .await?;
        if let Some(done) = conditional_outcome(out, size, unconditional, bucket)? {
            return Ok(done);
        }
        let target = StoredWrite::Passthrough { name };
        self.storage
            .put_object(bucket, deltaspace_id, target, body(), &meta)
            .await?;
        Ok(Rewritten::Done { bytes: size })
    }

    /// A large passthrough body into `spool`, hashed on the way (SHA-256
    /// by the spool, MD5 by the stream). `Ok(None)`: it was not `size`
    /// bytes long.
    async fn read_into_spool(
        mut stream: crate::storage::ByteStream,
        spool: &crate::deltaglider::spool::Spool,
        size: u64,
        (bucket, name): (&str, &str),
    ) -> Result<Option<ReadBody>, EngineError> {
        use crate::deltaglider::spool::SpoolFillError;
        let md5 = std::sync::Arc::new(std::sync::Mutex::new(<md5::Md5 as md5::Digest>::new()));
        let feed = md5.clone();
        let mut hashed = futures::StreamExt::map(&mut stream, move |chunk| {
            if let Ok(c) = &chunk {
                md5::Digest::update(&mut *feed.lock().expect("md5 lock"), c);
            }
            chunk
        });
        let fill = spool
            .fill_from_stream(&mut hashed, size)
            .await
            .map_err(|e| match e {
                SpoolFillError::Source(e) => EngineError::Storage(e),
                SpoolFillError::Write(e) => EngineError::Storage(e.into()),
                SpoolFillError::Overrun { .. } => EngineError::Storage(StorageError::Other(
                    format!("{bucket}/{name} grew while it was read"),
                )),
            })?;
        drop(hashed);
        if fill.written != size {
            return Ok(None);
        }
        let md5 = std::mem::replace(
            &mut *md5.lock().expect("md5 lock"),
            <md5::Md5 as md5::Digest>::new(),
        );
        Ok(Some(ReadBody {
            hashes: BodyHashes {
                sha256: fill.sha256,
                md5: hex::encode(md5::Digest::finalize(md5)),
            },
            data: None,
        }))
    }

    /// A small passthrough body into memory. `Ok(None)`: it was not `size`
    /// bytes long.
    async fn read_into_memory(
        stream: crate::storage::ByteStream,
        size: u64,
    ) -> Result<Option<ReadBody>, EngineError> {
        use futures::TryStreamExt;
        let data: Vec<u8> = stream
            .try_fold(Vec::new(), |mut acc, chunk| async move {
                acc.extend_from_slice(&chunk);
                Ok(acc)
            })
            .await?;
        if data.len() as u64 != size {
            return Ok(None);
        }
        Ok(Some(ReadBody {
            hashes: BodyHashes::of(&data),
            data: Some(Bytes::from(data)),
        }))
    }

    /// Under the deltaspace lock: the delta bytes, read and stored again.
    async fn rewrite_delta(
        &self,
        lock: &ObjectWriteGuard,
        (bucket, deltaspace_id, name): (&str, &str, &str),
        version: &str,
        unconditional: Unconditional,
    ) -> Result<Rewritten, EngineError> {
        lock.covers(bucket, deltaspace_id, name)?;
        let held = self
            .held_reference_lock(bucket, deltaspace_id, "delta")
            .map_err(EngineError::Storage)?;
        let (data, read) = match self
            .open_raw(bucket, deltaspace_id, StoredObject::Delta(name))
            .await
        {
            Ok(r) => r,
            Err(StorageError::NotFound(_)) => return Ok(Rewritten::Changed),
            Err(e) => return Err(e.into()),
        };
        if read.backend_version.as_deref() != Some(version) {
            return Ok(Rewritten::Changed);
        }
        if let crate::types::StorageInfo::Delta { delta_size, .. } = &read.storage_info {
            if *delta_size > 0 && *delta_size != data.len() as u64 {
                return Err(EngineError::Storage(StorageError::Other(format!(
                    "{bucket}/{deltaspace_id}/{name}.delta holds {} bytes, its metadata \
                     names {delta_size}; not rewritten",
                    data.len()
                ))));
            }
        }
        // A delta's SHA-256 is the object's, not the delta's: no hashes.
        let meta = rewrite_metadata(&read, None)?;
        let bytes = data.len() as u64;
        let data = Bytes::from(data);
        test_stall(RewriteStall::BeforeWrite, name).await;
        let out = held
            .rewrite_delta_if(
                &*self.storage,
                bucket,
                deltaspace_id,
                name,
                crate::storage::PutBody::Bytes(data.clone()),
                &meta,
                version,
            )
            .await?;
        if let Some(done) = conditional_outcome(out, bytes, unconditional, bucket)? {
            return Ok(done);
        }
        let body = crate::storage::PutBody::Bytes(data);
        held.put_delta(&*self.storage, bucket, deltaspace_id, name, body, &meta)
            .await?;
        Ok(Rewritten::Done { bytes })
    }

    /// Rewrite reference.bin of `prefix` in place, when `needs(meta)` says
    /// its at-rest state is not the desired one. Call it inside
    /// [`Self::with_dest_prefix_lock`] (the lock every reference write
    /// takes). The body and its version come from ONE read; the write is
    /// conditional on that version. `Ok(None)`: no reference, or it needs
    /// no rewrite; `Ok(Some((outcome, read)))`: the metadata it was read
    /// with, for the caller's report.
    pub async fn rewrite_reference_if_unchanged(
        &self,
        bucket: &str,
        prefix: &str,
        needs: impl FnOnce(&FileMetadata) -> bool,
        unconditional: Unconditional,
    ) -> Result<Option<(Rewritten, FileMetadata)>, EngineError> {
        let held = self
            .held_reference_lock(bucket, prefix, "reference")
            .map_err(EngineError::Storage)?;
        let (data, read) = match self.open_raw(bucket, prefix, StoredObject::Reference).await {
            Ok(r) => r,
            Err(StorageError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if !needs(&read) {
            return Ok(None);
        }
        let Some(version) = read.backend_version.clone() else {
            return Err(EngineError::Storage(StorageError::Other(format!(
                "reference.bin of {bucket}/{prefix}: the read named no stored version, \
                 and a rewrite is conditional on one"
            ))));
        };
        let hashes = BodyHashes::of(&data);
        let meta = rewrite_metadata(&read, Some(&hashes))?;
        let bytes = data.len() as u64;
        let data = Bytes::from(data);
        let out = held
            .rewrite_reference_if(
                &*self.storage,
                bucket,
                prefix,
                crate::storage::PutBody::Bytes(data.clone()),
                &meta,
                &version,
            )
            .await?;
        let outcome = match conditional_outcome(out, bytes, unconditional, bucket)? {
            Some(done) => done,
            None => {
                held.put_reference(&*self.storage, bucket, prefix, &data, &meta)
                    .await?;
                Rewritten::Done { bytes }
            }
        };
        Ok(Some((outcome, read)))
    }
}

/// Test seam (debug builds only): the stall of `DGP_TEST_REWRITE_STALL`
/// at `point`, if it names this point. With a signal file, `object` is
/// appended to it as one line first.
async fn test_stall(point: RewriteStall, object: &str) {
    let Some(stall) = crate::config::test_seams::test_seams().rewrite_stall_at(point) else {
        return;
    };
    if let Some(path) = &stall.signal {
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{object}");
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(stall.ms)).await;
}

/// The error of a rewrite that must be conditional on a backend that has
/// no conditional writes.
fn no_conditional_writes(bucket: &str) -> EngineError {
    EngineError::Storage(StorageError::Other(format!(
        "the backend of bucket '{bucket}' answered that it has no conditional writes \
         (501); the rewrite is not stored without its condition"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_hex_shapes() {
        assert!(is_md5_hex("d41d8cd98f00b204e9800998ecf8427e"));
        assert!(!is_md5_hex("d41d8cd98f00b204e9800998ecf8427e-3"));
        assert!(!is_md5_hex(""));
        assert!(!is_md5_hex("zz1d8cd98f00b204e9800998ecf8427e"));
    }
}
