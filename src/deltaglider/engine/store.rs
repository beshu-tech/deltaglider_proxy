// SPDX-License-Identifier: BUSL-1.1

//! Store pipeline — delta encoding, passthrough, and baseline management.

use super::usage::CounterPrior;
use super::*;
use crate::deltaglider::spool::SpoolBudget;
use crate::storage::{MultipartUpload, StorageBackend, UploadedPart};
use md5::{Digest, Md5};
use sha2::Sha256;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio::io::AsyncReadExt;

/// In-progress streaming passthrough multipart upload (Phase B). Holds the
/// per-deltaspace lock for the upload's lifetime. The handle is IMMUTABLE
/// during part uploads so the caller can drive parts concurrently; it
/// collects the small [`UploadedPart`] receipts itself. Whole-object hashes
/// are taken from the copy source (a copy doesn't recompute them), so no
/// in-order hashing is needed and memory stays O(in-flight parts).
pub struct PassthroughMultipartHandle {
    bucket: String,
    key: String,
    deltaspace_id: String,
    filename: String,
    total_size: u64,
    /// The object's final metadata, stored when the upload is created.
    metadata: FileMetadata,
    upload: MultipartUpload,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

/// The facts of an object that a streaming multipart copy writes, all known
/// from the copy source before the first part. The backend must store them
/// when it creates the upload: a native S3 complete stores no metadata.
pub struct MultipartObjectFacts {
    pub total_size: u64,
    pub content_type: Option<String>,
    pub user_metadata: HashMap<String, String>,
    pub sha256: String,
    pub md5: String,
    pub multipart_etag: Option<String>,
}

/// A body spool with the hashes that the streaming PUT computed from it
/// (and checked against the declared size), so no later step reads the
/// body again to hash it.
#[derive(Clone, Copy)]
struct HashedSpool<'a> {
    spool: &'a crate::deltaglider::spool::Spool,
    sha256: &'a str,
    md5: &'a str,
}

impl PassthroughMultipartHandle {
    /// Whether the backend writes parts durably & incrementally (S3) — the
    /// caller may drop part bytes after each `upload_passthrough_part`.
    pub fn native(&self) -> bool {
        self.upload.native
    }
}

/// What the encode of a delta-eligible PUT produced, as [`StorePlan::decide`]
/// sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EncodeOutcome {
    /// A complete delta of `delta_len` bytes.
    Encoded { delta_len: u64 },
    /// The streaming encode stopped at the ratio cap: the delta loses.
    OverCap,
    /// No spool budget for the streaming encode now (a holder never waits).
    NoSpool,
}

/// How a PUT is stored. Pure: both delta-eligible PUT paths (buffered and
/// streaming) decide through [`Self::tries_delta`] and [`Self::decide`], and
/// commit through one pipeline (`store_delta_eligible`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StorePlan {
    /// Keep the delta.
    Delta,
    /// Store the body as it is. `remove_fresh_baseline`: this PUT created
    /// the baseline, and no delta uses it.
    Passthrough { remove_fresh_baseline: bool },
}

impl StorePlan {
    /// Whether a PUT tries a delta at all. No: store passthrough, with no
    /// baseline.
    pub(crate) fn tries_delta(
        compression_enabled: bool,
        delta_eligible: bool,
        no_delta_requested: bool,
    ) -> bool {
        compression_enabled && delta_eligible && !no_delta_requested
    }

    /// After the encode. S-P1-1: the ratio is checked on EVERY PUT, not only
    /// the first one in the deltaspace. A poor delta is stored passthrough;
    /// a baseline that other deltas use stays, and a baseline this PUT
    /// created is removed (the next PUT may bring a useful one).
    pub(crate) fn decide(
        size: u64,
        outcome: EncodeOutcome,
        max_ratio: f32,
        fresh_baseline: bool,
    ) -> Self {
        let keep = match outcome {
            EncodeOutcome::Encoded { delta_len } => {
                DeltaCodec::compression_ratio(size as usize, delta_len as usize) < max_ratio
            }
            EncodeOutcome::OverCap | EncodeOutcome::NoSpool => false,
        };
        if keep {
            StorePlan::Delta
        } else {
            StorePlan::Passthrough {
                remove_fresh_baseline: fresh_baseline,
            }
        }
    }
}

/// A PUT body: in RAM (the buffered PUT) or on a spool file that the caller
/// holds (the streaming PUT).
#[derive(Clone, Copy)]
enum PutBody<'a> {
    Buffered(&'a [u8]),
    Spooled(&'a crate::deltaglider::spool::Spool),
}

impl PutBody<'_> {
    /// The spool this op holds already, if any.
    fn held(&self) -> Option<&crate::deltaglider::spool::Spool> {
        match self {
            PutBody::Spooled(s) => Some(s),
            PutBody::Buffered(_) => None,
        }
    }
}

/// One delta-eligible PUT through the store pipeline.
struct PutObject<'a> {
    bucket: &'a str,
    key: &'a str,
    obj_key: &'a ObjectKey,
    deltaspace_id: &'a str,
    body: PutBody<'a>,
    size: u64,
    sha256: String,
    md5: String,
    content_type: Option<String>,
    user_metadata: HashMap<String, String>,
    /// When `Some`, the persisted `FileMetadata.multipart_etag` is stamped
    /// with this value so later HEAD/GET/LIST return the ETag that the
    /// CompleteMultipartUpload response gave (H1). A single PUT passes
    /// `None` and gets the full-body-MD5 ETag.
    multipart_etag: Option<String>,
    /// What the key held before: the commit records the counter against it.
    prior: &'a CounterPrior,
}

impl PutObject<'_> {
    fn passthrough_write(&self) -> PassthroughWrite<'_> {
        PassthroughWrite {
            bucket: self.bucket,
            key: self.key,
            deltaspace_id: self.deltaspace_id,
            prior: self.prior,
            metadata: passthrough_metadata(
                &self.obj_key.filename,
                self.sha256.clone(),
                self.md5.clone(),
                self.size,
                self.content_type.clone(),
                self.user_metadata.clone(),
                self.multipart_etag.clone(),
            ),
            source: match self.body {
                PutBody::Buffered(data) => PassthroughSource::Bytes(data),
                PutBody::Spooled(spool) => PassthroughSource::File {
                    path: spool.path(),
                    held: Some(spool),
                },
            },
        }
    }
}

/// The metadata of a passthrough object.
#[allow(clippy::too_many_arguments)]
fn passthrough_metadata(
    filename: &str,
    sha256: String,
    md5: String,
    size: u64,
    content_type: Option<String>,
    user_metadata: HashMap<String, String>,
    multipart_etag: Option<String>,
) -> FileMetadata {
    let mut metadata =
        FileMetadata::new_passthrough(filename.to_string(), sha256, md5, size, content_type);
    metadata.set_user_metadata(user_metadata);
    metadata.multipart_etag = multipart_etag;
    metadata
}

/// Where a passthrough write reads its bytes.
#[derive(Clone, Copy)]
enum PassthroughSource<'a> {
    Bytes(&'a [u8]),
    /// A file; `held` is the spool it lives on when the op holds one.
    File {
        path: &'a Path,
        held: Option<&'a crate::deltaglider::spool::Spool>,
    },
}

/// One passthrough write (`write_passthrough_locked`).
struct PassthroughWrite<'a> {
    bucket: &'a str,
    key: &'a str,
    deltaspace_id: &'a str,
    /// What the key held before (see [`PutObject::prior`]).
    prior: &'a CounterPrior,
    metadata: FileMetadata,
    source: PassthroughSource<'a>,
}

/// The delta the encode produced, where it lives until the commit.
enum EncodedDelta {
    InRam(Vec<u8>),
    /// On the delta spool of the (ref, delta) pair.
    Spooled {
        _ref_spool: crate::deltaglider::spool::Spool,
        delta_spool: crate::deltaglider::spool::Spool,
    },
    /// No delta to keep.
    None,
}

impl EncodedDelta {
    /// The delta bytes, for the commit (a kept delta is below the cap).
    async fn into_bytes(self) -> Result<Vec<u8>, EngineError> {
        match self {
            EncodedDelta::InRam(delta) => Ok(delta),
            EncodedDelta::Spooled { delta_spool, .. } => Ok(tokio::fs::read(delta_spool.path())
                .await
                .map_err(StorageError::from)?),
            EncodedDelta::None => Err(EngineError::Storage(StorageError::Other(
                "store plan kept a delta that the encode did not produce".into(),
            ))),
        }
    }
}

/// Store an object with automatic delta compression
impl<S: StorageBackend> DeltaGliderEngine<S> {
    #[instrument(skip(self, data, user_metadata))]
    pub async fn store(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        content_type: Option<String>,
        user_metadata: std::collections::HashMap<String, String>,
    ) -> Result<StoreResult, EngineError> {
        // The counter is recorded where the object commits (`record_commit`).
        self.store_inner(bucket, key, data, content_type, user_metadata, None)
            .await
    }

    /// Multipart-aware variant of [`Self::store`]. The `multipart_etag` is
    /// persisted alongside the object so HEAD/GET/LIST return it verbatim
    /// (H1 correctness fix). All other semantics are identical.
    #[instrument(skip(self, data, user_metadata, multipart_etag))]
    #[allow(clippy::too_many_arguments)]
    pub async fn store_with_multipart_etag(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        content_type: Option<String>,
        user_metadata: std::collections::HashMap<String, String>,
        multipart_etag: String,
    ) -> Result<StoreResult, EngineError> {
        self.store_inner(
            bucket,
            key,
            data,
            content_type,
            user_metadata,
            Some(multipart_etag),
        )
        .await
    }

    /// `PUT photos/` with an empty body (review D3): a zero-byte folder
    /// marker, the object S3 clients create for an empty folder. The backend
    /// stores it as-is (never encrypted, never a delta), so an S3 listing
    /// still shows it as a zero-byte `photos/`.
    async fn store_directory_marker(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<StoreResult, EngineError> {
        // The data PUT's ingest gate (reserved namespaces, `//`), minus its
        // refusal of marker keys.
        let obj_key = ObjectKey::parse(bucket, key);
        obj_key
            .validate_ingest()
            .map_err(|e| EngineError::InvalidArgument(e.to_string()))?;
        let deltaspace_id = obj_key.deltaspace_id();
        let prior = self.prior_for_counter(bucket, key).await;
        let _guard = self.acquire_prefix_lock(bucket, &deltaspace_id).await;
        self.storage
            .put_directory_marker(bucket, &obj_key.full_key())
            .await?;
        let metadata = FileMetadata::directory_marker(&obj_key.full_key());
        self.record_commit(bucket, &prior, &metadata);
        self.metadata_cache.insert(bucket, key, metadata.clone());
        Ok(StoreResult::new(metadata, 0).with_accounting(prior.replaced(), 0))
    }

    #[allow(clippy::too_many_arguments)]
    async fn store_inner(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        content_type: Option<String>,
        user_metadata: std::collections::HashMap<String, String>,
        multipart_etag: Option<String>,
    ) -> Result<StoreResult, EngineError> {
        // Invalidate stale metadata on overwrite (before the write, so concurrent
        // readers don't see outdated metadata during the write window).
        self.metadata_cache.invalidate(bucket, key);

        // Check size limit
        if data.len() as u64 > self.max_object_size {
            return Err(EngineError::TooLarge {
                size: data.len() as u64,
                max: self.max_object_size,
            });
        }

        if data.is_empty() && ObjectKey::parse(bucket, key).is_directory_marker() {
            return self.store_directory_marker(bucket, key).await;
        }
        let (obj_key, deltaspace_id) = self.validated_key_ingest(bucket, key)?;

        // Usage-counter accounting: capture the PRIOR object metadata (S3 PUT is
        // an upsert) so the counter nets an overwrite to +0 instead of double-
        // counting. Small TOCTOU window vs the write below is acceptable — the
        // counter is best-effort and reconciled by Refresh.
        let prior = self.prior_for_counter(bucket, key).await;

        // Calculate hashes
        let sha256 = hex::encode(Sha256::digest(data));
        let md5 = hex::encode(Md5::digest(data));

        info!(
            "Storing {}/{} ({} bytes, sha256={})",
            bucket,
            key,
            data.len(),
            &sha256[..8]
        );

        let put = PutObject {
            bucket,
            key,
            obj_key: &obj_key,
            deltaspace_id: &deltaspace_id,
            body: PutBody::Buffered(data),
            size: data.len() as u64,
            sha256,
            md5,
            content_type,
            user_metadata,
            multipart_etag,
            prior: &prior,
        };
        if !self.tries_delta(bucket, &obj_key, &put.user_metadata) {
            let _guard = self.acquire_prefix_lock(bucket, &deltaspace_id).await;
            let result = self
                .write_passthrough_locked(put.passthrough_write(), None)
                .await?;
            self.count_decision("passthrough");
            // Passthrough creates no reference baseline; only overwrite-net.
            return Ok(result.with_accounting(prior.replaced(), 0));
        }
        self.store_delta_eligible(put).await
    }

    /// Whether a write of `key` into `bucket` with `user_metadata` tries a
    /// delta (the `StorePlan::tries_delta` rule). Multipart completion asks before it
    /// assembles the parts; counts nothing.
    pub fn write_tries_delta(
        &self,
        bucket: &str,
        key: &str,
        user_metadata: &HashMap<String, String>,
    ) -> bool {
        StorePlan::tries_delta(
            self.bucket_policies.compression_enabled(bucket),
            self.is_delta_eligible(key),
            crate::types::no_delta_requested(user_metadata),
        )
    }

    /// [`StorePlan::tries_delta`] for this bucket and key. The caller counts
    /// the passthrough decision once the write commits.
    fn tries_delta(
        &self,
        bucket: &str,
        obj_key: &ObjectKey,
        user_metadata: &HashMap<String, String>,
    ) -> bool {
        let compression = self.bucket_policies.compression_enabled(bucket);
        let eligible = self.file_router.is_delta_eligible(&obj_key.filename);
        let no_delta = crate::types::no_delta_requested(user_metadata);
        if StorePlan::tries_delta(compression, eligible, no_delta) {
            return true;
        }
        if no_delta {
            debug!("The PUT asks for no delta, storing as passthrough");
        } else if compression {
            debug!("File type not delta-eligible, storing as passthrough");
        } else {
            debug!("Compression disabled for bucket '{bucket}', storing as passthrough");
        }
        false
    }

    fn count_decision(&self, decision: &str) {
        self.with_metrics(|m| m.delta_decisions_total.with_label_values(&[decision]).inc());
    }

    /// STREAMING delta PUT (Phase 4): store a large delta-eligible object whose
    /// body is already on a seekable spool file, WITHOUT buffering it in RAM.
    ///
    /// Memory is bounded by the codec pump (Spike C: 2MB RSS on a 1.5GB target).
    /// Flow:
    /// 1. Hash the body by streaming the spool (sha256 + md5) — no full-RAM read.
    /// 2. If not delta-eligible / compression off → passthrough straight from
    ///    the body spool (store_passthrough_file).
    /// 3. Else: the same pipeline as the buffered PUT
    ///    (`Self::store_delta_eligible`), with the encode reading the body
    ///    spool, capped at `ratio_threshold × size`.
    ///
    /// The caller owns `body` (a `Spool`); it lives until this returns.
    #[allow(clippy::too_many_arguments)]
    pub async fn store_spooled_delta(
        &self,
        bucket: &str,
        key: &str,
        body: &crate::deltaglider::spool::Spool,
        size: u64,
        content_type: Option<String>,
        user_metadata: std::collections::HashMap<String, String>,
        multipart_etag: Option<String>,
    ) -> Result<StoreResult, EngineError> {
        self.store_spooled_delta_inner(
            bucket,
            key,
            body,
            size,
            content_type,
            user_metadata,
            multipart_etag,
        )
        .await
    }

    /// Body of [`Self::store_spooled_delta`]. The commit records the counter;
    /// this attaches the accounting to the result.
    #[allow(clippy::too_many_arguments)]
    async fn store_spooled_delta_inner(
        &self,
        bucket: &str,
        key: &str,
        body: &crate::deltaglider::spool::Spool,
        size: u64,
        content_type: Option<String>,
        user_metadata: std::collections::HashMap<String, String>,
        multipart_etag: Option<String>,
    ) -> Result<StoreResult, EngineError> {
        self.metadata_cache.invalidate(bucket, key);
        let (obj_key, deltaspace_id) = self.validated_key_ingest(bucket, key)?;
        // Size ceiling depends on the STRATEGY: a delta-eligible object is
        // bounded by max_object_size (it will be xdelta3-encoded in RAM); a
        // passthrough object streams from the spool and is bounded by the far
        // larger max_passthrough_object_size. Applying the delta limit to
        // passthrough objects made every spooled passthrough copy fail
        // TooLarge under default config (finding #3).
        let is_passthrough = !StorePlan::tries_delta(
            self.bucket_policies.compression_enabled(bucket),
            self.file_router.is_delta_eligible(&obj_key.filename),
            crate::types::no_delta_requested(&user_metadata),
        );
        let ceiling = if is_passthrough {
            self.max_passthrough_object_size
        } else {
            self.max_object_size
        };
        if size > ceiling {
            return Err(EngineError::TooLarge { size, max: ceiling });
        }
        let prior = self.prior_for_counter(bucket, key).await;

        // (1) Hash the body by streaming the spool — bounded memory. Also count
        // the observed bytes and reject a spool that doesn't match the declared
        // `size` (source overwritten mid-stream, or a stale/corrupt xattr
        // file_size on a filesystem source) — else we'd stamp file_size=declared
        // over a sha256 of DIFFERENT bytes. Matches the multipart-relay guard.
        let (sha256, md5, observed) = Self::hash_spool_file(body.path()).await?;
        if observed != size {
            return Err(EngineError::Storage(StorageError::Other(format!(
                "Spooled object size mismatch: declared {size}, observed {observed}"
            ))));
        }

        // (2) Not delta-eligible → passthrough from the body spool.
        if !self.tries_delta(bucket, &obj_key, &user_metadata) {
            let result = self
                .store_passthrough_file_inner(
                    bucket,
                    key,
                    body.path(),
                    Some(HashedSpool {
                        spool: body,
                        sha256: &sha256,
                        md5: &md5,
                    }),
                    size,
                    content_type,
                    user_metadata,
                    multipart_etag,
                    &prior,
                )
                .await?;
            self.count_decision("passthrough");
            return Ok(result.with_accounting(prior.replaced(), 0));
        }

        // (3) The pipeline the buffered PUT uses, reading the body spool.
        let put = PutObject {
            bucket,
            key,
            obj_key: &obj_key,
            deltaspace_id: &deltaspace_id,
            body: PutBody::Spooled(body),
            size,
            sha256,
            md5,
            content_type,
            user_metadata,
            multipart_etag,
            prior: &prior,
        };
        self.store_delta_eligible(put).await
    }

    /// THE delta-eligible PUT, for both body forms: under both deltaspace
    /// locks it finds or creates the baseline, encodes, lets
    /// [`StorePlan::decide`] choose, and commits. One rule for a baseline
    /// that this PUT created (S-P1-2): it stays only when the PUT commits a
    /// delta against it. A lost ratio or a failure removes it, under the
    /// same locks, so no other PUT can have used it in between.
    async fn store_delta_eligible(&self, put: PutObject<'_>) -> Result<StoreResult, EngineError> {
        let (bucket, deltaspace_id, prior) = (put.bucket, put.deltaspace_id, put.prior);
        // The critical section: has_reference check → set_reference → store_delta
        // must be atomic per-prefix to avoid two writers both creating a reference.
        // The in-process mutex serializes same-node threads; the cross-instance
        // lock (multi-instance only, inert otherwise) serializes across nodes
        // (B1, see CLAUDE.md HA contract). The passthrough write below runs
        // under the same locks: it never re-acquires them.
        let _guard = self.acquire_prefix_lock(bucket, deltaspace_id).await;
        let xnode = self.acquire_reference_lock(bucket, deltaspace_id).await?;

        // A backend error here must ABORT the PUT — never fall through to the
        // "create baseline" branch, which would overwrite a reference.bin that
        // may exist and orphan every sibling delta.
        let has_existing_reference = match xnode.observed_reference() {
            Some(seen) => seen,
            None => self.storage.has_reference(bucket, deltaspace_id).await?,
        };
        let ref_meta = if has_existing_reference {
            let read = self
                .storage
                .get_reference_metadata(bucket, deltaspace_id)
                .await?;
            // Heal a stripped-metadata reference in place (same bytes) so the
            // delta we write next carries a valid ref_sha256 and replication
            // stops re-copying this deltaspace. No-op (zero I/O) when healthy.
            self.heal_reference_if_corrupt(bucket, deltaspace_id, read, put.body.held(), &xnode)
                .await?
        } else {
            debug!("No reference in deltaspace, creating baseline");
            self.write_baseline(&put, &xnode).await?
        };
        let fresh_baseline = !has_existing_reference;

        match self
            .encode_and_commit(put, &ref_meta, fresh_baseline, &xnode)
            .await
        {
            Ok((result, baseline_kept)) => {
                // A seeded reference's bytes count (symmetric with delete's
                // reclamation subtraction); a removed one does not. The
                // counter has both already: each write recorded its own.
                let created = if fresh_baseline && baseline_kept {
                    ref_meta.file_size
                } else {
                    0
                };
                Ok(result.with_accounting(prior.replaced(), created))
            }
            Err(e) => {
                if fresh_baseline {
                    debug!("S-P1-2: store failed ({e}); removing the fresh baseline");
                    if self
                        .remove_fresh_baseline(bucket, deltaspace_id, &xnode)
                        .await
                    {
                        self.record_reference(bucket, -(ref_meta.file_size as i64));
                    }
                }
                Err(e)
            }
        }
    }

    /// Encode against the reference, decide, commit. Returns the result and
    /// whether the baseline is still in place.
    async fn encode_and_commit(
        &self,
        put: PutObject<'_>,
        ref_meta: &FileMetadata,
        fresh_baseline: bool,
        xnode: &super::ReferenceLockGuard,
    ) -> Result<(StoreResult, bool), EngineError> {
        let max_ratio = self.bucket_policies.max_delta_ratio(put.bucket);
        let (outcome, delta) = match put.body {
            PutBody::Buffered(data) => self.encode_buffered(&put, data, ref_meta).await?,
            PutBody::Spooled(body) => {
                self.encode_spooled(&put, body, ref_meta, fresh_baseline, max_ratio)
                    .await?
            }
        };
        if let EncodeOutcome::Encoded { delta_len } = outcome {
            let ratio = DeltaCodec::compression_ratio(put.size as usize, delta_len as usize);
            self.with_metrics(|m| m.delta_compression_ratio.observe(ratio as f64));
            info!(
                "Delta computed: {} bytes -> {} bytes (ratio: {:.2}%)",
                put.size,
                delta_len,
                ratio * 100.0
            );
        }
        match StorePlan::decide(put.size, outcome, max_ratio, fresh_baseline) {
            StorePlan::Delta => {
                let delta = delta.into_bytes().await?;
                let result = self
                    .commit_delta(&put, ref_meta, delta, fresh_baseline, xnode)
                    .await?;
                Ok((result, true))
            }
            StorePlan::Passthrough {
                remove_fresh_baseline,
            } => {
                debug!(
                    "Delta for {}/{} not kept ({outcome:?}, max ratio {max_ratio:.2}); storing as passthrough",
                    put.bucket, put.key
                );
                // Free the (ref, delta) spools before the write reserves its own.
                drop(delta);
                // The streaming PUT holds its body spool, so this reservation
                // never waits (a holder never waits): safe under the lock.
                let reserved = match put.body {
                    PutBody::Spooled(body) => {
                        self.reserve_storage_spool(put.bucket, put.size, false, body.reserved_mib())
                            .await?
                    }
                    PutBody::Buffered(_) => None,
                };
                // Write passthrough FIRST, then clean up. This prevents
                // transient 404s on concurrent GETs during strategy
                // transition.
                let result = self
                    .write_passthrough_locked(put.passthrough_write(), reserved.as_ref())
                    .await?;
                self.count_decision("passthrough");
                let removed = remove_fresh_baseline
                    && self
                        .remove_fresh_baseline(put.bucket, put.deltaspace_id, xnode)
                        .await;
                if removed {
                    self.record_reference(put.bucket, -(ref_meta.file_size as i64));
                }
                Ok((result, !removed))
            }
        }
    }

    /// The buffered encode: the reference from the cache, the body in RAM.
    async fn encode_buffered(
        &self,
        put: &PutObject<'_>,
        data: &[u8],
        ref_meta: &FileMetadata,
    ) -> Result<(EncodeOutcome, EncodedDelta), EngineError> {
        let (reference, _cache_hit) = self
            .get_reference_cached(put.bucket, put.deltaspace_id, &ref_meta.file_sha256)
            .await?;
        // PERF: try_acquire instead of acquire — fail fast with 503 when all codec
        // slots are busy rather than queuing unbounded requests in memory (each
        // holding a full object body while waiting for a permit).
        // The source file first (spool before codec slot, as on every path).
        let source_file = self.codec_source_spool_now(reference.len())?;
        let _codec_permit = self.try_acquire_codec()?;
        // spawn_blocking: xdelta3 is CPU-bound; data must be owned ('static).
        let data_owned = data.to_vec();
        let codec = self.codec.clone();
        let encode_start = Instant::now();
        let delta = tokio::task::spawn_blocking(move || {
            codec.encode_spooled(&source_file, &reference, &data_owned)
        })
        .await
        .map_err(|e| {
            tracing::error!("Delta encode task panicked: {}", e);
            EngineError::Storage(StorageError::Other(format!("codec task panicked: {}", e)))
        })??;
        self.with_metrics(|m| {
            m.delta_encode_duration_seconds
                .observe(encode_start.elapsed().as_secs_f64())
        });
        let delta_len = delta.len() as u64;
        Ok((
            EncodeOutcome::Encoded { delta_len },
            EncodedDelta::InRam(delta),
        ))
    }

    /// The streaming encode: body spool → delta spool, stopped at the ratio
    /// cap. A fresh baseline IS the body (storage-9), so the encode reads it
    /// from the body spool and the ref spool stays empty.
    async fn encode_spooled(
        &self,
        put: &PutObject<'_>,
        body: &crate::deltaglider::spool::Spool,
        ref_meta: &FileMetadata,
        fresh_baseline: bool,
        max_ratio: f32,
    ) -> Result<(EncodeOutcome, EncodedDelta), EngineError> {
        // ONE timed, combined reservation for both spools (ref + delta) — two raw
        // sequential acquire()s self-deadlock when 2×size > budget, the exact
        // class the GET path uses acquire_pair to prevent (mega-review finding).
        // The ref spool holds the REFERENCE, which can be larger than this object
        // — reserve it at the reference's actual size so the byte-budget isn't
        // under-accounted under concurrency (→ ENOSPC).
        // A verified copy of the reference that a large delta GET of this
        // deltaspace downloaded lately (`range_spools`) saves the download.
        let shared_reference = (!fresh_baseline && !ref_meta.file_sha256.is_empty())
            .then(|| {
                self.range_spools.get_fresh(
                    &crate::deltaglider::range_spool::RangeSpoolKey::reference(
                        self.cache_key(put.bucket, put.deltaspace_id),
                        ref_meta.file_sha256.clone(),
                    ),
                )
            })
            .flatten();
        let ref_size = if fresh_baseline || shared_reference.is_some() {
            1
        } else {
            ref_meta.file_size
        };
        // Clamped beside the body spool this op already holds (else body +
        // pair > budget waited on itself for the whole acquire timeout).
        let Some((ref_spool, delta_spool)) = self
            .spool_acquire_pair_beside(body, ref_size, put.size)
            .await?
        else {
            // No budget free now. Waiting while this PUT holds its body could
            // deadlock with another PUT that waits on ours.
            debug!("streaming PUT {}/{}: spool contended", put.bucket, put.key);
            return Ok((EncodeOutcome::NoSpool, EncodedDelta::None));
        };
        let ref_path = if fresh_baseline {
            // Reading the reference back would cost a second transfer of
            // the object on S3.
            body.path().to_path_buf()
        } else if let Some(shared) = &shared_reference {
            shared.path().to_path_buf()
        } else {
            self.storage
                .get_reference_to_file(put.bucket, put.deltaspace_id, ref_spool.path())
                .await?;
            ref_spool.path().to_path_buf()
        };

        let cap = ((put.size as f64) * (max_ratio as f64)).ceil() as u64;
        let _permit = self.try_acquire_codec()?;
        let codec = self.codec.clone();
        let body_path = body.path().to_path_buf();
        let delta_path = delta_spool.path().to_path_buf();
        let encode_start = Instant::now();

        // Encode body→delta spool, aborting if the delta exceeds the cap (ratio
        // loses — Spike C). A capped-write error signals "passthrough wins".
        let encoded = tokio::task::spawn_blocking(move || -> Result<Option<u64>, EngineError> {
            use std::io::Write;
            struct CapWriter<W: Write> {
                inner: W,
                written: u64,
                cap: u64,
                capped: bool,
            }
            impl<W: Write> Write for CapWriter<W> {
                fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                    self.written = self.written.saturating_add(buf.len() as u64);
                    if self.written > self.cap {
                        self.capped = true;
                        return Err(std::io::Error::other("delta exceeded ratio cap"));
                    }
                    self.inner.write_all(buf)?;
                    Ok(buf.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    self.inner.flush()
                }
            }
            let body = std::fs::File::open(&body_path)
                .map_err(|e| EngineError::Storage(StorageError::from(e)))?;
            let out = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&delta_path)
                .map_err(|e| EngineError::Storage(StorageError::from(e)))?;
            let mut sink = CapWriter {
                inner: std::io::BufWriter::new(out),
                written: 0,
                cap,
                capped: false,
            };
            match codec.encode_from_reader(&ref_path, body, &mut sink) {
                Ok(n) => {
                    sink.flush()
                        .map_err(|e| EngineError::Storage(StorageError::from(e)))?;
                    Ok(Some(n))
                }
                // Cap tripped → ratio loses; signal passthrough (None).
                Err(_) if sink.capped => Ok(None),
                Err(e) => Err(EngineError::Codec(e)),
            }
        })
        .await
        .map_err(|e| EngineError::Storage(StorageError::Other(format!("encode task: {e}"))))??;
        drop(_permit);
        self.with_metrics(|m| {
            m.delta_encode_duration_seconds
                .observe(encode_start.elapsed().as_secs_f64())
        });
        Ok(match encoded {
            Some(delta_len) => (
                EncodeOutcome::Encoded { delta_len },
                EncodedDelta::Spooled {
                    _ref_spool: ref_spool,
                    delta_spool,
                },
            ),
            None => (EncodeOutcome::OverCap, EncodedDelta::None),
        })
    }

    /// Write this PUT's body as the deltaspace baseline.
    async fn write_baseline(
        &self,
        put: &PutObject<'_>,
        xnode: &super::ReferenceLockGuard,
    ) -> Result<FileMetadata, EngineError> {
        let metadata = FileMetadata::new_reference(
            Self::INTERNAL_REFERENCE_NAME.to_string(),
            put.obj_key.full_key(),
            put.sha256.clone(),
            put.md5.clone(),
            put.size,
            put.content_type.clone(),
        );
        let cache_key = self.cache_key(put.bucket, put.deltaspace_id);
        match put.body {
            PutBody::Buffered(data) => {
                xnode
                    .put_reference(
                        &*self.storage,
                        put.bucket,
                        put.deltaspace_id,
                        data,
                        &metadata,
                    )
                    .await?;
                self.record_reference(put.bucket, put.size as i64);
                self.cache
                    .put(&cache_key, Bytes::copy_from_slice(data), &put.sha256);
            }
            PutBody::Spooled(body) => {
                // Streamed into place from the spool (no heap-load — M1.4).
                xnode
                    .put_reference_from_file(
                        &*self.storage,
                        put.bucket,
                        put.deltaspace_id,
                        body.path(),
                        &metadata,
                    )
                    .await?;
                self.record_reference(put.bucket, put.size as i64);
                // Not pre-cached; the next GET loads it fresh.
                self.cache.invalidate(&cache_key);
            }
        }
        // The `reference` decision counts when a delta commits against it.
        Ok(metadata)
    }

    /// Best-effort removal of the baseline this PUT created. Errors are
    /// logged and do not mask the PUT's outcome. Returns whether it went.
    async fn remove_fresh_baseline(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        xnode: &super::ReferenceLockGuard,
    ) -> bool {
        self.cache
            .invalidate(&self.cache_key(bucket, deltaspace_id));
        match xnode
            .delete_reference(&*self.storage, bucket, deltaspace_id)
            .await
        {
            Ok(()) => true,
            Err(e) => {
                warn!("S-P1-2: fresh baseline of {bucket}/{deltaspace_id} not removed: {e}");
                false
            }
        }
    }

    /// Commit the encoded delta: write it (only valid against the reference
    /// we locked), then drop an older passthrough variant of the key.
    /// `fresh_baseline`: this PUT created the reference the delta uses.
    async fn commit_delta(
        &self,
        put: &PutObject<'_>,
        ref_meta: &FileMetadata,
        delta: Vec<u8>,
        fresh_baseline: bool,
        xnode: &super::ReferenceLockGuard,
    ) -> Result<StoreResult, EngineError> {
        let mut metadata = FileMetadata::new_delta(
            put.obj_key.filename.clone(),
            put.sha256.clone(),
            put.md5.clone(),
            put.size,
            "reference.bin".to_string(),
            ref_meta.file_sha256.clone(),
            delta.len() as u64,
            put.content_type.clone(),
        );
        metadata.set_user_metadata(put.user_metadata.clone());
        metadata.multipart_etag = put.multipart_etag.clone();
        xnode
            .put_delta(
                &*self.storage,
                put.bucket,
                put.deltaspace_id,
                &put.obj_key.filename,
                &delta,
                &metadata,
            )
            .await?;
        // Committed: count it before the next await. The metrics count only
        // a delta that committed (a failed commit counted its savings).
        self.record_commit(put.bucket, put.prior, &metadata);
        self.with_metrics(|m| {
            m.delta_decisions_total.with_label_values(&["delta"]).inc();
            if fresh_baseline {
                m.delta_decisions_total
                    .with_label_values(&["reference"])
                    .inc();
            }
            m.delta_bytes_saved_total
                .inc_by(put.size.saturating_sub(delta.len() as u64));
        });
        if let Err(e) = self
            .delete_passthrough_idempotent(put.bucket, put.deltaspace_id, &put.obj_key.filename)
            .await
        {
            warn!(
                "Failed to clean up old passthrough after delta write: {}",
                e
            );
        }
        self.metadata_cache
            .insert(put.bucket, put.key, metadata.clone());
        Ok(StoreResult::new(metadata, delta.len() as u64))
    }

    /// Write a passthrough object, then drop an older delta variant of the
    /// key. The caller holds the deltaspace prefix lock; this never takes it.
    /// `reserved`: the spool the storage write uses, reserved by the caller
    /// before the lock, or under it when the op holds a spool already (a
    /// holder never waits).
    async fn write_passthrough_locked(
        &self,
        write: PassthroughWrite<'_>,
        reserved: Option<&crate::deltaglider::spool::SpoolReservation>,
    ) -> Result<StoreResult, EngineError> {
        let PassthroughWrite {
            bucket,
            key,
            deltaspace_id,
            prior,
            metadata,
            source,
        } = write;
        let filename = metadata.original_name.clone();
        match source {
            PassthroughSource::Bytes(data) => {
                self.storage
                    .put_passthrough(bucket, deltaspace_id, &filename, data, &metadata)
                    .await?
            }
            PassthroughSource::File { path, held } => {
                self.storage
                    .put_passthrough_file(
                        bucket,
                        deltaspace_id,
                        &filename,
                        path,
                        &metadata,
                        SpoolBudget::new(&self.spool, held, reserved),
                    )
                    .await?
            }
        }
        // Committed: count it before the next await.
        self.record_commit(bucket, prior, &metadata);
        if let Err(e) = self
            .delete_delta_idempotent(bucket, deltaspace_id, &filename)
            .await
        {
            warn!("Failed to clean up old delta after passthrough write: {e}");
        }
        let size = metadata.file_size;
        self.metadata_cache.insert(bucket, key, metadata.clone());
        Ok(StoreResult::new(metadata, size))
    }

    /// True iff a read reference's DG metadata is missing/corrupt (its S3
    /// `x-amz-meta-dg-*` headers / xattr were stripped — the classic "copied
    /// without --metadata" damage). Both conditions matter: a fully-stripped
    /// reference reads back as a `Passthrough` fallback (fails `is_reference`),
    /// and a partially-stripped one that kept `dg-note=reference` but lost its
    /// SHA reads as a Reference with an empty `file_sha256`. A HEALTHY reference
    /// always carries a non-empty `file_sha256`. NB: a transient backend error
    /// surfaces as `Err` from `get_reference_metadata`, never as this shape —
    /// so this can never mistake a 503/timeout for corruption.
    fn reference_metadata_is_corrupt(m: &FileMetadata) -> bool {
        !m.is_reference() || m.file_sha256.is_empty()
    }

    /// Heal a reference whose DG metadata was stripped, RE-STAMPING it (same
    /// bytes, correct headers) so future reads resolve it and content-diff
    /// replication stops re-copying the whole deltaspace every run. Returns the
    /// (possibly re-stamped) reference metadata to encode against.
    ///
    /// Bytes are UNCHANGED — every existing sibling delta was encoded against
    /// these bytes and must still reconstruct (the decode uses the reference
    /// bytes, not its `ref_sha256`). Caller MUST hold the per-deltaspace prefix
    /// lock. `original_name` is pinned to `INTERNAL_REFERENCE_NAME` — the
    /// legacy-reference migrator keys off that to skip already-internal
    /// references; any other value would trigger a spurious migration.
    async fn heal_reference_if_corrupt(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        ref_meta: FileMetadata,
        // A spool the caller already holds (the streaming PUT body).
        held: Option<&crate::deltaglider::spool::Spool>,
        xnode: &super::ReferenceLockGuard,
    ) -> Result<FileMetadata, EngineError> {
        if !Self::reference_metadata_is_corrupt(&ref_meta) {
            return Ok(ref_meta); // healthy — zero extra I/O
        }
        warn!(
            "Reference {}/{} has stripped DG metadata — re-stamping (bytes unchanged) \
             so replication stops re-copying this deltaspace",
            bucket, deltaspace_id
        );
        // Materialise the intact reference bytes to a spool, re-hash, re-put
        // with correct metadata. Reserve the spool at the reference's on-disk
        // size (fall back to the fallback-reported size, which is the object's
        // content length — accurate for the reference object).
        let spool = self
            .spool_acquire_beside(held, ref_meta.file_size.max(1))
            .await?;
        self.storage
            .get_reference_to_file(bucket, deltaspace_id, spool.path())
            .await?;
        let (sha256, md5, size) = Self::hash_spool_file(spool.path()).await?;
        let healed = FileMetadata::new_reference(
            Self::INTERNAL_REFERENCE_NAME.to_string(),
            // source_name is cosmetic (display/label only; not used by decode,
            // verify, or replication compare). A stable placeholder that equals
            // the read-side default keeps re-reads idempotent.
            Self::INTERNAL_REFERENCE_NAME.to_string(),
            sha256,
            md5,
            size,
            ref_meta.content_type.clone(),
        );
        // counter: unchanged (the same bytes under repaired metadata).
        xnode
            .put_reference_from_file(&*self.storage, bucket, deltaspace_id, spool.path(), &healed)
            .await?;
        self.cache
            .invalidate(&self.cache_key(bucket, deltaspace_id));
        Ok(healed)
    }

    /// Stream-hash a spool file → (sha256_hex, md5_hex, byte_len). Bounded
    /// memory (256KiB chunks). Shared by the store hash path and the reference
    /// heal.
    pub(super) async fn hash_spool_file(path: &Path) -> Result<(String, String, u64), EngineError> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || -> std::io::Result<(String, String, u64)> {
            use std::io::Read;
            let mut f = std::fs::File::open(&path)?;
            let mut sh = Sha256::new();
            let mut mh = Md5::new();
            let mut observed: u64 = 0;
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                observed = observed.saturating_add(n as u64);
                sh.update(&buf[..n]);
                mh.update(&buf[..n]);
            }
            Ok((
                hex::encode(sh.finalize()),
                hex::encode(mh.finalize()),
                observed,
            ))
        })
        .await
        .map_err(|e| EngineError::Storage(StorageError::Other(format!("hash task: {e}"))))?
        .map_err(|e| EngineError::Storage(StorageError::from(e)))
    }

    /// Check if a key's filename is eligible for delta compression.
    pub fn is_delta_eligible(&self, key: &str) -> bool {
        self.file_router.is_delta_eligible(key)
    }

    /// Store a non-delta-eligible object from pre-split chunks without assembling
    /// into a contiguous buffer. Computes SHA256 and MD5 incrementally.
    #[instrument(skip(self, chunks, user_metadata))]
    pub async fn store_passthrough_chunked(
        &self,
        bucket: &str,
        key: &str,
        chunks: &[Bytes],
        total_size: u64,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
    ) -> Result<StoreResult, EngineError> {
        let prior = self.prior_for_counter(bucket, key).await;
        let result = self
            .store_passthrough_chunked_inner(
                bucket,
                key,
                chunks,
                total_size,
                content_type,
                user_metadata,
                None,
                &prior,
            )
            .await?;
        Ok(result.with_accounting(prior.replaced(), 0))
    }

    /// Multipart-aware variant of [`Self::store_passthrough_chunked`]. The
    /// `multipart_etag` is persisted on metadata so HEAD/GET/LIST return
    /// it verbatim (H1 correctness fix).
    #[instrument(skip(self, chunks, user_metadata, multipart_etag))]
    #[allow(clippy::too_many_arguments)]
    pub async fn store_passthrough_chunked_with_multipart_etag(
        &self,
        bucket: &str,
        key: &str,
        chunks: &[Bytes],
        total_size: u64,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
        multipart_etag: String,
    ) -> Result<StoreResult, EngineError> {
        let prior = self.prior_for_counter(bucket, key).await;
        let result = self
            .store_passthrough_chunked_inner(
                bucket,
                key,
                chunks,
                total_size,
                content_type,
                user_metadata,
                Some(multipart_etag),
                &prior,
            )
            .await?;
        Ok(result.with_accounting(prior.replaced(), 0))
    }

    /// Guard a PASSTHROUGH store against the passthrough ceiling (64 GiB
    /// default), NOT the much smaller delta `max_object_size`. Every
    /// passthrough sink shares this — a new one that forgets the check would
    /// let an object past the wrong limit. (The delta path uses its own
    /// strategy-aware ceiling in `store_spooled_delta`; don't route it here.)
    fn ensure_within_passthrough_ceiling(&self, total_size: u64) -> Result<(), EngineError> {
        if total_size > self.max_passthrough_object_size {
            return Err(EngineError::TooLarge {
                size: total_size,
                max: self.max_passthrough_object_size,
            });
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn store_passthrough_chunked_inner(
        &self,
        bucket: &str,
        key: &str,
        chunks: &[Bytes],
        total_size: u64,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
        multipart_etag: Option<String>,
        prior: &CounterPrior,
    ) -> Result<StoreResult, EngineError> {
        self.ensure_within_passthrough_ceiling(total_size)?;

        // Invalidate stale metadata on overwrite
        self.metadata_cache.invalidate(bucket, key);

        let (obj_key, deltaspace_id) = self.validated_key_ingest(bucket, key)?;

        // Compute SHA256 + MD5 incrementally across chunks
        let mut sha256_hasher = Sha256::new();
        let mut md5_hasher = Md5::new();
        for chunk in chunks {
            sha256_hasher.update(chunk);
            md5_hasher.update(chunk);
        }
        let sha256 = hex::encode(sha256_hasher.finalize());
        let md5 = hex::encode(md5_hasher.finalize());

        info!(
            "Storing chunked {}/{} ({} bytes, {} chunks, sha256={})",
            bucket,
            key,
            total_size,
            chunks.len(),
            &sha256[..8]
        );

        let _guard = self.acquire_prefix_lock(bucket, &deltaspace_id).await;

        let mut metadata = FileMetadata::new_passthrough(
            obj_key.filename.clone(),
            sha256,
            md5,
            total_size,
            content_type,
        );
        metadata.set_user_metadata(user_metadata);
        metadata.multipart_etag = multipart_etag;

        self.storage
            .put_passthrough_chunked(bucket, &deltaspace_id, &obj_key.filename, chunks, &metadata)
            .await?;
        // Committed: count it before the next await.
        self.record_commit(bucket, prior, &metadata);
        // Write succeeded — now safe to clean up old delta variant
        if let Err(e) = self
            .delete_delta_idempotent(bucket, &deltaspace_id, &obj_key.filename)
            .await
        {
            warn!(
                "Failed to clean up old delta after chunked passthrough write: {}",
                e
            );
        }

        let result = StoreResult::new(metadata, total_size);
        self.metadata_cache
            .insert(bucket, key, result.metadata.clone());
        Ok(result)
    }

    /// Store a passthrough object from relayed multipart part files without
    /// materializing an assembled temporary file.
    #[instrument(skip(self, part_paths, user_metadata, multipart_etag))]
    #[allow(clippy::too_many_arguments)]
    pub async fn store_passthrough_relayed_parts_with_multipart_etag(
        &self,
        bucket: &str,
        key: &str,
        part_paths: &[PathBuf],
        total_size: u64,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
        multipart_etag: String,
    ) -> Result<StoreResult, EngineError> {
        let prior = self.prior_for_counter(bucket, key).await;
        let result = self
            .store_passthrough_relayed_parts_inner(
                bucket,
                key,
                part_paths,
                total_size,
                content_type,
                user_metadata,
                multipart_etag,
                &prior,
            )
            .await?;
        Ok(result.with_accounting(prior.replaced(), 0))
    }

    /// Body of [`Self::store_passthrough_relayed_parts_with_multipart_etag`]; its commit records the counter.
    #[allow(clippy::too_many_arguments)]
    async fn store_passthrough_relayed_parts_inner(
        &self,
        bucket: &str,
        key: &str,
        part_paths: &[PathBuf],
        total_size: u64,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
        multipart_etag: String,
        prior: &CounterPrior,
    ) -> Result<StoreResult, EngineError> {
        self.ensure_within_passthrough_ceiling(total_size)?;

        self.metadata_cache.invalidate(bucket, key);
        let (obj_key, deltaspace_id) = self.validated_key_ingest(bucket, key)?;

        let mut sha256_hasher = Sha256::new();
        let mut md5_hasher = Md5::new();
        let mut observed = 0u64;
        let mut buf = vec![0u8; 1024 * 1024];
        for path in part_paths {
            let mut file = tokio::fs::File::open(path)
                .await
                .map_err(StorageError::from)?;
            loop {
                let n = file.read(&mut buf).await.map_err(StorageError::from)?;
                if n == 0 {
                    break;
                }
                observed = observed.saturating_add(n as u64);
                sha256_hasher.update(&buf[..n]);
                md5_hasher.update(&buf[..n]);
            }
        }
        if observed != total_size {
            return Err(EngineError::Storage(StorageError::Other(format!(
                "Multipart relay size mismatch: expected {}, observed {}",
                total_size, observed
            ))));
        }
        let sha256 = hex::encode(sha256_hasher.finalize());
        let md5 = hex::encode(md5_hasher.finalize());
        // The relay parts hold spool budget (the multipart store reserves it
        // per part), so this op is a holder and never waits.
        let parts_mib = crate::deltaglider::spool::mib_ceil(total_size);
        let reserved = self
            .reserve_storage_spool(bucket, total_size, true, parts_mib)
            .await?;
        let _guard = self.acquire_prefix_lock(bucket, &deltaspace_id).await;

        let mut metadata = FileMetadata::new_passthrough(
            obj_key.filename.clone(),
            sha256,
            md5,
            total_size,
            content_type,
        );
        metadata.set_user_metadata(user_metadata);
        metadata.multipart_etag = Some(multipart_etag);

        self.storage
            .put_passthrough_parts(
                bucket,
                &deltaspace_id,
                &obj_key.filename,
                part_paths,
                &metadata,
                SpoolBudget::new(&self.spool, None, reserved.as_ref()),
            )
            .await?;
        // Committed: count it before the next await.
        self.record_commit(bucket, prior, &metadata);
        if let Err(e) = self
            .delete_delta_idempotent(bucket, &deltaspace_id, &obj_key.filename)
            .await
        {
            warn!(
                "Failed to clean up old delta after relayed passthrough write: {}",
                e
            );
        }

        let result = StoreResult::new(metadata, total_size);
        self.metadata_cache
            .insert(bucket, key, result.metadata.clone());
        Ok(result)
    }

    /// Store a passthrough object from a local file path, computing hashes
    /// incrementally to avoid reconstructing large multipart payloads in memory.
    #[instrument(skip(self, user_metadata, multipart_etag))]
    #[allow(clippy::too_many_arguments)]
    pub async fn store_passthrough_file_with_multipart_etag(
        &self,
        bucket: &str,
        key: &str,
        source_path: &Path,
        total_size: u64,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
        multipart_etag: String,
    ) -> Result<StoreResult, EngineError> {
        let prior = self.prior_for_counter(bucket, key).await;
        let result = self
            .store_passthrough_file_inner(
                bucket,
                key,
                source_path,
                None,
                total_size,
                content_type,
                user_metadata,
                Some(multipart_etag),
                &prior,
            )
            .await?;
        Ok(result.with_accounting(prior.replaced(), 0))
    }

    /// Body of [`Self::store_passthrough_file_with_multipart_etag`]; its commit records the counter.
    #[allow(clippy::too_many_arguments)]
    async fn store_passthrough_file_inner(
        &self,
        bucket: &str,
        key: &str,
        source_path: &Path,
        // The body spool this op holds, hashed and size-checked already.
        held: Option<HashedSpool<'_>>,
        total_size: u64,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
        multipart_etag: Option<String>,
        prior: &CounterPrior,
    ) -> Result<StoreResult, EngineError> {
        self.ensure_within_passthrough_ceiling(total_size)?;

        self.metadata_cache.invalidate(bucket, key);
        let (obj_key, deltaspace_id) = self.validated_key_ingest(bucket, key)?;

        let (sha256, md5) = match &held {
            // storage-9: the streaming PUT hashed its body already.
            Some(h) => (h.sha256.to_string(), h.md5.to_string()),
            None => {
                let (sha256, md5, observed) = Self::hash_spool_file(source_path).await?;
                if observed != total_size {
                    return Err(EngineError::Storage(StorageError::Other(format!(
                        "Multipart relay size mismatch: expected {}, observed {}",
                        total_size, observed
                    ))));
                }
                (sha256, md5)
            }
        };
        let held = held.map(|h| h.spool);
        let reserved = self
            .reserve_storage_spool(
                bucket,
                total_size,
                false,
                held.map_or(0, crate::deltaglider::spool::Spool::reserved_mib),
            )
            .await?;
        let _guard = self.acquire_prefix_lock(bucket, &deltaspace_id).await;
        let write = PassthroughWrite {
            bucket,
            key,
            deltaspace_id: &deltaspace_id,
            prior,
            metadata: passthrough_metadata(
                &obj_key.filename,
                sha256,
                md5,
                total_size,
                content_type,
                user_metadata,
                multipart_etag,
            ),
            source: PassthroughSource::File {
                path: source_path,
                held,
            },
        };
        self.write_passthrough_locked(write, reserved.as_ref())
            .await
    }

    /// Begin a streaming passthrough multipart upload (Phase B). Gated on
    /// `max_passthrough_object_size`. Acquires the per-deltaspace lock and
    /// holds it for the lifetime of the returned handle, mirroring
    /// `store_passthrough_chunked_inner`. The caller drives parts via
    /// [`Self::upload_passthrough_part`] then finalizes with
    /// [`Self::finish_passthrough_multipart`] (or aborts).
    pub async fn begin_passthrough_multipart(
        &self,
        bucket: &str,
        key: &str,
        facts: MultipartObjectFacts,
    ) -> Result<PassthroughMultipartHandle, EngineError> {
        let total_size = facts.total_size;
        self.ensure_within_passthrough_ceiling(total_size)?;

        self.metadata_cache.invalidate(bucket, key);
        let (obj_key, deltaspace_id) = self.validated_key_ingest(bucket, key)?;
        let guard = self.acquire_prefix_lock(bucket, &deltaspace_id).await;

        // The FINAL metadata, hashes included: a native S3 backend stores
        // object metadata only at create time (its complete takes none), so
        // empty hashes here made every reader fall back to a foreign-object
        // HEAD (no Content-Type, no user metadata, the `<md5>-N` ETag).
        let mut metadata = FileMetadata::new_passthrough(
            obj_key.filename.clone(),
            facts.sha256,
            facts.md5,
            total_size,
            facts.content_type,
        );
        metadata.set_user_metadata(facts.user_metadata);
        metadata.multipart_etag = facts.multipart_etag;

        let upload = self
            .storage
            .create_multipart_upload(bucket, &deltaspace_id, &obj_key.filename, &metadata)
            .await?;

        Ok(PassthroughMultipartHandle {
            bucket: bucket.to_string(),
            key: key.to_string(),
            deltaspace_id,
            filename: obj_key.filename,
            total_size,
            metadata,
            upload,
            _guard: guard,
        })
    }

    /// Upload one part of an in-progress passthrough multipart upload.
    /// IMMUTABLE in the handle so the caller can drive parts CONCURRENTLY.
    /// Returns the [`UploadedPart`] receipt (the caller collects + sorts
    /// them). Memory stays O(in-flight parts) — the bytes are owned by the
    /// caller's bounded `buffer_unordered` and dropped after this returns.
    pub async fn upload_passthrough_part(
        &self,
        handle: &PassthroughMultipartHandle,
        part_number: i32,
        data: Bytes,
    ) -> Result<UploadedPart, EngineError> {
        let part = self
            .storage
            .upload_part(
                &handle.upload,
                &handle.deltaspace_id,
                &handle.filename,
                part_number,
                data,
            )
            .await?;
        Ok(part)
    }

    /// Finalize a passthrough multipart upload: complete on the backend
    /// (a buffering backend writes the handle's metadata now), and clean the
    /// old delta variant. Consumes the handle (releases the lock). `parts`
    /// and `assembled` must be in part-number order; `assembled` is empty
    /// for native backends.
    pub async fn finish_passthrough_multipart(
        &self,
        handle: PassthroughMultipartHandle,
        mut parts: Vec<UploadedPart>,
        assembled: Vec<Bytes>,
    ) -> Result<StoreResult, EngineError> {
        // `parts` must be part-number-ordered for the multipart complete;
        // `assembled` (buffering backends only) is already caller-ordered.
        parts.sort_by_key(|p| p.part_number);

        // Overwrite-net accounting for the usage counter (see store_inner). The
        // handle already holds the per-deltaspace lock, so this is race-safe.
        let prior = self.prior_for_counter(&handle.bucket, &handle.key).await;

        let metadata = handle.metadata.clone();

        if let Err(e) = self
            .storage
            .complete_multipart_upload(
                &handle.upload,
                &handle.deltaspace_id,
                &handle.filename,
                &parts,
                &assembled,
                &metadata,
            )
            .await
        {
            // Complete failed: abort so the fully-uploaded part set doesn't
            // dangle on backends (B2) that never GC incomplete uploads.
            self.abort_passthrough_multipart_ref(&handle).await;
            return Err(e.into());
        }
        // Committed: count it before the next await.
        self.record_commit(&handle.bucket, &prior, &metadata);

        if let Err(e) = self
            .delete_delta_idempotent(&handle.bucket, &handle.deltaspace_id, &handle.filename)
            .await
        {
            warn!(
                "Failed to clean up old delta after multipart passthrough write: {}",
                e
            );
        }

        // Streaming multipart (large replication/lifecycle copies). Every
        // store entry point records exactly once, at its commit;
        // `counter_tests` pins the accounting against the buffered `store()`
        // oracle.
        let result =
            StoreResult::new(metadata, handle.total_size).with_accounting(prior.replaced(), 0);
        self.metadata_cache
            .insert(&handle.bucket, &handle.key, result.metadata.clone());
        Ok(result)
    }

    /// Abort an in-progress passthrough multipart upload (best-effort)
    /// WITHOUT consuming the handle — callers holding it behind an Arc can
    /// abort at any strong count (the prefix lock releases when the last
    /// clone drops).
    pub async fn abort_passthrough_multipart_ref(&self, handle: &PassthroughMultipartHandle) {
        if let Err(e) = self
            .storage
            .abort_multipart_upload(&handle.upload, &handle.deltaspace_id, &handle.filename)
            .await
        {
            warn!(
                "Failed to abort multipart upload {}/{}: {}",
                handle.bucket, handle.key, e
            );
        }
    }

    /// Delete a storage object, ignoring NotFound errors (idempotent delete).
    /// Swallow NotFound errors from a storage delete — the object is already gone.
    fn delete_ignoring_not_found(result: Result<(), StorageError>) -> Result<(), EngineError> {
        match result {
            Ok(()) | Err(StorageError::NotFound(_)) => Ok(()),
            Err(other) => Err(other.into()),
        }
    }

    /// Delete a delta file, ignoring NotFound (idempotent).
    async fn delete_delta_idempotent(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        filename: &str,
    ) -> Result<(), EngineError> {
        Self::delete_ignoring_not_found(
            self.storage
                .delete_delta(bucket, deltaspace_id, filename)
                .await,
        )
    }

    /// Delete a passthrough file, ignoring NotFound (idempotent).
    pub(crate) async fn delete_passthrough_idempotent(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        filename: &str,
    ) -> Result<(), EngineError> {
        Self::delete_ignoring_not_found(
            self.storage
                .delete_passthrough(bucket, deltaspace_id, filename)
                .await,
        )
    }

    pub(super) async fn migrate_legacy_reference_object_if_needed(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        filename: &str,
        xnode: &super::ReferenceLockGuard,
    ) -> Result<bool, EngineError> {
        if !self.storage.has_reference(bucket, deltaspace_id).await? {
            return Ok(false);
        }

        let mut ref_meta = self
            .storage
            .get_reference_metadata(bucket, deltaspace_id)
            .await?;
        if ref_meta.original_name == Self::INTERNAL_REFERENCE_NAME {
            return Ok(false);
        }
        if ref_meta.original_name != filename {
            return Ok(false);
        }

        // A client stored `filename` after the legacy reference: that
        // variant is the object now. Only restamp the reference; writing
        // its bytes as `filename` would put the legacy version back over it.
        if self
            .resolve_object_metadata(bucket, deltaspace_id, filename)
            .await?
            .is_some()
        {
            ref_meta.original_name = Self::INTERNAL_REFERENCE_NAME.to_string();
            xnode
                .put_reference_metadata(&*self.storage, bucket, deltaspace_id, &ref_meta)
                .await?;
            self.cache
                .invalidate(&self.cache_key(bucket, deltaspace_id));
            return Ok(true);
        }

        let (reference, _cache_hit) = self
            .get_reference_cached(bucket, deltaspace_id, &ref_meta.file_sha256)
            .await?;
        let source_file = self.codec_source_spool_now(reference.len())?;
        let _codec_permit = self.codec_semaphore.acquire().await.map_err(|_| {
            EngineError::Storage(StorageError::Other("codec semaphore closed".into()))
        })?;
        let delta = self
            .codec
            .encode_spooled(&source_file, &reference, &reference)?;
        drop(_codec_permit);

        let delta_meta = FileMetadata::new_delta(
            filename.to_string(),
            ref_meta.file_sha256.clone(),
            ref_meta.md5.clone(),
            ref_meta.file_size,
            "reference.bin".to_string(),
            ref_meta.file_sha256.clone(),
            delta.len() as u64,
            ref_meta.content_type.clone(),
        );

        // Write the delta BEFORE deleting the passthrough. If put_delta fails,
        // the passthrough still exists and the object remains accessible.
        xnode
            .put_delta(
                &*self.storage,
                bucket,
                deltaspace_id,
                filename,
                &delta,
                &delta_meta,
            )
            .await?;
        // A new user-visible object (the reference stays, already counted or
        // out of band): no variant of `filename` existed (checked above).
        self.record_commit(bucket, &CounterPrior::Absent, &delta_meta);
        self.delete_passthrough_idempotent(bucket, deltaspace_id, filename)
            .await?;

        ref_meta.original_name = Self::INTERNAL_REFERENCE_NAME.to_string();
        xnode
            .put_reference_metadata(&*self.storage, bucket, deltaspace_id, &ref_meta)
            .await?;

        // Invalidate cache — reference metadata changed (though data is unchanged,
        // the cached Bytes doesn't include metadata, so this is precautionary).
        let cache_key = self.cache_key(bucket, deltaspace_id);
        self.cache.invalidate(&cache_key);

        Ok(true)
    }

    /// Batch-migrate all legacy reference objects in a bucket. Visits only
    /// the deltaspaces that hold a reference.
    /// Returns (migrated_count, skipped_count, error_count).
    pub async fn migrate_legacy_references(
        &self,
        bucket: &str,
    ) -> Result<(u32, u32, u32), EngineError> {
        let deltaspaces = self.storage.list_reference_prefixes(bucket, "").await?;
        let mut migrated = 0u32;
        let mut skipped = 0u32;
        let mut errors = 0u32;

        for ds in &deltaspaces {
            // Check if reference exists and needs migration
            if !self.storage.has_reference(bucket, ds).await? {
                skipped += 1;
                continue;
            }

            let ref_meta = match self.storage.get_reference_metadata(bucket, ds).await {
                Ok(m) => m,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };

            if ref_meta.original_name == Self::INTERNAL_REFERENCE_NAME {
                skipped += 1;
                continue;
            }

            // This is a legacy reference — migrate it
            let filename = ref_meta.original_name.clone();
            let _guard = self.acquire_prefix_lock(bucket, ds).await;
            let xnode = match self.acquire_reference_lock(bucket, ds).await {
                Ok(g) => g,
                Err(e) => {
                    tracing::warn!("Failed to migrate {}/{}: {}", bucket, ds, e);
                    errors += 1;
                    continue;
                }
            };
            match self
                .migrate_legacy_reference_object_if_needed(bucket, ds, &filename, &xnode)
                .await
            {
                Ok(true) => {
                    tracing::info!("Migrated legacy reference in {}/{}", bucket, ds);
                    migrated += 1;
                }
                Ok(false) => {
                    skipped += 1;
                }
                Err(e) => {
                    tracing::warn!("Failed to migrate {}/{}: {}", bucket, ds, e);
                    errors += 1;
                }
            }
        }

        Ok((migrated, skipped, errors))
    }
}

/// Usage-counter accounting across every public store entry point. The
/// buffered `store()` path is the oracle: any other path that stores the same
/// bytes under the same keys must leave the counter in the same state.
#[cfg(test)]
mod counter_tests {
    use super::*;
    use crate::bucket_usage::{BucketUsage, BucketUsageRow};
    use crate::config::Config;
    use crate::storage::FilesystemBackend;

    pub(super) const BUCKET: &str = "counter-bkt";

    pub(super) struct Harness {
        _tmp: tempfile::TempDir,
        usage: Arc<BucketUsage>,
        pub(super) engine: DeltaGliderEngine<FilesystemBackend>,
    }

    pub(super) async fn harness() -> Harness {
        let tmp = tempfile::tempdir().unwrap();
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .unwrap();
        backend.create_bucket(BUCKET).await.unwrap();
        let usage = Arc::new(BucketUsage::in_memory().unwrap());
        let engine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None)
                .with_bucket_usage(Some(usage.clone()));
        Harness {
            _tmp: tmp,
            usage,
            engine,
        }
    }

    fn row(h: &Harness) -> (u64, u64, u64) {
        h.usage.flush_pending();
        let r: BucketUsageRow = h.usage.read(BUCKET).unwrap().unwrap_or(BucketUsageRow {
            object_count: 0,
            logical_bytes: 0,
            stored_bytes: 0,
            last_scan_at: None,
        });
        (r.object_count, r.logical_bytes, r.stored_bytes)
    }

    /// Two versions of a delta-eligible artifact: v2 is v1 with a small edit.
    pub(super) fn versions() -> (Vec<u8>, Vec<u8>) {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let v1: Vec<u8> = (0..200_000)
            .map(|_| {
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
            })
            .collect();
        let mut v2 = v1.clone();
        v2[100_000..100_100].fill(0xAB);
        (v1, v2)
    }

    async fn buffered(h: &Harness, key: &str, data: &[u8]) {
        h.engine
            .store(BUCKET, key, data, None, HashMap::new())
            .await
            .unwrap();
    }

    async fn spooled(h: &Harness, key: &str, data: &[u8]) {
        let spool = h.engine.spool_acquire(data.len() as u64).await.unwrap();
        tokio::fs::write(spool.path(), data).await.unwrap();
        h.engine
            .store_spooled_delta(
                BUCKET,
                key,
                &spool,
                data.len() as u64,
                None,
                HashMap::new(),
                None,
            )
            .await
            .unwrap();
    }

    async fn relayed(h: &Harness, key: &str, data: &[u8]) {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = data.split_at(data.len() / 2);
        let paths = vec![dir.path().join("p1"), dir.path().join("p2")];
        tokio::fs::write(&paths[0], a).await.unwrap();
        tokio::fs::write(&paths[1], b).await.unwrap();
        h.engine
            .store_passthrough_relayed_parts_with_multipart_etag(
                BUCKET,
                key,
                &paths,
                data.len() as u64,
                None,
                HashMap::new(),
                "\"0123456789abcdef0123456789abcdef-2\"".to_string(),
            )
            .await
            .unwrap();
    }

    async fn file(h: &Harness, key: &str, data: &[u8]) {
        let f = tempfile::NamedTempFile::new().unwrap();
        tokio::fs::write(f.path(), data).await.unwrap();
        h.engine
            .store_passthrough_file_with_multipart_etag(
                BUCKET,
                key,
                f.path(),
                data.len() as u64,
                None,
                HashMap::new(),
                "\"0123456789abcdef0123456789abcdef-1\"".to_string(),
            )
            .await
            .unwrap();
    }

    async fn chunked(h: &Harness, key: &str, data: &[u8]) {
        let chunks: Vec<Bytes> = data.chunks(64 * 1024).map(Bytes::copy_from_slice).collect();
        h.engine
            .store_passthrough_chunked_with_multipart_etag(
                BUCKET,
                key,
                &chunks,
                data.len() as u64,
                None,
                HashMap::new(),
                "\"0123456789abcdef0123456789abcdef-3\"".to_string(),
            )
            .await
            .unwrap();
    }

    /// Streaming delta PUTs: a fresh baseline, a sibling delta, then an
    /// overwrite. Pre-fix the delta-win branch never recorded (count 0).
    #[tokio::test]
    async fn spooled_delta_matches_buffered() {
        let (v1, v2) = versions();
        let (oracle, h) = (harness().await, harness().await);
        for (key, data) in [("rel/a.zip", &v1), ("rel/b.zip", &v2), ("rel/b.zip", &v1)] {
            buffered(&oracle, key, data).await;
            spooled(&h, key, data).await;
        }
        assert_eq!(row(&oracle).0, 2, "oracle: two keys");
        assert_eq!(row(&h), row(&oracle));
    }

    /// Streaming PUT of a non-delta-eligible key, then an overwrite. Pre-fix
    /// the overwrite counted a second object.
    #[tokio::test]
    async fn spooled_passthrough_overwrite_matches_buffered() {
        let (v1, v2) = versions();
        let (oracle, h) = (harness().await, harness().await);
        for data in [&v1, &v2] {
            buffered(&oracle, "img/a.jpg", data).await;
            spooled(&h, "img/a.jpg", data).await;
        }
        assert_eq!(row(&oracle).0, 1);
        assert_eq!(row(&h), row(&oracle));
    }

    /// Every multipart passthrough sink must net an overwrite to +0 objects.
    #[tokio::test]
    async fn multipart_sinks_overwrite_counts_one_object() {
        let (v1, v2) = versions();
        for sink in ["relayed", "file", "chunked"] {
            let h = harness().await;
            for data in [&v1, &v2] {
                match sink {
                    "relayed" => relayed(&h, "img/a.jpg", data).await,
                    "file" => file(&h, "img/a.jpg", data).await,
                    _ => chunked(&h, "img/a.jpg", data).await,
                }
            }
            let (count, logical, _) = row(&h);
            assert_eq!(count, 1, "{sink}: overwrite must not add an object");
            assert_eq!(logical, v2.len() as u64, "{sink}: logical bytes of v2 only");
        }
    }
}

/// N1: `dg-no-delta: true` user metadata (the `--no-delta` flag of the CLI,
/// or `x-amz-meta-dg-no-delta` from an S3 client) stores a delta-eligible
/// object passthrough, and the hint itself is not stored.
#[cfg(test)]
mod no_delta_hint_tests {
    use super::counter_tests::{harness, versions, BUCKET};
    use super::*;

    fn with_hint() -> HashMap<String, String> {
        HashMap::from([
            ("dg-no-delta".to_string(), "true".to_string()),
            ("owner".to_string(), "ci".to_string()),
        ])
    }

    #[tokio::test]
    async fn the_hint_stores_passthrough_and_is_not_persisted() {
        let (v1, v2) = versions();
        let h = harness().await;
        // Control: without the hint, v2 is a delta against v1.
        h.engine
            .store(BUCKET, "ctl/v1.zip", &v1, None, HashMap::new())
            .await
            .unwrap();
        let ctl = h
            .engine
            .store(BUCKET, "ctl/v2.zip", &v2, None, HashMap::new())
            .await
            .unwrap();
        assert!(ctl.metadata.is_delta(), "control must delta-encode");

        for (key, data) in [("rel/v1.zip", &v1), ("rel/v2.zip", &v2)] {
            let r = h
                .engine
                .store(BUCKET, key, data, None, with_hint())
                .await
                .unwrap();
            assert!(
                matches!(r.metadata.storage_info, StorageInfo::Passthrough),
                "{key}: buffered PUT with the hint must be passthrough"
            );
        }
        let spool = h.engine.spool_acquire(v2.len() as u64).await.unwrap();
        tokio::fs::write(spool.path(), &v2).await.unwrap();
        let r = h
            .engine
            .store_spooled_delta(
                BUCKET,
                "rel/v3.zip",
                &spool,
                v2.len() as u64,
                None,
                with_hint(),
                None,
            )
            .await
            .unwrap();
        assert!(
            matches!(r.metadata.storage_info, StorageInfo::Passthrough),
            "spooled PUT with the hint must be passthrough"
        );
        assert!(
            !h.engine.storage.has_reference(BUCKET, "rel").await.unwrap(),
            "the hint must not seed a baseline"
        );
        for key in ["rel/v1.zip", "rel/v2.zip", "rel/v3.zip"] {
            h.engine.metadata_cache.invalidate(BUCKET, key);
            let meta = h.engine.head(BUCKET, key).await.unwrap();
            assert_eq!(
                meta.user_metadata,
                HashMap::from([("owner".to_string(), "ci".to_string())]),
                "{key}: the hint is an instruction, not stored metadata"
            );
        }
    }
}

/// D8: the buffered encode must not trust a cached reference that no longer
/// matches the stored one (a peer node reseeded the deltaspace).
#[cfg(test)]
mod stale_reference_cache_tests {
    use super::*;
    use crate::config::Config;
    use crate::storage::FilesystemBackend;

    fn noise(seed: u64, n: usize) -> Vec<u8> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
            })
            .collect()
    }

    #[tokio::test]
    async fn buffered_store_reloads_a_reseeded_reference() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = Arc::new(
            FilesystemBackend::new(tmp.path().to_path_buf())
                .await
                .unwrap(),
        );
        backend.create_bucket("b").await.unwrap();
        let engine = DeltaGliderEngine::new_with_backend(backend.clone(), &Config::default(), None);

        // v1 seeds the reference and the cache.
        let v1 = noise(1, 200_000);
        engine
            .store("b", "rel/a.zip", &v1, None, HashMap::new())
            .await
            .unwrap();

        // A peer node reseeds reference.bin with different bytes + metadata.
        let r2 = noise(2, 200_000);
        let r2_meta = FileMetadata::new_reference(
            DeltaGliderEngine::<FilesystemBackend>::INTERNAL_REFERENCE_NAME.to_string(),
            "rel/other.zip".into(),
            hex::encode(Sha256::digest(&r2)),
            hex::encode(Md5::digest(&r2)),
            r2.len() as u64,
            None,
        );
        backend
            .put_reference(
                "b",
                "rel",
                &r2,
                &r2_meta,
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .unwrap();

        // v2 is close to v1: against the STALE cached v1 it deltas well and
        // gets committed as a delta, stamped with r2's sha.
        let mut v2 = v1.clone();
        v2[1000..1100].fill(0xAB);
        engine
            .store("b", "rel/b.zip", &v2, None, HashMap::new())
            .await
            .unwrap();
        // Another node (or this one after a restart) has no cached copy and
        // decodes against the stored reference.
        let fresh = DeltaGliderEngine::new_with_backend(backend, &Config::default(), None);
        let (back, _) = fresh.retrieve("b", "rel/b.zip").await.expect("readable");
        assert_eq!(back, v2);
    }
}

/// Tier 4: a streaming PUT holds its body spool while it needs the ref +
/// delta pair. Under a small budget it waited for its own budget until the
/// acquire timeout (120 s) and then failed.
#[cfg(test)]
mod spool_budget_tests {
    use super::*;
    use crate::config::Config;
    use crate::storage::FilesystemBackend;

    #[tokio::test]
    async fn streaming_put_does_not_wait_on_its_own_body_spool() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = FilesystemBackend::new(tmp.path().join("data"))
            .await
            .unwrap();
        backend.create_bucket("b").await.unwrap();
        let mut engine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None);
        // 2 MiB budget; each object is 1.5 MiB, so body + pair > budget.
        engine.spool = Arc::new(
            crate::deltaglider::spool::SpoolDir::new(tmp.path().join("spool"), 2 * 1024 * 1024)
                .unwrap(),
        );
        let v1: Vec<u8> = (0..1_500_000u32).map(|n| (n % 251) as u8).collect();
        let mut v2 = v1.clone();
        v2[700_000..700_100].fill(0xAB);
        for (key, data) in [("rel/a.zip", &v1), ("rel/b.zip", &v2)] {
            let body = engine.spool_acquire(data.len() as u64).await.unwrap();
            tokio::fs::write(body.path(), data).await.unwrap();
            let put = engine.store_spooled_delta(
                "b",
                key,
                &body,
                data.len() as u64,
                None,
                HashMap::new(),
                None,
            );
            tokio::time::timeout(std::time::Duration::from_secs(10), put)
                .await
                .unwrap_or_else(|_| panic!("{key}: streaming PUT stalled on its own spool"))
                .unwrap();
        }
        let (back, _) = engine.retrieve("b", "rel/b.zip").await.unwrap();
        assert_eq!(back, v2);
    }
}

/// Tier 4: a GET served from the metadata cache must not re-insert the entry
/// (each insert restarts moka's TTL, so a hot key never expired).
#[cfg(test)]
mod metadata_cache_ttl_tests {
    use super::*;
    use crate::config::Config;
    use crate::storage::FilesystemBackend;

    #[tokio::test]
    async fn cache_hits_do_not_restart_the_ttl() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .unwrap();
        backend.create_bucket("b").await.unwrap();
        let engine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None);
        for key in ["rel/a.zip", "img/a.jpg"] {
            engine
                .store("b", key, b"payload bytes", None, HashMap::new())
                .await
                .unwrap();
            let after_put = engine.metadata_cache().insert_count();
            for _ in 0..3 {
                engine.retrieve("b", key).await.unwrap();
                let _ = engine.retrieve_stream_range("b", key, 0, 3, None).await;
            }
            assert_eq!(
                engine.metadata_cache().insert_count(),
                after_put,
                "{key}: cache hits re-inserted the entry"
            );
        }
    }
}

/// Tier 3: the per-deltaspace lock is per BUCKET too. Keyed by the prefix
/// alone, `a/releases` and `b/releases` shared one mutex.
#[cfg(test)]
mod prefix_lock_scope_tests {
    use super::*;
    use crate::config::Config;
    use crate::storage::FilesystemBackend;

    #[tokio::test]
    async fn same_prefix_in_two_buckets_does_not_contend() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .unwrap();
        let engine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None);
        let _held = engine.acquire_prefix_lock("bucket-a", "releases").await;
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            engine.acquire_prefix_lock("bucket-b", "releases"),
        )
        .await
        .expect("another bucket's deltaspace must not wait");
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                engine.acquire_prefix_lock("bucket-a", "releases"),
            )
            .await
            .is_err(),
            "the same deltaspace must still serialise"
        );
    }
}

/// A rebuilt engine (config reload) serves while the old one drains its
/// in-flight PUTs. Each built its own prefix-lock map, so the two engines'
/// writers to one deltaspace did not exclude each other.
#[cfg(test)]
mod prefix_lock_rebuild_tests {
    use super::*;
    use crate::config::Config;
    use crate::storage::FilesystemBackend;

    #[tokio::test]
    async fn rebuilt_engines_share_prefix_locks() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = Arc::new(
            FilesystemBackend::new(tmp.path().to_path_buf())
                .await
                .unwrap(),
        );
        let old = DeltaGliderEngine::new_with_backend(backend.clone(), &Config::default(), None);
        let new = DeltaGliderEngine::new_with_backend(backend, &Config::default(), None);
        let bucket = format!("rebuild-{}", uuid::Uuid::new_v4());
        let _held = old.acquire_prefix_lock(&bucket, "releases").await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                new.acquire_prefix_lock(&bucket, "releases"),
            )
            .await
            .is_err(),
            "the new engine must wait for the old engine's writer"
        );
    }
}

/// A rebuilt engine (config reload) must share the spool budget with the
/// engine it replaces: each built its own, so a reload doubled the budget.
#[cfg(test)]
mod spool_singleton_tests {
    use super::*;
    use crate::config::Config;
    use crate::storage::FilesystemBackend;

    #[tokio::test]
    async fn rebuilt_engines_share_one_spool_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = Arc::new(
            FilesystemBackend::new(tmp.path().to_path_buf())
                .await
                .unwrap(),
        );
        let a = DeltaGliderEngine::new_with_backend(backend.clone(), &Config::default(), None);
        let b = DeltaGliderEngine::new_with_backend(backend, &Config::default(), None);
        assert!(a.spool.same_budget(&b.spool));
    }
}

#[cfg(test)]
mod review2_tests {
    use super::*;
    use crate::config::Config;
    use crate::storage::FilesystemBackend;

    /// Review-2 (7692271b): a streaming PUT no longer waits on its OWN body
    /// spool, but two PUTs still wait on EACH OTHER's (hold-and-wait): both
    /// hold a body reservation and ask for a pair that only fits once the
    /// other releases. Both stall until DGP_SPOOL_ACQUIRE_TIMEOUT_SECS, then 503.
    #[tokio::test]
    async fn review2_two_streaming_puts_do_not_deadlock_on_each_others_body_spool() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = FilesystemBackend::new(tmp.path().join("data"))
            .await
            .unwrap();
        backend.create_bucket("b").await.unwrap();
        let mut engine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None);
        engine.spool = Arc::new(
            crate::deltaglider::spool::SpoolDir::new(tmp.path().join("spool"), 4 * 1024 * 1024)
                .unwrap(),
        );
        let v: Vec<u8> = (0..1_500_000u32).map(|n| (n % 251) as u8).collect();
        let body_a = engine.spool_acquire(v.len() as u64).await.unwrap();
        let body_b = engine.spool_acquire(v.len() as u64).await.unwrap();
        tokio::fs::write(body_a.path(), &v).await.unwrap();
        tokio::fs::write(body_b.path(), &v).await.unwrap();
        let a = engine.store_spooled_delta(
            "b",
            "x/a.zip",
            &body_a,
            v.len() as u64,
            None,
            HashMap::new(),
            None,
        );
        let b = engine.store_spooled_delta(
            "b",
            "y/b.zip",
            &body_b,
            v.len() as u64,
            None,
            HashMap::new(),
            None,
        );
        let (ra, rb) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            futures::future::join(a, b),
        )
        .await
        .expect("two streaming PUTs deadlocked on each other's body spool");
        ra.unwrap();
        rb.unwrap();
    }

    /// An engine over an encrypting filesystem backend with a small spool.
    async fn encrypted_engine(
        tmp: &tempfile::TempDir,
        spool_bytes: u64,
    ) -> DeltaGliderEngine<crate::storage::EncryptingBackend<FilesystemBackend>> {
        let backend = FilesystemBackend::new(tmp.path().join("data"))
            .await
            .unwrap();
        backend.create_bucket("b").await.unwrap();
        let cfg = Arc::new(arc_swap::ArcSwap::new(Arc::new(
            crate::storage::EncryptionConfig {
                key: Some(crate::storage::EncryptionKey::from_hex(&"ab".repeat(32)).unwrap()),
                key_id: Some("kid".into()),
                ..Default::default()
            },
        )));
        let wrapper = crate::storage::EncryptingBackend::new(backend, cfg);
        let mut engine =
            DeltaGliderEngine::new_with_backend(Arc::new(wrapper), &Config::default(), None);
        engine.spool = Arc::new(
            crate::deltaglider::spool::SpoolDir::new(tmp.path().join("spool"), spool_bytes)
                .unwrap(),
        );
        engine
    }

    /// Two streaming passthrough PUTs to an encrypting backend: each holds
    /// its body spool, and the encrypt needs as much again. Neither may wait
    /// for the other's body (hold-and-wait): each stores, or fails at once
    /// with a retryable SlowDown.
    #[tokio::test]
    async fn encrypted_streaming_puts_do_not_deadlock_on_each_others_body_spool() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = encrypted_engine(&tmp, 4 * 1024 * 1024).await;
        let v = vec![3u8; 1_500_000];
        let body_a = engine.spool_acquire(v.len() as u64).await.unwrap();
        let body_b = engine.spool_acquire(v.len() as u64).await.unwrap();
        tokio::fs::write(body_a.path(), &v).await.unwrap();
        tokio::fs::write(body_b.path(), &v).await.unwrap();
        {
            let put = |key: &'static str, body| {
                engine.store_spooled_delta(
                    "b",
                    key,
                    body,
                    v.len() as u64,
                    None,
                    HashMap::new(),
                    None,
                )
            };
            let (ra, rb) = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                futures::future::join(put("x/a.png", &body_a), put("y/b.png", &body_b)),
            )
            .await
            .expect("two encrypted streaming PUTs deadlocked on each other's body spool");
            for r in [ra, rb] {
                match r {
                    Ok(_) | Err(EngineError::Overloaded(_)) => {}
                    Err(e) => panic!("unexpected error: {e:?}"),
                }
            }
        }
        // With room for both, both store and read back.
        drop((body_a, body_b));
        let body = engine.spool_acquire(v.len() as u64).await.unwrap();
        tokio::fs::write(body.path(), &v).await.unwrap();
        engine
            .store_spooled_delta(
                "b",
                "x/a.png",
                &body,
                v.len() as u64,
                None,
                HashMap::new(),
                None,
            )
            .await
            .unwrap();
        let (got, _) = engine.retrieve("b", "x/a.png").await.unwrap();
        assert_eq!(got, v);
    }

    /// The buffered codec's source file is a spool file. A buffered PUT
    /// encodes under the deltaspace lock, so it never waits for budget (a
    /// full budget is SlowDown at once); a GET holds nothing and waits.
    #[tokio::test]
    async fn buffered_codec_source_is_a_spool_file() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = FilesystemBackend::new(tmp.path().join("data"))
            .await
            .unwrap();
        backend.create_bucket("b").await.unwrap();
        let mut engine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None);
        engine.spool = Arc::new(
            crate::deltaglider::spool::SpoolDir::new(tmp.path().join("spool"), 4 * 1024 * 1024)
                .unwrap(),
        );
        let engine = Arc::new(engine);
        let v1: Vec<u8> = (0..300_000u32).map(|n| (n % 251) as u8).collect();
        let mut v2 = v1.clone();
        v2[1000] ^= 0xff;
        engine
            .store("b", "rel/a.zip", &v1, None, HashMap::new())
            .await
            .unwrap();

        let full = engine.spool_acquire(4 * 1024 * 1024).await.unwrap();
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            engine.store("b", "rel/b.zip", &v2, None, HashMap::new()),
        )
        .await
        .expect("a buffered PUT must not wait for spool budget under the lock");
        assert!(matches!(r, Err(EngineError::Overloaded(_))), "got {r:?}");
        drop(full);
        engine
            .store("b", "rel/b.zip", &v2, None, HashMap::new())
            .await
            .unwrap();

        // GET: waits for the budget, then decodes.
        let full = engine.spool_acquire(4 * 1024 * 1024).await.unwrap();
        let e = engine.clone();
        let get = tokio::spawn(async move { e.retrieve("b", "rel/b.zip").await });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!get.is_finished(), "the GET waits for spool budget");
        drop(full);
        let (got, _) = tokio::time::timeout(std::time::Duration::from_secs(10), get)
            .await
            .expect("the GET goes on once the budget is free")
            .unwrap()
            .unwrap();
        assert_eq!(got, v2);
    }

    /// A relayed multipart store holds spool budget: its relay parts. So it
    /// never waits for more (two completing uploads would each wait for the
    /// other's parts); with the budget in use it is a SlowDown at once.
    #[tokio::test]
    async fn encrypted_relayed_store_never_waits_for_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = encrypted_engine(&tmp, 4 * 1024 * 1024).await;
        let part = tmp.path().join("part1");
        let v = vec![5u8; 1024 * 1024];
        tokio::fs::write(&part, &v).await.unwrap();
        let store = || {
            engine.store_passthrough_relayed_parts_with_multipart_etag(
                "b",
                "x/a.png",
                std::slice::from_ref(&part),
                v.len() as u64,
                None,
                HashMap::new(),
                "\"0123456789abcdef0123456789abcdef-1\"".to_string(),
            )
        };
        let full = engine.spool_acquire(4 * 1024 * 1024).await.unwrap();
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), store())
            .await
            .expect("a relayed store must not wait for spool budget");
        assert!(matches!(r, Err(EngineError::Overloaded(_))), "got {r:?}");
        drop(full);
        store().await.unwrap();
        let (got, _) = engine.retrieve("b", "x/a.png").await.unwrap();
        assert_eq!(got, v);
    }

    /// Review-2 (144b303b): the deltaspace lock is keyed by the VIRTUAL
    /// bucket. Two bucket names aliased onto one real bucket share one
    /// `reference.bin`, but take different locks, so two first PUTs can both
    /// create a baseline.
    #[tokio::test]
    async fn review2_two_virtual_names_of_one_storage_share_the_deltaspace_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let yaml = format!(
            r#"
storage:
  backends:
    - name: local-disk
      type: filesystem
      path: {}
  buckets:
    releases:
      backend: local-disk
      alias: shared
    downloads:
      backend: local-disk
      alias: shared
"#,
            tmp.path().display()
        );
        let cfg = Config::from_yaml_str(&yaml).unwrap();
        let engine = DeltaGliderEngine::new(&cfg, None).await.unwrap();
        // The reference cache too: one entry for one reference.bin, so an
        // invalidation through one name reaches a read through the other.
        assert_eq!(
            engine.cache_key("releases", "fw"),
            engine.cache_key("downloads", "fw")
        );
        assert_ne!(
            engine.cache_key("releases", "fw"),
            engine.cache_key("releases", "fx")
        );
        let _held = engine.acquire_prefix_lock("releases", "fw").await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(200),
                engine.acquire_prefix_lock("downloads", "fw"),
            )
            .await
            .is_err(),
            "the same real deltaspace through two names must serialise"
        );
    }
}
