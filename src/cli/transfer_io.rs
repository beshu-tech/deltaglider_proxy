// SPDX-License-Identifier: BUSL-1.1

//! Bounded-memory object transfer for the `s3` verbs (`cp`, `sync`,
//! `migrate`, `verify`).
//!
//! A body above [`BUFFERED_MAX`] never sits whole in RAM: an upload is
//! copied into an engine spool file and stored with
//! `store_spooled_delta`; a download writes the `retrieve_stream` chunks
//! to disk; an S3-to-S3 copy spools the source stream. The engine's own
//! size ceilings still apply (`max_object_size` for a delta-eligible
//! object, `max_passthrough_object_size` for the rest).

use crate::deltaglider::{DynEngine, EngineError, RetrieveResponse};
use crate::types::{FileMetadata, StoreResult};
use futures::StreamExt;
use std::collections::HashMap;
use std::path::Path;
use tokio::io::AsyncWriteExt;

/// Bodies up to this size take the buffered engine path: a spool copy
/// costs more than it saves for a small file.
pub(crate) const BUFFERED_MAX: u64 = 8 * 1024 * 1024;

/// Where a transfer failed. The verbs map each side to their own
/// message and exit code.
#[derive(Debug)]
pub(crate) enum TransferError {
    /// Reading the local source file.
    LocalRead(std::io::Error),
    /// Writing the local destination file.
    LocalWrite(std::io::Error),
    /// Reading the S3 source (retrieve or its stream).
    Source(EngineError),
    /// Storing at the S3 destination (spool or store).
    Dest(EngineError),
    /// The source size differs from what its metadata or `stat` said:
    /// it changed while it was read.
    SourceChanged { expected: u64, observed: u64 },
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LocalRead(e) => write!(f, "read failed: {e}"),
            Self::LocalWrite(e) => write!(f, "write failed: {e}"),
            Self::Source(e) => write!(f, "source fetch failed: {e}"),
            Self::Dest(e) => write!(
                f,
                "destination put failed: {}",
                super::engine_factory::render_store_error(e)
            ),
            Self::SourceChanged { expected, observed } => write!(
                f,
                "source changed while it was read (expected {expected} bytes, read {observed})"
            ),
        }
    }
}

/// Store the local file at `bucket/key`.
pub(crate) async fn upload_file(
    engine: &DynEngine,
    bucket: &str,
    key: &str,
    local: &Path,
    content_type: Option<String>,
    user_metadata: HashMap<String, String>,
) -> Result<StoreResult, TransferError> {
    let size = tokio::fs::metadata(local)
        .await
        .map_err(TransferError::LocalRead)?
        .len();
    if size <= BUFFERED_MAX {
        let data = tokio::fs::read(local)
            .await
            .map_err(TransferError::LocalRead)?;
        return engine
            .store(bucket, key, &data, content_type, user_metadata)
            .await
            .map_err(TransferError::Dest);
    }
    let spool = engine
        .spool_acquire(size)
        .await
        .map_err(TransferError::Dest)?;
    let copied = tokio::fs::copy(local, spool.path())
        .await
        .map_err(TransferError::LocalRead)?;
    if copied != size {
        return Err(TransferError::SourceChanged {
            expected: size,
            observed: copied,
        });
    }
    engine
        .store_spooled_delta(bucket, key, &spool, size, content_type, user_metadata, None)
        .await
        .map_err(TransferError::Dest)
}

/// Write `bucket/key` to the local path `dst`. The bytes go to a
/// sibling temp file that is renamed over `dst` at the end, so a failed
/// stream never leaves a truncated `dst`.
pub(crate) async fn download_file(
    engine: &DynEngine,
    bucket: &str,
    key: &str,
    dst: &Path,
) -> Result<FileMetadata, TransferError> {
    let resp = engine
        .retrieve_stream(bucket, key)
        .await
        .map_err(TransferError::Source)?;
    let partial = partial_path(dst);
    let result = write_response(resp, &partial).await;
    match result {
        Ok(meta) => {
            tokio::fs::rename(&partial, dst)
                .await
                .map_err(TransferError::LocalWrite)?;
            Ok(meta)
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&partial).await;
            Err(e)
        }
    }
}

/// `.<name>.dgp-partial-<pid>` beside `dst`.
fn partial_path(dst: &Path) -> std::path::PathBuf {
    let name = dst
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dst.with_file_name(format!(".{name}.dgp-partial-{}", std::process::id()))
}

async fn write_response(
    resp: RetrieveResponse,
    path: &Path,
) -> Result<FileMetadata, TransferError> {
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(TransferError::LocalWrite)?;
    let metadata = match resp {
        RetrieveResponse::Buffered { data, metadata, .. } => {
            file.write_all(&data)
                .await
                .map_err(TransferError::LocalWrite)?;
            metadata
        }
        RetrieveResponse::Streamed {
            mut stream,
            metadata,
            ..
        } => {
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| TransferError::Source(EngineError::Storage(e)))?;
                file.write_all(&chunk)
                    .await
                    .map_err(TransferError::LocalWrite)?;
            }
            metadata
        }
    };
    file.flush().await.map_err(TransferError::LocalWrite)?;
    Ok(metadata)
}

/// SHA-256 (hex) and byte count of `bucket/key`, read as a stream.
pub(crate) async fn hash_object(
    engine: &DynEngine,
    bucket: &str,
    key: &str,
) -> Result<(String, u64, FileMetadata), EngineError> {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    let (size, metadata) = match engine.retrieve_stream(bucket, key).await? {
        RetrieveResponse::Buffered { data, metadata, .. } => {
            hasher.update(&data);
            (data.len() as u64, metadata)
        }
        RetrieveResponse::Streamed {
            mut stream,
            metadata,
            ..
        } => {
            let mut size = 0u64;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(EngineError::Storage)?;
                size += chunk.len() as u64;
                hasher.update(&chunk);
            }
            (size, metadata)
        }
    };
    Ok((hex::encode(hasher.finalize()), size, metadata))
}

/// Copy `src_bucket/src_key` (read through `src`) to
/// `dst_bucket/dst_key` (written through `dst`; the same engine for
/// `cp` and `sync`, two engines for `migrate`). `dest_attrs` turns the
/// source metadata into the destination content type and user metadata.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn copy_object(
    src: &DynEngine,
    src_bucket: &str,
    src_key: &str,
    dst: &DynEngine,
    dst_bucket: &str,
    dst_key: &str,
    dest_attrs: impl FnOnce(&FileMetadata) -> (Option<String>, HashMap<String, String>),
) -> Result<StoreResult, TransferError> {
    let resp = src
        .retrieve_stream(src_bucket, src_key)
        .await
        .map_err(TransferError::Source)?;
    let (mut stream, metadata) = match resp {
        RetrieveResponse::Buffered { data, metadata, .. } => {
            let (ct, meta) = dest_attrs(&metadata);
            return dst
                .store(dst_bucket, dst_key, &data, ct, meta)
                .await
                .map_err(TransferError::Dest);
        }
        RetrieveResponse::Streamed {
            stream, metadata, ..
        } => (stream, metadata),
    };
    let size = metadata.file_size;
    let (ct, meta) = dest_attrs(&metadata);
    if size <= BUFFERED_MAX {
        let mut data = Vec::with_capacity(size as usize);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| TransferError::Source(EngineError::Storage(e)))?;
            if data.len() as u64 + chunk.len() as u64 > size {
                return Err(TransferError::SourceChanged {
                    expected: size,
                    observed: data.len() as u64 + chunk.len() as u64,
                });
            }
            data.extend_from_slice(&chunk);
        }
        return dst
            .store(dst_bucket, dst_key, &data, ct, meta)
            .await
            .map_err(TransferError::Dest);
    }
    let spool = dst.spool_acquire(size).await.map_err(TransferError::Dest)?;
    use crate::deltaglider::spool::SpoolFillError;
    let changed = |observed| TransferError::SourceChanged {
        expected: size,
        observed,
    };
    let fill = spool
        .fill_from_stream(&mut stream, size)
        .await
        .map_err(|e| match e {
            SpoolFillError::Source(e) => TransferError::Source(EngineError::Storage(e)),
            SpoolFillError::Write(e) => TransferError::Dest(EngineError::Storage(e.into())),
            SpoolFillError::Overrun { written, .. } => changed(written),
        })?;
    if fill.written != size {
        return Err(changed(fill.written));
    }
    dst.store_spooled_delta(dst_bucket, dst_key, &spool, size, ct, meta, None)
        .await
        .map_err(TransferError::Dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::DynStorageBackend;
    use std::sync::Arc;

    async fn fs_engine(dir: &Path) -> DynEngine {
        let backend: Box<crate::storage::DynStorageBackend<'static>> = DynStorageBackend::new_box(
            crate::storage::FilesystemBackend::new(dir.to_path_buf())
                .await
                .unwrap(),
        );
        let engine = crate::deltaglider::DeltaGliderEngine::new_with_backend(
            Arc::new(backend),
            &crate::config::Config::default(),
            None,
        );
        engine.create_bucket("b").await.unwrap();
        engine
    }

    fn body(len: usize, seed: u32) -> Vec<u8> {
        (0..len as u32)
            .map(|i| ((i.wrapping_mul(2654435761) ^ seed) >> 7) as u8)
            .collect()
    }

    /// Two delta-eligible versions above `BUFFERED_MAX` take the spool
    /// path on upload (the second one as a delta), and both download
    /// and copy back byte-exact.
    #[tokio::test]
    async fn large_delta_eligible_files_round_trip_through_the_spool() {
        let store = tempfile::tempdir().unwrap();
        let local = tempfile::tempdir().unwrap();
        let engine = fs_engine(store.path()).await;
        let len = BUFFERED_MAX as usize + 1024 * 1024;
        let v1 = body(len, 1);
        let mut v2 = v1.clone();
        v2[len / 2..len / 2 + 4096].fill(7);
        for (name, bytes) in [("v1.zip", &v1), ("v2.zip", &v2)] {
            let p = local.path().join(name);
            std::fs::write(&p, bytes).unwrap();
            upload_file(
                &engine,
                "b",
                &format!("rel/{name}"),
                &p,
                None,
                HashMap::new(),
            )
            .await
            .unwrap();
        }
        let head = engine.head("b", "rel/v2.zip").await.unwrap();
        assert!(
            head.is_delta(),
            "the second version must be stored as a delta: {:?}",
            head.storage_info
        );
        let out = local.path().join("out.zip");
        download_file(&engine, "b", "rel/v2.zip", &out)
            .await
            .unwrap();
        assert!(std::fs::read(&out).unwrap() == v2, "download differs");
        copy_object(
            &engine,
            "b",
            "rel/v2.zip",
            &engine,
            "b",
            "copy/v2.zip",
            |m| (m.content_type.clone(), HashMap::new()),
        )
        .await
        .unwrap();
        let (sha, size, _) = hash_object(&engine, "b", "copy/v2.zip").await.unwrap();
        assert_eq!(size, len as u64);
        assert_eq!(
            sha,
            hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&v2))
        );
        assert!(
            std::fs::read_dir(local.path()).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("dgp-partial")),
            "a partial file stays behind"
        );
    }

    /// A failed download leaves neither a partial file nor a changed `dst`.
    #[tokio::test]
    async fn a_failed_download_keeps_the_old_destination() {
        let store = tempfile::tempdir().unwrap();
        let local = tempfile::tempdir().unwrap();
        let engine = fs_engine(store.path()).await;
        let dst = local.path().join("keep.bin");
        std::fs::write(&dst, b"old").unwrap();
        let err = download_file(&engine, "b", "missing.bin", &dst)
            .await
            .unwrap_err();
        assert!(
            matches!(err, TransferError::Source(ref e) if e.is_not_found()),
            "{err}"
        );
        assert_eq!(std::fs::read(&dst).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(local.path()).unwrap().count(), 1);
    }

    /// Guard for the class: the verbs that move object bodies go through
    /// this module, never through the whole-body engine calls.
    #[test]
    fn verbs_move_bodies_through_transfer_io() {
        for (name, src) in [
            ("cp.rs", include_str!("cp.rs")),
            ("sync.rs", include_str!("sync.rs")),
            ("migrate.rs", include_str!("migrate.rs")),
            ("verify.rs", include_str!("verify.rs")),
        ] {
            let code = &crate::source_scan::prod_text(src);
            for bad in [".retrieve(", ".store(", "fs::read(", "fs::write("] {
                assert!(!code.contains(bad), "{name} moves a whole body: `{bad}`");
            }
        }
    }
}
