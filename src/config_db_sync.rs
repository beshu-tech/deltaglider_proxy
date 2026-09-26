// SPDX-License-Identifier: BUSL-1.1

//! S3 sync for the IAM config database.
//!
//! When `DGP_CONFIG_SYNC_BUCKET` is set, the encrypted config DB file is
//! synchronized to/from S3 (default key `.deltaglider/config.db`, override
//! with `DGP_CONFIG_SYNC_KEY`). This enables
//! multi-instance deployments to share IAM state.
//!
//! - On startup: download from S3 if the ETag differs from the local copy.
//! - After IAM mutations: upload the local DB to S3.
//! - Every 5 minutes: poll S3 ETag and download if changed.

use aws_credential_types::Credentials;
use aws_sdk_s3::config::BehaviorVersion;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use rand::Rng;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

use crate::config::BackendConfig;
use crate::config_db::ConfigDb;
use crate::iam::external_auth::ExternalAuthManager;
use crate::iam::{IamIndex, IamState, SharedIamState};

/// Default S3 object key for the config database file (override with `DGP_CONFIG_SYNC_KEY`).
pub const DEFAULT_CONFIG_SYNC_OBJECT_KEY: &str = ".deltaglider/config.db";

/// Synchronizes the encrypted config DB file to/from S3.
/// A validated, downloaded peer DB awaiting an IAM merge. The caller merges the
/// IAM tables out of `temp_path`, deletes it, and — only on success — calls
/// `commit_downloaded_etag(etag)` so a failed merge re-downloads next poll.
pub struct DownloadedDb {
    pub temp_path: std::path::PathBuf,
    pub etag: Option<String>,
    /// The copy opened only with a fallback key and was re-encrypted for the
    /// merge: the synced object is still under the old key, so it must be
    /// uploaded again (under the primary key) after the merge.
    pub migrated: bool,
    /// The schema version the peer wrote, read BEFORE the copy was migrated
    /// for the merge.
    pub peer_schema: Option<i32>,
}

/// The schema version of a downloaded copy as the peer wrote it, read with
/// the keys a synced copy may open with, before any migration. `None` when
/// none of them opens it.
pub(crate) fn peer_schema_version(
    path: &std::path::Path,
    keys: &crate::config_db::ConfigDbKeys,
) -> Result<Option<i32>, crate::config_db::ConfigDbError> {
    for k in std::iter::once(&keys.primary).chain(keys.fallbacks.iter().map(|(_, k)| k)) {
        if let Some(v) = crate::config_db::probe_schema_version(path, k.expose())? {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

/// Why an upload failed: a CAS conflict (peer wrote concurrently — reconcile
/// and retry) vs anything else (transport, permissions, empty DB, ...).
#[derive(Debug)]
pub enum UploadError {
    Conflict,
    Other(String),
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UploadError::Conflict => {
                write!(
                    f,
                    "upload conflict: remote config DB changed since last sync"
                )
            }
            UploadError::Other(e) => write!(f, "{e}"),
        }
    }
}

pub struct ConfigDbSync {
    s3_client: Client,
    bucket: String,
    object_key: String,
    local_path: PathBuf,
    last_etag: Arc<RwLock<Option<String>>>,
    /// The keys that may open a downloaded copy ([`ConfigDbKeys::for_synced_copy`]).
    /// A download under a fallback key is re-encrypted with the primary key
    /// before the merge.
    db_keys: crate::config_db::ConfigDbKeys,
    /// Set when an upload exhausted its retries; the periodic poll flushes it.
    needs_upload: AtomicBool,
    /// Serialises this node's own uploads. Two concurrent same-node uploads
    /// (e.g. create-user X then create-user Y in quick succession) each
    /// tokio::spawn `upload_with_reconcile`; without this, they read the DB
    /// file + expected etag independently, one wins the CAS, the other 412s and
    /// its reconcile whole-table-replaces the local IAM with the older remote
    /// blob — silently deleting the just-committed row (X-ray H10). Held across
    /// the whole read→PUT→reconcile→retry sequence so same-node uploads are
    /// strictly ordered; cross-node conflicts still use the reconcile path.
    upload_lock: tokio::sync::Mutex<()>,
}

impl ConfigDbSync {
    /// Create a new sync instance from the backend config and sync bucket name.
    ///
    /// Uses the same S3 credentials as the storage backend (DGP_BE_AWS_ACCESS_KEY_ID etc).
    /// Returns `None` if the backend is not S3 or credentials are missing.
    pub async fn new(
        backend_config: &BackendConfig,
        sync_bucket: String,
        object_key: String,
        local_path: PathBuf,
        db_keys: crate::config_db::ConfigDbKeys,
        accept_legacy_sync: bool,
    ) -> Result<Self, String> {
        let client = Self::build_client(backend_config).await?;

        // Clean up orphaned .db.tmp* files from previous interrupted downloads
        // (per-download unique suffixes — see download_if_newer).
        if let (Some(dir), Some(stem)) = (local_path.parent(), local_path.file_name()) {
            let prefix = format!("{}.tmp", stem.to_string_lossy());
            if let Ok(entries) = std::fs::read_dir(dir) {
                for e in entries.flatten() {
                    if e.file_name().to_string_lossy().starts_with(&prefix) {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
        }

        // S8: a synced copy opens only with a real key, never with the
        // bootstrap hash, unless the operator opts in for a rolling upgrade
        // (DGP_CONFIG_DB_ACCEPT_LEGACY_SYNC, read and logged by the boot).
        let db_keys = db_keys.for_synced_copy(accept_legacy_sync);

        // A park from before a restart comes back with the ETag its change
        // was based on, so the flush can still CAS on top of it.
        let pending = read_pending_marker(&pending_marker_path(&local_path));
        if pending.is_some() {
            warn!(
                "Config DB S3 sync: an upload parked before the restart is pending — \
                 it is flushed before any download"
            );
        }
        Ok(Self {
            s3_client: client,
            bucket: sync_bucket,
            object_key,
            last_etag: Arc::new(RwLock::new(
                pending.as_ref().and_then(|p| p.base_etag.clone()),
            )),
            local_path,
            db_keys,
            needs_upload: AtomicBool::new(pending.is_some()),
            upload_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// The primary config DB key (the key of the local DB after boot).
    pub fn db_key(&self) -> &str {
        self.db_keys.primary.expose()
    }

    /// Queue an upload for the next poll tick (set after retry exhaustion).
    /// Persisted next to the DB (with the base ETag), so a restart does not
    /// drop the change: the boot flushes it instead of downloading over it.
    pub async fn mark_needs_upload(&self) {
        self.needs_upload.store(true, Ordering::SeqCst);
        let marker = PendingUpload {
            base_etag: self.last_etag.read().await.clone(),
        };
        let path = pending_marker_path(&self.local_path);
        let body = serde_json::to_vec(&marker).unwrap_or_default();
        if let Err(e) = tokio::fs::write(&path, body).await {
            warn!(
                "Config DB S3 sync: could not persist the pending upload marker {}: {e}",
                path.display()
            );
        }
    }

    /// True while an upload is parked (in memory or from before a restart).
    pub fn has_pending_upload(&self) -> bool {
        self.needs_upload.load(Ordering::SeqCst)
    }

    /// Consume the pending-upload flag (the poll flush claims the work).
    pub fn take_needs_upload(&self) -> bool {
        let taken = self.needs_upload.swap(false, Ordering::SeqCst);
        if taken {
            let _ = std::fs::remove_file(pending_marker_path(&self.local_path));
        }
        taken
    }

    /// Build an S3 client from BackendConfig, reusing the same credentials.
    /// `pub` so the coordination-lease builder (in the binary crate's startup)
    /// shares the exact same client construction as the config sync.
    pub async fn build_client(config: &BackendConfig) -> Result<Client, String> {
        let (endpoint, region, force_path_style, access_key_id, secret_access_key, allow_local) =
            match config {
                BackendConfig::S3 {
                    endpoint,
                    region,
                    force_path_style,
                    access_key_id,
                    secret_access_key,
                    allow_local,
                    ..
                } => (
                    endpoint.clone(),
                    region.clone(),
                    *force_path_style,
                    access_key_id.clone(),
                    secret_access_key.clone(),
                    *allow_local,
                ),
                BackendConfig::Filesystem { .. } => {
                    return Err("Config DB S3 sync requires an S3 backend. \
                     Set DGP_CONFIG_SYNC_BUCKET only when using the S3 backend."
                        .to_string());
                }
            };

        let credentials = match (access_key_id, secret_access_key) {
            (Some(ref key_id), Some(ref secret)) => {
                Credentials::new(key_id, secret, None, None, "deltaglider_proxy-config-sync")
            }
            _ => {
                return Err("Config DB S3 sync requires backend S3 credentials \
                     (DGP_BE_AWS_ACCESS_KEY_ID and DGP_BE_AWS_SECRET_ACCESS_KEY)"
                    .to_string());
            }
        };

        let mut builder = aws_sdk_s3::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new(region))
            .credentials_provider(credentials)
            .force_path_style(force_path_style)
            .request_checksum_calculation(
                aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired,
            )
            .response_checksum_validation(
                aws_sdk_s3::config::ResponseChecksumValidation::WhenRequired,
            );

        if let Some(ref ep) = endpoint {
            // The same SSRF guard as the engine's backends: this client
            // sends signed requests to the endpoint too.
            builder = crate::storage::guard_s3_endpoint(builder, ep, allow_local)?;
        }

        Ok(Client::from_conf(builder.build()))
    }

    /// Check S3 for a newer config DB file and download it if the ETag differs.
    ///
    /// Returns `Some(DownloadedDb)` when a new version was downloaded + validated.
    /// The caller MUST merge its IAM tables into the live DB via
    /// `ConfigDb::merge_iam_from` (NOT a file swap — that would clobber per-node
    /// coordination state; see B3), delete the temp file, and — ONLY on a
    /// successful merge — call `commit_downloaded_etag(dl.etag)`. The ETag is
    /// deliberately NOT advanced here so a failed merge re-downloads next poll.
    /// Returns `None` when the local copy is already current.
    pub async fn download_if_newer(&self) -> Result<Option<DownloadedDb>, String> {
        // HEAD to get current ETag
        crate::storage::BACKEND_HEAD_REQUESTS.inc();
        let head_result = self
            .s3_client
            .head_object()
            .bucket(&self.bucket)
            .key(&self.object_key)
            .send()
            .await;

        let remote_etag = match head_result {
            Ok(head) => head.e_tag().map(|s| s.to_string()),
            Err(e) => {
                if head_error_is_absent(&e) {
                    debug!(
                        "Config DB not found in S3 (bucket={}) — using local copy",
                        self.bucket
                    );
                    return Ok(None);
                }
                return Err(format!(
                    "Failed to HEAD config DB in S3: {}",
                    describe_sdk_error(&e)
                ));
            }
        };

        // Compare with our last known ETag
        let current_etag = self.last_etag.read().await;
        if *current_etag == remote_etag {
            debug!("Config DB S3 ETag unchanged — no download needed");
            return Ok(None);
        }
        drop(current_etag);

        // Download the file
        let get_result = self
            .s3_client
            .get_object()
            .bucket(&self.bucket)
            .key(&self.object_key)
            .send()
            .await
            .map_err(|e| {
                format!(
                    "Failed to download config DB from S3: {}",
                    describe_sdk_error(&e)
                )
            })?;

        let get_etag = get_result.e_tag().map(|s| s.to_string());
        if get_etag != remote_etag {
            return Err(format!(
                "Config DB changed during download (HEAD etag={:?}, GET etag={:?}); retry later",
                remote_etag, get_etag
            ));
        }

        let body = get_result
            .body
            .collect()
            .await
            .map_err(|e| format!("Failed to read config DB body from S3: {}", e))?;

        let data = body.into_bytes();
        if data.is_empty() {
            return Err("Downloaded config DB from S3 is empty".to_string());
        }

        // Write to a per-download UNIQUE temp file (concurrent poll / sync-now /
        // conflict-reconcile downloads must never clobber each other's tmp),
        // then validate before the caller merges from it.
        let tmp_path = self
            .local_path
            .with_extension(format!("db.tmp.{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::write(&tmp_path, &data)
            .await
            .map_err(|e| format!("Failed to write temp config DB: {}", e))?;

        // Validate that the downloaded DB opens with our config DB key. A DB
        // under another key must NOT reach the merge — it would be unreadable.
        // A DB under an accepted fallback key (DGP_CONFIG_DB_KEY_PREVIOUS, or
        // the bootstrap hash with DGP_CONFIG_DB_ACCEPT_LEGACY_SYNC) is
        // re-encrypted with our key here, so the merge attaches it with the
        // primary key.
        // Read the peer's schema version BEFORE the open below migrates the
        // copy: after that, every copy reads as the current version.
        let peer_schema = match peer_schema_version(&tmp_path, &self.db_keys) {
            Ok(v) => v,
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                warn!("Config DB downloaded from S3 cannot be read — NOT merging it: {e}");
                return Ok(None);
            }
        };
        match peer_schema {
            Some(v) if v > crate::config_db::SCHEMA_VERSION => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                warn!(
                    "Config DB downloaded from S3 has schema v{v}, newer than this binary's \
                     v{} (rolling upgrade in progress?) — NOT merging it",
                    crate::config_db::SCHEMA_VERSION
                );
                return Ok(None);
            }
            Some(v) if v < crate::config_db::SCHEMA_VERSION => warn!(
                "Config DB downloaded from S3 has schema v{v}, older than this binary's v{} \
                 (a peer on an older release): a copy is migrated for the merge. Its rows \
                 without a sync_mtime have an unknown age, so a conflict with them goes to \
                 the bucket's copy",
                crate::config_db::SCHEMA_VERSION
            ),
            _ => {}
        }
        let migrated = match ConfigDb::open_with_keys(&tmp_path, &self.db_keys) {
            Ok((_, opened)) => {
                debug!("Downloaded config DB passed key validation");
                matches!(opened, crate::config_db::OpenedWith::Migrated(_))
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return match e {
                    crate::config_db::ConfigDbError::WrongPassphrase(_) => {
                        let msg = format!(
                            "the config DB in s3://{}/{} is encrypted with a different config \
                             DB key — NOT merging it. Set {} to the same value on every \
                             instance that shares this sync bucket",
                            self.bucket,
                            self.object_key,
                            crate::config_db::key::CONFIG_DB_KEY_ENV
                        );
                        tracing::error!("{msg}");
                        Err(msg)
                    }
                    crate::config_db::ConfigDbError::SchemaTooNew { .. } => {
                        tracing::warn!(
                            "Config DB downloaded from S3 comes from a newer binary (rolling \
                             upgrade in progress?) — NOT merging into the local copy: {e}"
                        );
                        Ok(None)
                    }
                    _ => {
                        tracing::warn!(
                            "Config DB downloaded from S3 cannot be opened — NOT merging into \
                             the local copy: {e}"
                        );
                        Ok(None)
                    }
                };
            }
        };

        // B3: do NOT rename over the live DB (that wholesale-clobbers per-node
        // coordination tables) and do NOT advance the ETag yet. The caller merges
        // ONLY the IAM tables out of the temp file, and ONLY on a SUCCESSFUL merge
        // calls `commit_downloaded_etag` — so a failed merge leaves the ETag
        // behind and the next poll retries (review fix: previously the ETag was
        // advanced here, stranding the node with stale IAM on a merge failure).
        info!(
            "Config DB downloaded from S3 (bucket={}, size={} bytes) — IAM merge pending",
            self.bucket,
            data.len()
        );
        Ok(Some(DownloadedDb {
            temp_path: tmp_path,
            etag: remote_etag,
            migrated,
            peer_schema,
        }))
    }

    /// Record that a downloaded version was successfully applied (IAM merged),
    /// so the next poll doesn't re-download it. Call ONLY after the merge
    /// succeeds — a failed merge must leave the ETag behind so the poll retries.
    pub async fn commit_downloaded_etag(&self, etag: Option<String>) {
        *self.last_etag.write().await = etag;
    }

    /// Upload the local config DB file to S3.
    ///
    /// Uses a conditional (compare-and-swap) PUT so two instances mutating
    /// IAM concurrently can't silently clobber each other's writes:
    ///   - if we've previously synced this object (`last_etag` is `Some`),
    ///     send `If-Match: <etag>` so the PUT fails with 412 when a peer
    ///     changed the remote copy since we last saw it;
    ///   - if we've never seen the remote object (`last_etag` is `None`),
    ///     send `If-None-Match: *` so the PUT fails with 412 if a peer
    ///     created it concurrently (instead of overwriting their copy).
    ///
    /// On a precondition failure the upload is reported as
    /// [`UploadError::Conflict`]; `upload_with_reconcile` pulls the peer's
    /// version, merges, and retries on top of the reconciled DB.
    /// `data` is a consistent snapshot of the DB file: read it with
    /// [`read_db_snapshot`] (under the DB lock), never straight from disk while
    /// the connection may be mid-commit (a torn upload).
    pub async fn upload(&self, data: Vec<u8>) -> Result<(), UploadError> {
        if data.is_empty() {
            return Err(UploadError::Other(
                "Local config DB is empty — refusing to upload".to_string(),
            ));
        }

        // Snapshot the ETag we expect the remote object to still carry. This is
        // the compare half of the compare-and-swap.
        let expected_etag = self.last_etag.read().await.clone();

        let mut put = self
            .s3_client
            .put_object()
            .bucket(&self.bucket)
            .key(&self.object_key)
            .body(ByteStream::from(data.clone()))
            .content_type("application/octet-stream");
        put = match &expected_etag {
            Some(etag) => put.if_match(etag),
            None => put.if_none_match("*"),
        };

        let put_result = match put.send().await {
            Ok(result) => result,
            Err(e) => {
                match classify_upload_error(&sdk_error_signal(&e)) {
                    UploadError::Conflict => {
                        // A peer instance updated the remote config DB since we
                        // last synced. Forget our stale ETag so the next download
                        // forces a fresh HEAD+GET, then surface the conflict.
                        *self.last_etag.write().await = None;
                        warn!(
                            "Config DB S3 upload conflict (bucket={}): remote copy changed since last sync \
                             (expected etag={:?}) — a peer instance wrote concurrently",
                            self.bucket, expected_etag
                        );
                        return Err(UploadError::Conflict);
                    }
                    UploadError::Other(_) => {
                        return Err(UploadError::Other(format!(
                            "Failed to upload config DB to S3: {}",
                            e
                        )));
                    }
                }
            }
        };

        // Store the ETag from the PUT response
        if let Some(etag) = put_result.e_tag() {
            *self.last_etag.write().await = Some(etag.to_string());
        }

        info!(
            "Config DB uploaded to S3 (bucket={}, size={} bytes)",
            self.bucket,
            data.len()
        );
        Ok(())
    }

    /// Download the raw config DB bytes from S3 without passphrase validation.
    /// Used by the recovery endpoint to try candidate passwords against the S3 copy.
    pub async fn download_raw(&self) -> Result<Vec<u8>, String> {
        let get_result = self
            .s3_client
            .get_object()
            .bucket(&self.bucket)
            .key(&self.object_key)
            .send()
            .await
            .map_err(|e| {
                format!(
                    "Failed to download config DB from S3: {}",
                    describe_sdk_error(&e)
                )
            })?;

        let body = get_result
            .body
            .collect()
            .await
            .map_err(|e| format!("Failed to read config DB body from S3: {}", e))?;

        let data = body.into_bytes().to_vec();
        if data.is_empty() {
            return Err("Config DB in S3 is empty".to_string());
        }

        Ok(data)
    }

    /// Boot-time gate: PROVE the coordination bucket enforces atomic
    /// conditional writes (`If-None-Match: *`) before any HA feature hinges on
    /// it. Returns `Err` on a bucket that can't be trusted — the caller CRASHES
    /// the process (a silent-clobber coordination bucket is a data-loss trap).
    ///
    /// Design (simple + 16-node-concurrency-safe):
    ///  1. WITNESS fast-path — if `.deltaglider/coordination-witness.json` exists
    ///     and is fresh, a prior boot already proved this bucket. Skip the probe.
    ///     (One cheap GET on the normal boot path.)
    ///  2. PROBE on a RANDOM key (uuid) — so 16 nodes booting at once each run a
    ///     self-contained probe that no peer's cleanup can disturb (a shared
    ///     probe key would race into false negatives → spurious crashes). The
    ///     probe is fail-closed: it demands a real `412`, the one signal a
    ///     silent-ignore backend can't fake.
    ///  3. WITNESS write — create-if-absent (`If-None-Match: *`). With 16 nodes,
    ///     exactly one wins the write and 15 get `412`; that lost race is itself
    ///     a free live-fire CAS test, and either way the witness now exists. A
    ///     STALE witness is refreshed with a plain overwrite (CAS already proven
    ///     in step 2; the timestamp bump isn't CAS-critical).
    pub async fn validate_coordination_bucket(&self) -> Result<CoordinationValidation, String> {
        validate_cas_bucket(&self.s3_client, &self.bucket, COORDINATION_WITNESS_KEY)
            .await
            .map_err(|failure| match failure {
                CasValidationFailure::NonCas => format!(
                    "Coordination bucket '{}' does NOT enforce atomic conditional writes \
                     (If-None-Match). HA coordination (leases, single-writer locks) would be \
                     UNSAFE — refusing to start. Use a coordination bucket on AWS S3, MinIO \
                     (>=2024-09), or Ceph/Hetzner; Backblaze B2 (501) and old MinIO/SeaweedFS \
                     (silent overwrite) are NOT supported.",
                    self.bucket
                ),
                CasValidationFailure::Indeterminate(e) => e,
            })
    }
}

/// Why a CAS validation did not pass. `NonCas` is DEFINITIVE (the backend
/// accepted an `If-None-Match:*` re-PUT it should have 412'd); `Indeterminate`
/// means the probe could not run (network, missing bucket) — callers decide
/// whether that is fatal (coordination: yes) or a loud warning (data-plane
/// capability gate: yes-warn, no-crash).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CasValidationFailure {
    NonCas,
    Indeterminate(String),
}

/// Generalized boot-time CAS validation of an arbitrary (client, bucket):
/// witness fast-path → random-key fail-closed probe → best-effort witness
/// write. The single implementation behind BOTH the coordination-bucket gate
/// and the per-backend write-capability gate (they differ only in witness key
/// and in how the caller treats failure).
pub async fn validate_cas_bucket(
    client: &Client,
    bucket: &str,
    witness_key: &str,
) -> Result<CoordinationValidation, CasValidationFailure> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // ── 1. Witness fast-path ──
    // `refreshing_stale` = we saw an EXISTING witness that aged out. It
    // matters for step 3: a stale witness must be OVERWRITTEN (its key already
    // exists, so a create-if-absent write would 412 and never bump the stamp).
    let mut refreshing_stale = false;
    match read_witness(client, bucket, witness_key).await {
        Ok(Some(w)) if witness_is_fresh(w.validated_at_unix, now, WITNESS_MAX_AGE_SECS) => {
            return Ok(CoordinationValidation::CachedWitness {
                validated_at_unix: w.validated_at_unix,
                validated_by: w.validated_by,
            });
        }
        Ok(Some(_)) => refreshing_stale = true, // exists but aged out → overwrite
        Ok(None) => {}                          // absent → create-if-absent
        Err(e) => {
            // A read error is not itself a validation failure (transient), but
            // don't fail on it — fall through to the probe, the real gate.
            tracing::debug!("CAS witness read failed on '{bucket}' (will probe): {e}");
        }
    }

    // ── 2. Isolated probe on a random key (fail-closed) ──
    let probe_key = format!(".deltaglider/_cwprobe/{}", uuid::Uuid::new_v4());
    match crate::coordination::cas_probe::probe_cas(client, bucket, &probe_key).await {
        Ok(true) => {}
        Ok(false) => return Err(CasValidationFailure::NonCas),
        Err(e) => return Err(CasValidationFailure::Indeterminate(e)),
    }

    // ── 3. Witness write (create-if-absent; overwrite if refreshing stale) ──
    write_witness(client, bucket, witness_key, now, refreshing_stale).await; // best-effort
    Ok(CoordinationValidation::Probed)
}

/// Read + parse a witness object. `Ok(None)` = absent (a 404/NoSuchKey).
async fn read_witness(client: &Client, bucket: &str, key: &str) -> Result<Option<Witness>, String> {
    match client.get_object().bucket(bucket).key(key).send().await {
        Ok(out) => {
            let bytes = out
                .body
                .collect()
                .await
                .map_err(|e| format!("witness body read: {e}"))?
                .into_bytes();
            Ok(serde_json::from_slice::<Witness>(&bytes).ok())
        }
        Err(e) => {
            if is_object_absent(&sdk_error_signal(&e)) {
                Ok(None)
            } else {
                Err(format!("{e:?}"))
            }
        }
    }
}

/// Best-effort witness write. Never fatal — the probe already proved the
/// bucket; a missing witness just re-probes next boot.
///
/// `overwrite` picks the mode:
///  - `false` (absent witness): create-if-absent (`If-None-Match:*`) so 16
///    concurrent nodes don't clobber — exactly one wins, the rest 412
///    harmlessly (a free live-fire CAS test).
///  - `true` (refreshing a STALE witness): plain overwrite — the key already
///    exists, so create-if-absent would 412 forever and never bump the stamp.
async fn write_witness(client: &Client, bucket: &str, key: &str, now: i64, overwrite: bool) {
    let body = serde_json::to_vec(&Witness {
        version: 1,
        validated_at_unix: now,
        validated_by: node_id(),
        primitive: "if-none-match-cas".to_string(),
    })
    .unwrap_or_default();
    let mut put = client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from(body))
        .content_type("application/json");
    if !overwrite {
        put = put.if_none_match("*");
    }
    // Ignore the result: 412 (peer won the create race) and any transient
    // error are both non-fatal — validation already succeeded via the probe.
    let _ = put.send().await;
}

/// Fixed object key for the coordination-bucket validation witness.
const COORDINATION_WITNESS_KEY: &str = ".deltaglider/coordination-witness.json";
/// Re-validate a witnessed bucket only after this age — a huge default so normal
/// boots always take the cheap fast-path, while still catching a backend that
/// silently REGRESSED (e.g. versioning toggled) within a season.
const WITNESS_MAX_AGE_SECS: i64 = 30 * 24 * 3600;

/// Outcome of a coordination-bucket validation (for logging provenance).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoordinationValidation {
    /// A prior boot's fresh witness let us skip the probe.
    CachedWitness {
        validated_at_unix: i64,
        validated_by: String,
    },
    /// We ran the live probe this boot (and it passed).
    Probed,
}

/// The witness object written after a successful validation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Witness {
    version: u32,
    validated_at_unix: i64,
    validated_by: String,
    primitive: String,
}

/// Pure freshness check — extracted so the TTL decision is unit-testable.
fn witness_is_fresh(validated_at: i64, now: i64, max_age: i64) -> bool {
    now >= validated_at && now - validated_at < max_age
}

/// Node identifier for witness provenance: THE durable node id the leases
/// and locks use (`DGP_NODE_ID`, `HOSTNAME`, else the persisted id), so a
/// witness names the same node they do. Purely diagnostic.
fn node_id() -> String {
    let db = crate::config_db::config_db_path();
    crate::coordination::durable_node_id(db.parent().unwrap_or_else(|| std::path::Path::new(".")))
}

/// Compact classification signal from a typed SDK error: HTTP status + error
/// code ONLY. Never feed the substring classifiers `format!("{e:?}")` — the
/// debug string embeds endpoint/bucket/request-ids that can contain "412"/
/// "501"/"404" and poison the match (e.g. a backend on port 9501).
pub(crate) fn sdk_error_signal<E>(e: &aws_sdk_s3::error::SdkError<E>) -> String
where
    E: aws_sdk_s3::error::ProvideErrorMetadata,
{
    use aws_sdk_s3::error::ProvideErrorMetadata;
    let code = e.code().unwrap_or("");
    match e {
        aws_sdk_s3::error::SdkError::ServiceError(svc) => {
            format!("status={} code={code}", svc.raw().status().as_u16())
        }
        _ => format!("transport code={code}"),
    }
}

/// Is this HeadObject error "the config DB object does not exist yet"?
fn head_error_is_absent<E>(e: &aws_sdk_s3::error::SdkError<E>) -> bool
where
    E: aws_sdk_s3::error::ProvideErrorMetadata,
{
    // Classify on status + code: `SdkError`'s Display for a service error is
    // only "service error", with no status or code in it.
    is_object_absent(&sdk_error_signal(e))
}

/// Human-readable form of an SDK error for logs and API responses: the
/// status + code signal, then the full source chain. `SdkError`'s own
/// Display is only "service error" / "dispatch failure", which tells an
/// operator nothing.
pub(crate) fn describe_sdk_error<E>(e: &aws_sdk_s3::error::SdkError<E>) -> String
where
    E: aws_sdk_s3::error::ProvideErrorMetadata + std::error::Error + 'static,
{
    format!(
        "{} ({})",
        aws_sdk_s3::error::DisplayErrorContext(e),
        sdk_error_signal(e)
    )
}

/// Pure classifier: did the backend LOUDLY reject the conditional request as
/// unimplemented (HTTP 501 / NotImplemented)? Backblaze B2 answers conditional
/// writes this way — a DEFINITIVE "no CAS", unlike a transport error.
pub(crate) fn is_not_implemented(err_str: &str) -> bool {
    err_str.contains("NotImplemented") || err_str.contains("501")
}

/// Pure: does a stringified GET error signal the object is ABSENT (a 404-class
/// response) rather than a real failure? Extracted so `read_witness`'s
/// absent-vs-error decision is unit-testable without a live backend. Shared with
/// the S3 coordination-lease read path.
pub(crate) fn is_object_absent(err_str: &str) -> bool {
    err_str.contains("NoSuchKey") || err_str.contains("NotFound") || err_str.contains("404")
}

/// Pure classifier: map a stringified S3 PUT error to [`UploadError`] — a
/// lost conditional write (412, or AWS's 409 ConditionalRequestConflict,
/// see [`crate::coordination::cas::conditional_write_lost`]) becomes
/// `Conflict`, everything else `Other`.
fn classify_upload_error(err_str: &str) -> UploadError {
    if crate::coordination::cas::conditional_write_lost(err_str) {
        UploadError::Conflict
    } else {
        UploadError::Other(err_str.to_string())
    }
}

/// Read the DB file while holding the DB lock. Every write goes through the
/// one locked connection, so no commit can be half-written while we read.
async fn read_db_snapshot(
    sync: &ConfigDbSync,
    config_db: &Option<Arc<Mutex<ConfigDb>>>,
) -> Result<Vec<u8>, String> {
    let _guard = match config_db {
        Some(db) => Some(db.lock().await),
        None => None,
    };
    tokio::fs::read(&sync.local_path)
        .await
        .map_err(|e| format!("Failed to read local config DB: {e}"))
}

/// On-disk park of an upload that exhausted its retries.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct PendingUpload {
    /// The remote ETag the parked change was based on (`None` after a
    /// conflict: the flush then reconciles, last-writer-wins).
    base_etag: Option<String>,
}

/// The DB this node last agreed with the sync bucket: the base of the
/// three-way IAM merge (`ConfigDb::merge_iam_from`).
pub fn sync_base_path(local_path: &std::path::Path) -> PathBuf {
    local_path.with_extension("db.sync-base")
}

/// Record `data` (the DB bytes the bucket now holds) as the merge base.
/// Written to a temp file and renamed, so a crash never leaves half a base.
async fn write_sync_base(local_path: &std::path::Path, data: &[u8]) {
    let base = sync_base_path(local_path);
    let tmp = base.with_extension("sync-base.tmp");
    let res = async {
        tokio::fs::write(&tmp, data).await?;
        tokio::fs::rename(&tmp, &base).await
    }
    .await;
    if let Err(e) = res {
        // A missing base only costs precision: the next merge lets the remote win.
        let _ = tokio::fs::remove_file(&base).await;
        warn!("Config DB S3 sync: could not record the merge base: {e}");
    }
}

/// Park an upload for the next boot's sync start (before any
/// `ConfigDbSync` exists). No base ETag: the flush 412s, merges the remote
/// copy, and uploads on top of it.
pub fn park_upload(local_path: &std::path::Path) -> std::io::Result<()> {
    let body = serde_json::to_vec(&PendingUpload { base_etag: None }).unwrap_or_default();
    std::fs::write(pending_marker_path(local_path), body)
}

/// Open the LIVE config DB with the config DB keys. EVERY path that opens it
/// (boot, recovery promotion, `--set-bootstrap-password`) goes through here:
/// when the DB moved to the primary key, the synced copy is still under the
/// old key, so one upload is parked for the sync start, while the old key is
/// still a fallback on the other nodes. `sync_enabled = false` parks nothing
/// (no sync bucket).
pub fn open_live_db(
    path: &std::path::Path,
    keys: &crate::config_db::ConfigDbKeys,
    sync_enabled: bool,
) -> Result<(ConfigDb, crate::config_db::OpenedWith), crate::config_db::ConfigDbError> {
    let (db, opened) = ConfigDb::open_with_keys(path, keys)?;
    if sync_enabled && matches!(opened, crate::config_db::OpenedWith::Migrated(_)) {
        if let Err(e) = park_upload(path) {
            warn!("Could not queue the re-encrypted config DB for upload: {e}");
        }
    }
    Ok((db, opened))
}

fn pending_marker_path(local_path: &std::path::Path) -> PathBuf {
    local_path.with_extension("db.sync-pending")
}

fn read_pending_marker(path: &std::path::Path) -> Option<PendingUpload> {
    let bytes = std::fs::read(path).ok()?;
    // An unreadable marker still means "an upload is pending".
    Some(serde_json::from_slice(&bytes).unwrap_or(PendingUpload { base_etag: None }))
}

/// Maximum upload attempts before parking the work on the poll flush.
const MAX_UPLOAD_ATTEMPTS: u32 = 3;

/// Upload the config DB with reconcile-then-retry CAS-conflict handling.
///
/// Converges because each retry merges peer state FIRST (revocations are
/// monotonic MAX-upserts), so the re-upload carries both sides' facts.
/// On exhaustion the upload is queued (`mark_needs_upload`) so the periodic
/// poll flushes it — a revocation is never silently dropped.
#[allow(clippy::too_many_arguments)]
pub async fn upload_with_reconcile(
    sync: &ConfigDbSync,
    config_db: &Option<Arc<Mutex<ConfigDb>>>,
    db_key: &str,
    iam_state: &SharedIamState,
    external_auth: &Option<Arc<ExternalAuthManager>>,
    sessions: Option<&Arc<crate::session::SessionStore>>,
    context: &str,
) -> Result<(), UploadError> {
    // Serialise this node's own uploads: two concurrent same-node uploads that
    // interleave read-then-PUT let the loser's reconcile whole-table-replace the
    // local IAM with an older remote blob, deleting a just-committed row (H10).
    let _guard = sync.upload_lock.lock().await;
    let mut last_err = UploadError::Other("upload never attempted".to_string());
    for attempt in 1..=MAX_UPLOAD_ATTEMPTS {
        // Re-read per attempt: a reconcile below changes the file.
        let data = match read_db_snapshot(sync, config_db).await {
            Ok(d) => d,
            Err(e) => {
                warn!("Config DB sync ({context}): {e}");
                last_err = UploadError::Other(e);
                break;
            }
        };
        match sync.upload(data.clone()).await {
            Ok(()) => {
                write_sync_base(&sync.local_path, &data).await;
                return Ok(());
            }
            Err(UploadError::Conflict) => {
                last_err = UploadError::Conflict;
                // Pull + three-way merge the peer's version so the retried
                // upload carries both sides' changes. Only a row changed on
                // BOTH sides resolves last-writer-wins (audited).
                warn!("Config DB sync ({context}): CAS conflict — merging peer state before retry");
                if let Err(e) = pull_locked(
                    sync,
                    config_db,
                    db_key,
                    iam_state,
                    external_auth,
                    sessions,
                    context,
                )
                .await
                {
                    warn!("Config DB sync ({context}): reconcile download failed: {e}");
                }
            }
            Err(UploadError::Other(e)) => {
                warn!("Config DB sync ({context}): upload attempt {attempt} failed: {e}");
                last_err = UploadError::Other(e);
            }
        }
        if attempt < MAX_UPLOAD_ATTEMPTS {
            let jitter_ms = rand::thread_rng().gen_range(100..=300);
            tokio::time::sleep(std::time::Duration::from_millis(jitter_ms)).await;
        }
    }
    sync.mark_needs_upload().await;
    warn!("Config DB sync ({context}): upload retries exhausted — queued for next poll tick");
    Err(last_err)
}

/// Download the remote DB when it changed and merge its IAM into the live DB.
/// Returns `Ok(None)` when the local copy is current, `Ok(Some(applied))`
/// after a download. Holds the upload lock: a merge and an upload never
/// interleave, so the merge base always matches what the bucket held.
#[allow(clippy::too_many_arguments)]
pub async fn pull_and_merge(
    sync: &ConfigDbSync,
    config_db: &Option<Arc<Mutex<ConfigDb>>>,
    db_key: &str,
    iam_state: &SharedIamState,
    external_auth: &Option<Arc<ExternalAuthManager>>,
    sessions: Option<&Arc<crate::session::SessionStore>>,
    context: &str,
) -> Result<Option<bool>, String> {
    let _guard = sync.upload_lock.lock().await;
    pull_locked(
        sync,
        config_db,
        db_key,
        iam_state,
        external_auth,
        sessions,
        context,
    )
    .await
}

/// `pull_and_merge` for a caller that already holds the upload lock.
async fn pull_locked(
    sync: &ConfigDbSync,
    config_db: &Option<Arc<Mutex<ConfigDb>>>,
    db_key: &str,
    iam_state: &SharedIamState,
    external_auth: &Option<Arc<ExternalAuthManager>>,
    sessions: Option<&Arc<crate::session::SessionStore>>,
    context: &str,
) -> Result<Option<bool>, String> {
    let Some(dl) = sync.download_if_newer().await? else {
        return Ok(None);
    };
    let applied = reopen_and_rebuild_iam(
        config_db,
        db_key,
        iam_state,
        external_auth,
        sessions,
        &dl.temp_path,
        context,
    )
    .await;
    // Commit the ETag only on a successful merge so a failure retries.
    if applied {
        sync.commit_downloaded_etag(dl.etag).await;
        // The synced object is under a fallback key (a rotation's previous
        // key, or the legacy hash): queue an upload so it moves to the
        // primary key while the old key is still accepted.
        if dl.migrated {
            sync.mark_needs_upload().await;
        }
    }
    Ok(Some(applied))
}

/// Reopen the config DB file after an S3-sync download has replaced it
/// on disk, and rebuild the in-memory IAM index from the new content.
///
/// Moved into `config_db_sync` so it can be shared by:
///   - startup sync (`init_config_sync`)
///   - the periodic poll task (`spawn_config_sync_poll`)
///   - the operator-triggered `POST /api/admin/config/sync-now` endpoint
///
/// Previously lived in `src/startup.rs`, which is a binary-only module
/// (not re-exported by `lib.rs`), so the admin handler couldn't reach
/// it. Keeping this function in the library side preserves the "one
/// path for config-sync state application" invariant — any future
/// trigger mounts on top without re-implementing IAM index + external
/// auth rebuild.
///
/// Gracefully no-ops when `config_db` is `None` (legacy/open-access
/// mode, no IAM DB to reopen).
/// Returns `true` if the IAM merge was applied (so the caller can commit the
/// downloaded ETag); `false` on any failure, so the next poll retries.
#[allow(clippy::too_many_arguments)]
pub async fn reopen_and_rebuild_iam(
    config_db: &Option<Arc<Mutex<ConfigDb>>>,
    db_key: &str,
    iam_state: &SharedIamState,
    external_auth: &Option<Arc<ExternalAuthManager>>,
    sessions: Option<&Arc<crate::session::SessionStore>>,
    downloaded: &std::path::Path,
    context: &str,
) -> bool {
    let Some(db_arc) = config_db else {
        // No live DB (legacy/open mode) — nothing to merge into. Drop the temp.
        // Treat as applied (there's no IAM to converge), so we don't re-download.
        let _ = tokio::fs::remove_file(downloaded).await;
        return true;
    };
    let db = db_arc.lock().await;
    // B3: merge ONLY the IAM tables out of the downloaded peer DB into the live
    // connection — the live coordination tables (jobs/leases/outbox/cursors)
    // stay intact (a file swap would clobber them). D16: a three-way merge
    // against the last synced DB, so a change on either side survives.
    let base = sync_base_path(db.local_path());
    let merge = db.merge_iam_from(downloaded, Some(&base), db_key);
    let report = match merge {
        Ok(report) => {
            // The downloaded copy is what the bucket holds now: the next base.
            if let Err(e) = tokio::fs::rename(downloaded, &base).await {
                warn!("Config DB S3 sync ({context}): could not record the merge base: {e}");
                let _ = tokio::fs::remove_file(downloaded).await;
                let _ = tokio::fs::remove_file(&base).await;
            }
            report
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(downloaded).await;
            warn!(
                "Config DB S3 sync ({}): failed to merge IAM after download: {}",
                context, e
            );
            return false;
        }
    };
    if !report.base_used {
        info!(
            "Config DB S3 sync ({context}): no merge base yet — merged as a union, so a \
             delete that was not synced yet comes back (first sync or upgrade)"
        );
    }
    for c in &report.conflicts {
        warn!(
            "Config DB S3 sync ({context}): {} '{}' changed on this node and a peer; kept {}",
            c.table, c.target, c.resolution
        );
        crate::audit::audit_log(
            "iam_sync_conflict",
            "config-sync",
            &format!("{}:{} -> {}", c.table, c.target, c.resolution),
            &axum::http::HeaderMap::new(),
            "",
            "",
        );
    }
    if let Some(sessions) = sessions {
        let ended = sessions.revoke_external_user_ids(&report.stale_user_ids);
        if ended > 0 {
            info!(
                "Config DB S3 sync ({context}): ended {ended} external session(s) whose user id moved"
            );
        }
    }

    // Rebuild IAM index from the new DB
    let users = db.load_users().unwrap_or_default();
    let groups = db.load_groups().unwrap_or_default();
    let count = users.len();
    let group_count = groups.len();
    // An empty synced DB falls back to THIS node's bootstrap credential,
    // never to open access (see `build_iam_state`).
    let state = IamIndex::build_iam_state(users, groups, &iam_state.load());
    if matches!(&state, IamState::Iam(_)) {
        info!(
            "IAM index rebuilt from S3-synced DB ({} users, {} groups) [{}]",
            count, group_count, context
        );
    }
    iam_state.store(Arc::new(state));

    // Refresh the session-revocation snapshot from the just-merged table so a
    // revoke performed on another instance takes effect here (the cross-instance
    // stolen-cookie escape hatch).
    if let Some(sessions) = sessions {
        if let Ok(rows) = db.load_session_revocations() {
            sessions.set_revocations(rows);
        }
    }

    // Rebuild ExternalAuthManager from the new DB. Release the DB
    // lock before the async discovery round — it can take seconds
    // against real OIDC providers.
    if let Some(ref ext_auth) = external_auth {
        let providers = db.load_auth_providers().unwrap_or_default();
        // ALWAYS rebuild — an EMPTY list means every provider was deleted on the
        // peer, and skipping the rebuild would leave the deleted provider's live
        // discovery/client config in memory, so OAuth logins through it keep
        // succeeding here indefinitely. Only the async discovery round is skipped
        // when empty (nothing to discover).
        let is_empty = providers.is_empty();
        ext_auth.rebuild(&providers);
        drop(db);
        if !is_empty {
            ext_auth.discover_all().await;
        }
        info!(
            "External auth providers rebuilt from S3-synced DB ({} providers) [{}]",
            ext_auth.provider_names().len(),
            context
        );
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dead_s3_backend() -> BackendConfig {
        BackendConfig::S3 {
            // Port 1: connection refused, so every upload fails fast.
            endpoint: Some("http://127.0.0.1:1".into()),
            region: "us-east-1".into(),
            force_path_style: true,
            access_key_id: Some("k".into()),
            secret_access_key: Some("s".into()),
            allow_local: true,
            session_token: None,
        }
    }

    /// D16: the schema version of a peer copy is read before the download
    /// check migrates it; after the migration it always reads as current, so
    /// a check made then can never see an older peer.
    #[test]
    fn an_older_peer_copy_is_detected_before_its_migration() {
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("peer.db");
        let key = "k".repeat(40);
        {
            let db = ConfigDb::open_or_create(&copy, &key).unwrap();
            db.conn.pragma_update(None, "user_version", 25).unwrap();
        }
        let keys = crate::config_db::ConfigDbKeys::primary_only(&key);
        assert_eq!(peer_schema_version(&copy, &keys).unwrap(), Some(25));
        drop(ConfigDb::open_with_keys(&copy, &keys).unwrap());
        assert_eq!(
            peer_schema_version(&copy, &keys).unwrap(),
            Some(crate::config_db::SCHEMA_VERSION),
            "the migration hides the peer's version"
        );
        let other = crate::config_db::ConfigDbKeys::primary_only(&"x".repeat(40));
        assert_eq!(peer_schema_version(&copy, &other).unwrap(), None);
    }

    /// Every migration of the live DB queues the upload that moves the synced
    /// copy to the new key (only with a sync bucket).
    #[test]
    fn a_live_db_that_moves_key_parks_one_upload() {
        const HASH: &str = "$2b$04$legacyhashlegacyhashlegacyhashlegacyhash";
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("deltaglider_config.db");
        let keys = crate::config_db::ConfigDbKeys::primary_only(&"k".repeat(40))
            .with_fallback(crate::config_db::FallbackKind::LegacyBootstrapHash, HASH);
        drop(ConfigDb::open_or_create(&db_path, HASH).unwrap());
        let marker = pending_marker_path(&db_path);
        drop(open_live_db(&db_path, &keys, true).unwrap());
        assert!(marker.exists(), "the migrated DB must be queued for upload");
        std::fs::remove_file(&marker).unwrap();
        drop(open_live_db(&db_path, &keys, true).unwrap());
        assert!(!marker.exists(), "a DB already on the key queues nothing");
        let other = dir.path().join("single.db");
        drop(ConfigDb::open_or_create(&other, HASH).unwrap());
        drop(open_live_db(&other, &keys, false).unwrap());
        assert!(
            !pending_marker_path(&other).exists(),
            "no sync bucket, no park"
        );
    }

    /// S8: the bootstrap hash sits in configs and backups, so it must not
    /// open a copy from the shared bucket: with it, anyone who can write to
    /// the bucket and knows the hash plants an IAM DB on every node. The
    /// boot fallback list still has it (local DB migration); the sync does not.
    #[tokio::test]
    async fn a_synced_copy_under_the_legacy_hash_is_refused_by_default() {
        const HASH: &str = "$2b$04$legacyhashlegacyhashlegacyhashlegacyhash";
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("deltaglider_config.db");
        let keys = crate::config_db::ConfigDbKeys::primary_only(&"k".repeat(40))
            .with_fallback(crate::config_db::FallbackKind::LegacyBootstrapHash, HASH);
        let sync = ConfigDbSync::new(
            &dead_s3_backend(),
            "sync".into(),
            "k.db".into(),
            db_path.clone(),
            keys,
            false,
        )
        .await
        .unwrap();
        // A planted copy: encrypted with the hash, carrying an admin user.
        let planted = dir.path().join("planted.db");
        ConfigDb::open_or_create(&planted, HASH)
            .unwrap()
            .create_user("intruder", "AKINTRUDER01", "s", true, &[])
            .unwrap();
        let opened = ConfigDb::open_with_keys(&planted, &sync.db_keys);
        assert!(
            matches!(
                opened,
                Err(crate::config_db::ConfigDbError::WrongPassphrase(_))
            ),
            "the sync accepted a copy under the bootstrap hash"
        );
    }

    /// D16: an upload that exhausts its retries (S3 down) is parked for the
    /// poll flush. The park must survive a restart, together with the ETag
    /// the change was based on: otherwise the next boot downloads the remote
    /// copy over the local change, and the change (already answered 200) is
    /// lost on every node.
    #[tokio::test]
    async fn a_boot_park_is_flushed_by_the_next_sync_start() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("deltaglider_config.db");
        park_upload(&db_path).unwrap();
        let sync = ConfigDbSync::new(
            &dead_s3_backend(),
            "sync".into(),
            "k.db".into(),
            db_path.clone(),
            crate::config_db::ConfigDbKeys::primary_only("pw"),
            false,
        )
        .await
        .unwrap();
        assert!(sync.has_pending_upload());
        assert_eq!(sync.last_etag.read().await.as_deref(), None);
    }

    #[tokio::test]
    async fn parked_upload_survives_a_restart_with_its_base_etag() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("deltaglider_config.db");
        let db = Arc::new(Mutex::new(
            ConfigDb::open_or_create(&db_path, "pw").unwrap(),
        ));
        let sync = ConfigDbSync::new(
            &dead_s3_backend(),
            "sync".into(),
            "k.db".into(),
            db_path.clone(),
            crate::config_db::ConfigDbKeys::primary_only("pw"),
            false,
        )
        .await
        .unwrap();
        sync.commit_downloaded_etag(Some("\"base\"".into())).await;
        let iam: SharedIamState = Arc::new(arc_swap::ArcSwap::from_pointee(IamState::Disabled));
        let res =
            upload_with_reconcile(&sync, &Some(db.clone()), "pw", &iam, &None, None, "test").await;
        assert!(res.is_err(), "the dead endpoint must fail the upload");
        drop(sync);

        let restarted = ConfigDbSync::new(
            &dead_s3_backend(),
            "sync".into(),
            "k.db".into(),
            db_path.clone(),
            crate::config_db::ConfigDbKeys::primary_only("pw"),
            false,
        )
        .await
        .unwrap();
        assert!(
            restarted.has_pending_upload(),
            "the parked upload was lost on restart"
        );
        assert_eq!(
            restarted.last_etag.read().await.as_deref(),
            Some("\"base\""),
            "the base ETag must come back, or the flush 412s and loses the change"
        );
        assert!(restarted.take_needs_upload());
        assert!(!restarted.has_pending_upload(), "take clears the park");
        drop(restarted);
        let again = ConfigDbSync::new(
            &dead_s3_backend(),
            "sync".into(),
            "k.db".into(),
            db_path,
            crate::config_db::ConfigDbKeys::primary_only("pw"),
            false,
        )
        .await
        .unwrap();
        assert!(
            !again.has_pending_upload(),
            "a taken park must not come back"
        );
    }

    fn head_error(
        status: u16,
    ) -> aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::head_object::HeadObjectError> {
        use aws_smithy_runtime_api::http::{Response, StatusCode};
        let inner = aws_sdk_s3::operation::head_object::HeadObjectError::NotFound(
            aws_sdk_s3::types::error::NotFound::builder().build(),
        );
        let resp = Response::new(
            StatusCode::try_from(status).unwrap(),
            aws_smithy_types::body::SdkBody::empty(),
        );
        aws_sdk_s3::error::SdkError::service_error(inner, resp)
    }

    /// A first boot against an empty sync bucket gets HeadObject 404. The
    /// SDK's Display for a service error is just "service error", so the
    /// decision must come from the status and code, not the message.
    #[test]
    fn head_404_is_absent_and_403_is_not() {
        assert!(head_error_is_absent(&head_error(404)));
        assert!(!head_error_is_absent(&head_error(403)));
    }

    #[test]
    fn witness_freshness_truth_table() {
        let max = WITNESS_MAX_AGE_SECS;
        // Just written → fresh.
        assert!(witness_is_fresh(1000, 1000, max));
        // One second short of the TTL → fresh.
        assert!(witness_is_fresh(1000, 1000 + max - 1, max));
        // Exactly at the TTL → stale (re-validate).
        assert!(!witness_is_fresh(1000, 1000 + max, max));
        // Well past → stale.
        assert!(!witness_is_fresh(1000, 1000 + max + 999_999, max));
        // Clock skew: witness "from the future" → treated as stale, not fresh
        // (guards against a bad clock making a bogus witness look eternally valid).
        assert!(!witness_is_fresh(2000, 1000, max));
    }

    #[test]
    fn object_absent_detected_from_common_shapes() {
        // The sdk_error_signal contract shape:
        assert!(is_object_absent("status=404 code=NoSuchKey"));
        assert!(!is_object_absent("status=403 code=AccessDenied"));
        assert!(is_object_absent("service error: NoSuchKey"));
        assert!(is_object_absent("dispatch failure: NotFound"));
        assert!(is_object_absent("HTTP 404"));
        assert!(!is_object_absent("AccessDenied"));
        assert!(!is_object_absent("PreconditionFailed"));
    }

    #[test]
    fn witness_json_round_trips() {
        let w = Witness {
            version: 1,
            validated_at_unix: 1_783_000_000,
            validated_by: "node-abc".into(),
            primitive: "if-none-match-cas".into(),
        };
        let bytes = serde_json::to_vec(&w).unwrap();
        let back: Witness = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.validated_at_unix, w.validated_at_unix);
        assert_eq!(back.primitive, "if-none-match-cas");
    }

    #[test]
    fn not_implemented_classification() {
        // B2-style loud rejections → DEFINITIVE non-CAS.
        assert!(is_not_implemented(
            "service error: NotImplemented: conditional writes not supported"
        ));
        assert!(is_not_implemented("http status: 501"));
        // The sdk_error_signal contract shape — and the poisoning class it
        // exists to prevent: "501" in an endpoint/bucket must never classify.
        assert!(is_not_implemented("status=501 code=NotImplemented"));
        assert!(!is_not_implemented("status=403 code=AccessDenied"));
        // Transport-class errors are NOT "not implemented" — they must map to
        // Indeterminate, never to a fatal non-CAS verdict.
        assert!(!is_not_implemented("dispatch failure: connection refused"));
        assert!(!is_not_implemented("timeout waiting for response"));
        assert!(!is_not_implemented(
            "service error: AccessDenied (status 403)"
        ));
        assert!(!is_not_implemented(""));
    }

    #[test]
    fn upload_error_classification() {
        // CAS precondition shapes → Conflict (reconcile-then-retry path).
        assert!(matches!(
            classify_upload_error("service error: PreconditionFailed"),
            UploadError::Conflict
        ));
        assert!(matches!(
            classify_upload_error("dispatch failure: response status: 412"),
            UploadError::Conflict
        ));
        // Everything else → Other, carrying the original message.
        match classify_upload_error("dispatch failure: connection refused") {
            UploadError::Other(e) => assert!(e.contains("connection refused")),
            UploadError::Conflict => panic!("transport error misclassified as conflict"),
        }
        assert!(matches!(classify_upload_error(""), UploadError::Other(_)));
        // AWS answers two racing conditional writes with 409: the peer won,
        // so it is a conflict to reconcile, not a failed upload.
        assert!(matches!(
            classify_upload_error("status=409 code=ConditionalRequestConflict"),
            UploadError::Conflict
        ));
        assert!(matches!(
            classify_upload_error("status=409 code=BucketNotEmpty"),
            UploadError::Other(_)
        ));
    }
}
