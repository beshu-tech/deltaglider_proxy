// SPDX-License-Identifier: GPL-3.0-only

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
    /// The local bootstrap password hash, used to validate downloaded DBs.
    bootstrap_password_hash: String,
    /// Set when an upload exhausted its retries; the periodic poll flushes it.
    needs_upload: AtomicBool,
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
        bootstrap_password_hash: String,
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

        Ok(Self {
            s3_client: client,
            bucket: sync_bucket,
            object_key,
            local_path,
            last_etag: Arc::new(RwLock::new(None)),
            bootstrap_password_hash,
            needs_upload: AtomicBool::new(false),
        })
    }

    /// Queue an upload for the next poll tick (set after retry exhaustion).
    pub fn mark_needs_upload(&self) {
        self.needs_upload.store(true, Ordering::SeqCst);
    }

    /// Consume the pending-upload flag (the poll flush claims the work).
    pub fn take_needs_upload(&self) -> bool {
        self.needs_upload.swap(false, Ordering::SeqCst)
    }

    /// Build an S3 client from BackendConfig, reusing the same credentials.
    async fn build_client(config: &BackendConfig) -> Result<Client, String> {
        let (endpoint, region, force_path_style, access_key_id, secret_access_key) = match config {
            BackendConfig::S3 {
                endpoint,
                region,
                force_path_style,
                access_key_id,
                secret_access_key,
                ..
            } => (
                endpoint.clone(),
                region.clone(),
                *force_path_style,
                access_key_id.clone(),
                secret_access_key.clone(),
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
            builder = builder.endpoint_url(ep);
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
                let err_str = format!("{}", e);
                if err_str.contains("404")
                    || err_str.contains("NoSuchKey")
                    || err_str.contains("Not Found")
                {
                    debug!(
                        "Config DB not found in S3 (bucket={}) — using local copy",
                        self.bucket
                    );
                    return Ok(None);
                }
                return Err(format!("Failed to HEAD config DB in S3: {}", e));
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
            .map_err(|e| format!("Failed to download config DB from S3: {}", e))?;

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

        // Validate we can open the downloaded DB with our local bootstrap password.
        // If the remote DB was encrypted with a different password, we must NOT replace
        // our local copy — it would be unreadable and break IAM.
        match ConfigDb::open_or_create(&tmp_path, &self.bootstrap_password_hash) {
            Ok(_) => {
                debug!("Downloaded config DB passed passphrase validation");
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                tracing::warn!(
                    "Config DB downloaded from S3 is encrypted with a different bootstrap password — \
                     NOT replacing local copy: {}",
                    e
                );
                return Ok(None);
            }
        }

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
    pub async fn upload(&self) -> Result<(), UploadError> {
        let data = tokio::fs::read(&self.local_path)
            .await
            .map_err(|e| UploadError::Other(format!("Failed to read local config DB: {}", e)))?;

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
                let err_str = format!("{}", e);
                match classify_upload_error(&err_str) {
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

    /// Poll S3 for ETag changes. Called periodically (every 5 minutes).
    /// Returns `Some(temp_path)` when a new version was downloaded (caller merges
    /// IAM tables + deletes the temp), `None` otherwise.
    pub async fn poll_and_sync(&self) -> Result<Option<DownloadedDb>, String> {
        self.download_if_newer().await
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
            .map_err(|e| format!("Failed to download config DB from S3: {}", e))?;

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
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let witness_key = COORDINATION_WITNESS_KEY;

        // ── 1. Witness fast-path ──
        // `refreshing_stale` = we saw an EXISTING witness that aged out. It
        // matters for step 3: a stale witness must be OVERWRITTEN (its key already
        // exists, so a create-if-absent write would 412 and never bump the stamp).
        let mut refreshing_stale = false;
        match self.read_witness(witness_key).await {
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
                // don't crash on it — fall through to the probe, which is the real
                // gate. Log via the returned variant if the probe then passes.
                tracing::debug!("coordination witness read failed (will probe): {e}");
            }
        }

        // ── 2. Isolated probe on a random key (fail-closed) ──
        let probe_key = format!(".deltaglider/_cwprobe/{}", uuid::Uuid::new_v4());
        let supported = self.probe_conditional_write(&probe_key).await?;
        if !supported {
            return Err(format!(
                "Coordination bucket '{}' does NOT enforce atomic conditional writes \
                 (If-None-Match). HA coordination (leases, single-writer locks) would be \
                 UNSAFE — refusing to start. Use a coordination bucket on AWS S3, MinIO \
                 (>=2024-09), or Ceph/Hetzner; Backblaze B2 (501) and old MinIO/SeaweedFS \
                 (silent overwrite) are NOT supported.",
                self.bucket
            ));
        }

        // ── 3. Witness write (create-if-absent; overwrite if refreshing stale) ──
        self.write_witness(witness_key, now, refreshing_stale).await; // best-effort
        Ok(CoordinationValidation::Probed)
    }

    /// Read + parse the witness object. `Ok(None)` = absent (a 404/NoSuchKey).
    async fn read_witness(&self, key: &str) -> Result<Option<Witness>, String> {
        match self
            .s3_client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
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
                let s = format!("{e:?}");
                if is_object_absent(&s) {
                    Ok(None)
                } else {
                    Err(s)
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
    async fn write_witness(&self, key: &str, now: i64, overwrite: bool) {
        let body = serde_json::to_vec(&Witness {
            version: 1,
            validated_at_unix: now,
            validated_by: node_id(),
            primitive: "if-none-match-cas".to_string(),
        })
        .unwrap_or_default();
        let mut put = self
            .s3_client
            .put_object()
            .bucket(&self.bucket)
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

    /// The isolated two-step conditional-write probe on a caller-owned key.
    /// `Ok(true)` iff the re-PUT with `If-None-Match:*` returned a real `412`.
    async fn probe_conditional_write(&self, key: &str) -> Result<bool, String> {
        // Step 1: unconditional PUT to establish existence.
        self.s3_client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from_static(b"1"))
            .send()
            .await
            .map_err(|e| format!("probe could not write to '{}': {e:?}", self.bucket))?;
        // Step 2: re-PUT If-None-Match:* on the SAME key — MUST be 412.
        let put2 = self
            .s3_client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from_static(b"2"))
            .if_none_match("*")
            .send()
            .await;
        let supported = match &put2 {
            Ok(_) => false, // precondition ignored → silent overwrite
            Err(e) => is_precondition_failed(&format!("{e:?}")),
        };
        // Best-effort cleanup.
        let _ = self
            .s3_client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await;
        Ok(supported)
    }
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

/// Best-effort stable-ish node identifier for witness provenance (hostname or a
/// random fallback). Purely diagnostic — never used for coordination decisions.
fn node_id() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("node-{}", uuid::Uuid::new_v4()))
}

/// Pure classifier: does this stringified S3 SDK error represent a failed
/// conditional-write precondition (HTTP 412)?
///
/// The conditional PUT in [`ConfigDbSync::upload`] relies on the backend
/// rejecting the request with `412 Precondition Failed` when the `If-Match`
/// / `If-None-Match` guard doesn't hold. AWS S3 and MinIO both surface this
/// as `PreconditionFailed` / a 412 status in the error display string.
/// Extracted as a pure fn so the decision is unit-testable without a live
/// S3 backend (per the project's "pure functions at decision points" rule).
fn is_precondition_failed(err_str: &str) -> bool {
    err_str.contains("PreconditionFailed")
        || err_str.contains("Precondition Failed")
        || err_str.contains("412")
}

/// Pure: does a stringified GET error signal the object is ABSENT (a 404-class
/// response) rather than a real failure? Extracted so `read_witness`'s
/// absent-vs-error decision is unit-testable without a live backend.
fn is_object_absent(err_str: &str) -> bool {
    err_str.contains("NoSuchKey") || err_str.contains("NotFound") || err_str.contains("404")
}

/// Pure classifier: map a stringified S3 PUT error to [`UploadError`] —
/// CAS precondition failures become `Conflict`, everything else `Other`.
fn classify_upload_error(err_str: &str) -> UploadError {
    if is_precondition_failed(err_str) {
        UploadError::Conflict
    } else {
        UploadError::Other(err_str.to_string())
    }
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
    admin_password_hash: &str,
    iam_state: &SharedIamState,
    external_auth: &Option<Arc<ExternalAuthManager>>,
    sessions: Option<&Arc<crate::session::SessionStore>>,
    context: &str,
) -> Result<(), UploadError> {
    let mut last_err = UploadError::Other("upload never attempted".to_string());
    for attempt in 1..=MAX_UPLOAD_ATTEMPTS {
        match sync.upload().await {
            Ok(()) => return Ok(()),
            Err(UploadError::Conflict) => {
                last_err = UploadError::Conflict;
                // Pull + merge the peer's version so the retried upload sits
                // on top of the reconciled DB instead of clobbering it.
                // KNOWN LIMIT: IAM tables are replace-merged (last-writer-wins,
                // the pre-existing sync model) — a concurrent peer mutation can
                // revert THIS node's row-level change; only session_revocations
                // merge monotonically. Callers with security-critical intent
                // must re-assert it after this returns (revoke does).
                warn!(
                    "Config DB sync ({context}): CAS conflict — merging peer state before retry; \
                     concurrent IAM row changes resolve last-writer-wins"
                );
                match sync.download_if_newer().await {
                    Ok(Some(dl)) => {
                        let applied = reopen_and_rebuild_iam(
                            config_db,
                            admin_password_hash,
                            iam_state,
                            external_auth,
                            sessions,
                            &dl.temp_path,
                            context,
                        )
                        .await;
                        if applied {
                            sync.commit_downloaded_etag(dl.etag).await;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("Config DB sync ({context}): reconcile download failed: {e}");
                    }
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
    sync.mark_needs_upload();
    warn!("Config DB sync ({context}): upload retries exhausted — queued for next poll tick");
    Err(last_err)
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
    admin_password_hash: &str,
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
    // stay intact (a file swap would clobber them). Then drop the temp file.
    let merge = db.merge_iam_from(downloaded, admin_password_hash);
    let _ = tokio::fs::remove_file(downloaded).await;
    if let Err(e) = merge {
        warn!(
            "Config DB S3 sync ({}): failed to merge IAM after download: {}",
            context, e
        );
        return false;
    }

    // Rebuild IAM index from the new DB
    let users = db.load_users().unwrap_or_default();
    let groups = db.load_groups().unwrap_or_default();
    let count = users.len();
    let group_count = groups.len();
    let state = IamIndex::build_iam_state(users, groups);
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
        if !providers.is_empty() {
            ext_auth.rebuild(&providers);
            drop(db);
            ext_auth.discover_all().await;
            info!(
                "External auth providers rebuilt from S3-synced DB ({} providers) [{}]",
                ext_auth.provider_names().len(),
                context
            );
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn precondition_failed_detected_from_common_shapes() {
        // S3-style service error display.
        assert!(is_precondition_failed(
            "service error: PreconditionFailed: At least one of the pre-conditions you specified did not hold"
        ));
        // MinIO / human-readable status text.
        assert!(is_precondition_failed(
            "unhandled error (Precondition Failed)"
        ));
        // Raw HTTP status code.
        assert!(is_precondition_failed(
            "dispatch failure: response status: 412"
        ));
    }

    #[test]
    fn non_precondition_errors_are_not_misclassified() {
        assert!(!is_precondition_failed(
            "dispatch failure: connection refused"
        ));
        assert!(!is_precondition_failed(
            "NoSuchBucket: bucket does not exist"
        ));
        assert!(!is_precondition_failed(
            "service error: AccessDenied (status 403)"
        ));
        assert!(!is_precondition_failed(""));
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
    }
}
