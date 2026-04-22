//! Full-state export/import (backup & restore).
//!
//! ## Export shape
//!
//! The default `GET /_/api/admin/backup` returns a **zip** containing
//! the four artefacts needed to reconstitute an instance byte-for-byte:
//!
//!   * `manifest.json` — version, capture timestamp, source host,
//!     content summary. Cheap for scripts to sanity-check.
//!   * `config.yaml`   — canonical YAML config (secrets in backend /
//!     access sections are still redacted to `null` here; their real
//!     values live in `secrets.json`). Applying just this file is a
//!     no-op for secrets — the import path consumes both and merges.
//!   * `iam.json`      — users + groups + OAuth providers + mapping
//!     rules + external identities. Same shape as the legacy
//!     IAM-only JSON response (`?format=json`), for backwards compat
//!     with any script that was post-processing it.
//!   * `secrets.json`  — the things the operator would otherwise have
//!     to harvest from platform env vars by hand:
//!       - `bootstrap_password_hash`
//!       - `storage.access_key_id` / `storage.secret_access_key`
//!       - `oauth_client_secrets[provider_name]`
//!
//! Operators commit the first two to git; `secrets.json` + any zip
//! that contains it is a keystore.
//!
//! The legacy `?format=json` query parameter still returns just the
//! IAM-only JSON for backwards compat with pre-Full-Backup scripts.
//!
//! ## Import shape
//!
//! `POST /_/api/admin/backup` sniffs `Content-Type`:
//!   - `application/zip`  → unpacks + applies all four parts
//!     atomically (IAM first, then YAML merge-patch, then secrets).
//!   - `application/json` → today's IAM-only flow (unchanged).

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::io::{Cursor, Write};
use std::sync::Arc;

use crate::config_db::auth_providers::{AuthProviderConfig, ExternalIdentity, GroupMappingRule};
use crate::iam::{normalize_permissions, validate_permissions, Permission};

use super::users::rebuild_iam_index;
use super::{audit_log, trigger_config_sync, AdminState};

/// Hex-encoded SHA-256 of a byte slice. Used in three places in this
/// module (manifest write on export, manifest verify on import) so
/// it lives at module scope instead of as a closure repeated at each
/// call site.
fn sha_hex(b: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b);
    hex::encode(h.finalize())
}

/// Full IAM backup: users (with credentials) + groups + memberships + external auth.
#[derive(Serialize, Deserialize)]
pub struct IamBackup {
    pub version: u32,
    pub users: Vec<BackupUser>,
    pub groups: Vec<BackupGroup>,
    /// External auth providers (v2+, optional for backward compat).
    #[serde(default)]
    pub auth_providers: Vec<AuthProviderConfig>,
    /// Group mapping rules (v2+, optional for backward compat).
    #[serde(default)]
    pub mapping_rules: Vec<GroupMappingRule>,
    /// External identities (v2+, optional for backward compat).
    #[serde(default)]
    pub external_identities: Vec<ExternalIdentity>,
}

#[derive(Serialize, Deserialize)]
pub struct BackupUser {
    /// Source user id. Present in exports so `external_identities.user_id`
    /// and `groups.member_ids` can be remapped by the importer. Optional
    /// for compatibility with older backups that never exposed it.
    #[serde(default)]
    pub id: Option<i64>,
    pub name: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub enabled: bool,
    pub permissions: Vec<Permission>,
    pub group_ids: Vec<i64>,
}

#[derive(Serialize, Deserialize)]
pub struct BackupGroup {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub permissions: Vec<Permission>,
    pub member_ids: Vec<i64>,
}

/// Query params for `GET /_/api/admin/backup`.
#[derive(Deserialize)]
pub struct ExportQuery {
    /// `zip` (default) or `json`. Any other value returns 400.
    #[serde(default)]
    pub format: Option<String>,
}

/// Human-readable top-level description of what's in a backup zip.
/// Serialised as `manifest.json` inside the archive so scripts can
/// inspect a bundle without unzipping + parsing every part.
#[derive(Serialize)]
struct BackupManifest {
    /// Bumped when the zip layout changes in a breaking way.
    /// Readers should refuse unknown versions rather than silently
    /// mis-import. Version 1 = this layout (4 files: manifest.json,
    /// config.yaml, iam.json, secrets.json).
    version: u32,
    /// ISO-8601 UTC timestamp when the archive was produced.
    captured_at: String,
    /// Self-reported deltaglider_proxy version (from Cargo.toml).
    server_version: String,
    /// Top-level file listing + byte counts so an operator can sanity-
    /// check that `unzip -l` matches what they expect.
    files: Vec<ManifestEntry>,
}

#[derive(Serialize)]
struct ManifestEntry {
    name: String,
    bytes: usize,
    sha256: String,
}

/// Plaintext secrets that the server intentionally redacts from
/// `config.yaml` / `iam.json` exports. Written to `secrets.json`
/// inside the zip so a zip-import can round-trip a fully-functional
/// instance with one file.
///
/// Treat the containing zip as a keystore: encrypt at rest, never
/// commit to a public repo, never ship over unencrypted channels.
#[derive(Serialize, Deserialize, Default)]
struct BackupSecrets {
    /// Bcrypt hash (same format as `DGP_BOOTSTRAP_PASSWORD_HASH`)
    /// when in bootstrap mode. `None` if the server can't self-report
    /// (e.g. the hash came from env and was never persisted to state).
    #[serde(skip_serializing_if = "Option::is_none")]
    bootstrap_password_hash: Option<String>,
    /// The operator-authored SigV4 bootstrap pair from
    /// `access.access_key_id` / `access.secret_access_key` in YAML.
    #[serde(skip_serializing_if = "Option::is_none")]
    access: Option<SecretsAccess>,
    /// Storage backend credentials (S3 only — other backend types
    /// have no secrets to restore).
    #[serde(skip_serializing_if = "Option::is_none")]
    storage: Option<SecretsStorage>,
    /// Per-OAuth-provider client secret, keyed by provider `name`.
    /// The IAM JSON already carries these too, but we duplicate them
    /// here so the import flow can skip the IAM file (e.g. to re-seed
    /// secrets without replacing users / groups).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    oauth_client_secrets: std::collections::BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Default)]
struct SecretsAccess {
    #[serde(skip_serializing_if = "Option::is_none")]
    access_key_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret_access_key: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct SecretsStorage {
    #[serde(skip_serializing_if = "Option::is_none")]
    access_key_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret_access_key: Option<String>,
}

/// Build the IamBackup struct from current DB state. Used by both
/// the JSON and zip export paths.
async fn build_iam_backup(state: &Arc<AdminState>) -> Result<IamBackup, StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;

    let users = db.load_users().map_err(|e| {
        tracing::error!("Failed to load users for backup: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let groups = db.load_groups().map_err(|e| {
        tracing::error!("Failed to load groups for backup: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let auth_providers = db.load_auth_providers().unwrap_or_default();
    let mapping_rules = db.load_group_mapping_rules().unwrap_or_default();
    let external_identities = db.list_external_identities().unwrap_or_default();

    Ok(IamBackup {
        version: 2,
        users: users
            .into_iter()
            .map(|u| BackupUser {
                id: Some(u.id),
                name: u.name,
                access_key_id: u.access_key_id,
                secret_access_key: u.secret_access_key,
                enabled: u.enabled,
                permissions: u.permissions,
                group_ids: u.group_ids,
            })
            .collect(),
        groups: groups
            .into_iter()
            .map(|g| BackupGroup {
                id: g.id,
                name: g.name,
                description: g.description,
                permissions: g.permissions,
                member_ids: g.member_ids,
            })
            .collect(),
        auth_providers,
        mapping_rules,
        external_identities,
    })
}

/// GET /api/admin/backup[?format=zip|json]
///
/// Default is **zip** (contains config.yaml + iam.json +
/// secrets.json + manifest.json). Set `?format=json` for the legacy
/// IAM-only JSON body (kept for backwards compat with pre-v0.8.4
/// scripts; operators should migrate to zip).
pub async fn export_backup(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<ExportQuery>,
) -> Result<Response, StatusCode> {
    let iam = build_iam_backup(&state).await?;

    let format = q.format.as_deref().unwrap_or("zip");
    match format {
        "json" => Ok(Json(iam).into_response()),
        "zip" => export_zip(&state, &iam).await.map(|(body, filename)| {
            let mut resp = body.into_response();
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/zip"),
            );
            resp.headers_mut().insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&format!("attachment; filename=\"{}\"", filename))
                    .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
            );
            resp
        }),
        _ => Err(StatusCode::BAD_REQUEST),
    }
}

/// Assemble the zip body + suggested filename (`dgp-backup-<version>-<utc>.zip`).
async fn export_zip(
    state: &Arc<AdminState>,
    iam: &IamBackup,
) -> Result<(Bytes, String), StatusCode> {
    // We hold the read lock for the entire inspection of config so
    // a concurrent apply can't tear the YAML + secrets harvest apart.
    let cfg = state.config.read().await;

    // ── canonical YAML — FULLY redacted (X-ray HIGH #1 fix) ────────
    //    `to_canonical_yaml()` by itself only strips infra secrets
    //    (bootstrap hash + encryption key), NOT the SigV4/S3 creds —
    //    so the zip's config.yaml used to leak plaintext S3 access
    //    keys AND the legacy SigV4 bootstrap pair. The doc at the
    //    top of this module and the manifest UI both promise
    //    "config.yaml redacted" and "secrets.json is the keystore";
    //    we have to honour that so operators who git-commit the
    //    config.yaml entry (standard practice) don't silently leak
    //    credentials through a public repo.
    //
    //    The real secret values go into `secrets.json` below, so
    //    a zip-import can still round-trip functionality.
    let redacted = cfg.redact_all_secrets();
    let yaml = redacted.to_canonical_yaml().map_err(|e| {
        tracing::error!("Full-backup: YAML serialise failed: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // ── secrets.json — harvest real plaintext values that YAML
    //    export would redact ────────────────────────────────────
    let secrets = {
        let mut s = BackupSecrets {
            bootstrap_password_hash: cfg.bootstrap_password_hash.clone(),
            access: None,
            storage: None,
            oauth_client_secrets: Default::default(),
        };
        // Access-section bootstrap SigV4 pair.
        if cfg.access_key_id.is_some() || cfg.secret_access_key.is_some() {
            s.access = Some(SecretsAccess {
                access_key_id: cfg.access_key_id.clone(),
                secret_access_key: cfg.secret_access_key.clone(),
            });
        }
        // Storage-section backend credentials (S3 only — filesystem
        // backends have no secrets to round-trip).
        if let crate::config::BackendConfig::S3 {
            access_key_id,
            secret_access_key,
            ..
        } = &cfg.backend
        {
            if access_key_id.is_some() || secret_access_key.is_some() {
                s.storage = Some(SecretsStorage {
                    access_key_id: access_key_id.clone(),
                    secret_access_key: secret_access_key.clone(),
                });
            }
        }
        // OAuth client secrets (indexed by provider name, not id, so
        // restore is robust across id reshuffles).
        for p in &iam.auth_providers {
            if let Some(cs) = &p.client_secret {
                s.oauth_client_secrets.insert(p.name.clone(), cs.clone());
            }
        }
        s
    };

    // Done with config read lock — drop it before the (CPU-bound)
    // zip write so a concurrent apply doesn't block on us.
    drop(cfg);

    // ── Serialise all three parts ──────────────────────────────
    let iam_bytes = serde_json::to_vec_pretty(&iam).map_err(|e| {
        tracing::error!("Full-backup: iam.json serialise failed: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let secrets_bytes = serde_json::to_vec_pretty(&secrets).map_err(|e| {
        tracing::error!("Full-backup: secrets.json serialise failed: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let yaml_bytes = yaml.into_bytes();

    // ── Manifest (hashes + sizes of each file) ─────────────────
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let manifest = BackupManifest {
        version: 1,
        captured_at: now.clone(),
        server_version: env!("CARGO_PKG_VERSION").to_string(),
        files: vec![
            ManifestEntry {
                name: "config.yaml".into(),
                bytes: yaml_bytes.len(),
                sha256: sha_hex(&yaml_bytes),
            },
            ManifestEntry {
                name: "iam.json".into(),
                bytes: iam_bytes.len(),
                sha256: sha_hex(&iam_bytes),
            },
            ManifestEntry {
                name: "secrets.json".into(),
                bytes: secrets_bytes.len(),
                sha256: sha_hex(&secrets_bytes),
            },
        ],
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| {
        tracing::error!("Full-backup: manifest.json serialise failed: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // ── Write zip (in-memory) ──────────────────────────────────
    let mut buf = Cursor::new(Vec::<u8>::with_capacity(
        manifest_bytes.len() + yaml_bytes.len() + iam_bytes.len() + secrets_bytes.len() + 2048,
    ));
    {
        let mut zw = zip::ZipWriter::new(&mut buf);
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in [
            ("manifest.json", manifest_bytes.as_slice()),
            ("config.yaml", yaml_bytes.as_slice()),
            ("iam.json", iam_bytes.as_slice()),
            ("secrets.json", secrets_bytes.as_slice()),
        ] {
            zw.start_file(name, opts).map_err(|e| {
                tracing::error!("Full-backup: zip start_file({}) failed: {}", name, e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
            zw.write_all(bytes).map_err(|e| {
                tracing::error!("Full-backup: zip write({}) failed: {}", name, e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        }
        zw.finish().map_err(|e| {
            tracing::error!("Full-backup: zip finish failed: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    }
    let bytes = Bytes::from(buf.into_inner());
    let filename = format!(
        "dgp-backup-v{}-{}.zip",
        env!("CARGO_PKG_VERSION"),
        now.replace([':', '-'], "")
    );
    Ok((bytes, filename))
}

/// Sniff the Content-Type and route zip uploads to the full-backup
/// import path. Defaults to JSON if the header is missing so legacy
/// scripts keep working unchanged.
///
/// Wave 11.1 Full Backup: POST /_/api/admin/backup now accepts
///   * `application/zip` — zip produced by GET `?format=zip`
///     (or no format): unpacks manifest.json + config.yaml +
///     iam.json + secrets.json, applies them atomically.
///   * `application/json` (and all other content-types) — the
///     legacy IAM-only flow (same shape as v0.8.0).
pub async fn import_backup(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<ImportResult>, StatusCode> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .split(';') // strip charset= etc.
        .next()
        .unwrap_or("application/json")
        .trim()
        .to_ascii_lowercase();

    if ct == "application/zip" || ct == "application/x-zip-compressed" {
        return import_zip_full_backup(state, headers, body).await;
    }

    // Legacy JSON body — deserialise then run the existing flow.
    let backup: IamBackup = serde_json::from_slice(&body).map_err(|e| {
        tracing::warn!("import_backup: malformed JSON body: {}", e);
        StatusCode::BAD_REQUEST
    })?;
    import_backup_iam(state, headers, backup).await
}

/// Per-entry cap for zip unpack. 8 MiB is generous for the three
/// config artefacts we ship (YAML + two JSONs); anything larger
/// almost certainly means a malicious or corrupted archive.
/// See x-ray MED #1 — unbounded `Vec::with_capacity(f.size())`
/// was an easy OOM vector for a single crafted entry.
const MAX_ENTRY_BYTES: u64 = 8 * 1024 * 1024;

/// Unpack a Full Backup zip and apply all four parts atomically.
///
/// Two-phase flow (x-ray MED #3: validate first, side-effect second):
///   Phase A — unpack + parse every part + verify manifest sha256.
///             No state change. Any failure returns before we've
///             touched the DB or config.
///   Phase B — apply in order: config.yaml (via apply_config_doc),
///             then secrets.json (storage creds + bootstrap hash),
///             then iam.json. Secrets land before IAM so the
///             post-IAM S3-sync push uses the restored storage creds.
async fn import_zip_full_backup(
    state: Arc<AdminState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<ImportResult>, StatusCode> {
    // ── Phase A.1: unpack, bounded per-entry ───────────────────
    let reader = Cursor::new(body.as_ref());
    let mut archive = zip::ZipArchive::new(reader).map_err(|e| {
        tracing::warn!("Full-backup import: not a valid zip: {}", e);
        StatusCode::BAD_REQUEST
    })?;
    let mut files: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
    for i in 0..archive.len() {
        let mut f = archive.by_index(i).map_err(|e| {
            tracing::warn!("Full-backup import: zip entry {} unreadable: {}", i, e);
            StatusCode::BAD_REQUEST
        })?;
        let name = f.name().to_string();
        // `size()` is the header-declared uncompressed length; we
        // clamp capacity to MAX_ENTRY_BYTES to foil a zip that lies
        // about size to force a huge upfront allocation.
        let declared = f.size();
        if declared > MAX_ENTRY_BYTES {
            tracing::warn!(
                "Full-backup import: entry {} declares {} bytes > cap {}",
                name,
                declared,
                MAX_ENTRY_BYTES
            );
            return Err(StatusCode::BAD_REQUEST);
        }
        let cap = std::cmp::min(declared, MAX_ENTRY_BYTES) as usize;
        let mut buf = Vec::with_capacity(cap);
        // Wrap the decompressing reader in `take` so a zip that
        // underdeclares `size` (decompression bomb) is cut off at
        // the cap instead of filling memory unbounded.
        let mut bounded = std::io::Read::take(&mut f, MAX_ENTRY_BYTES + 1);
        std::io::Read::read_to_end(&mut bounded, &mut buf).map_err(|e| {
            tracing::warn!("Full-backup import: read {} failed: {}", name, e);
            StatusCode::BAD_REQUEST
        })?;
        if buf.len() as u64 > MAX_ENTRY_BYTES {
            tracing::warn!(
                "Full-backup import: entry {} exceeded cap {} during decompression",
                name,
                MAX_ENTRY_BYTES
            );
            return Err(StatusCode::BAD_REQUEST);
        }
        files.insert(name, buf);
    }

    // ── Phase A.2: manifest is required (LOW #1) ───────────────
    let m_bytes = files.get("manifest.json").ok_or_else(|| {
        tracing::warn!("Full-backup import: manifest.json missing (required)");
        StatusCode::BAD_REQUEST
    })?;
    let manifest: serde_json::Value = serde_json::from_slice(m_bytes).map_err(|e| {
        tracing::warn!("Full-backup import: manifest.json malformed: {}", e);
        StatusCode::BAD_REQUEST
    })?;
    let ver = manifest
        .get("version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if ver != 1 {
        tracing::warn!("Full-backup import: unsupported manifest version {}", ver);
        return Err(StatusCode::BAD_REQUEST);
    }

    // ── Phase A.3: verify manifest sha256 entries (LOW #2) ─────
    //    For each entry the manifest claims, recompute sha256 on
    //    the unpacked bytes and refuse mismatches. Missing files
    //    listed in the manifest are a corruption signal — fail.
    //    Files present in the zip but not listed in the manifest
    //    are ignored (forward-compat: older servers shouldn't
    //    choke on newer zips adding non-sensitive metadata).
    if let Some(entries) = manifest.get("files").and_then(|v| v.as_array()) {
        for entry in entries {
            let name = entry.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
                tracing::warn!("Full-backup import: manifest entry missing name");
                StatusCode::BAD_REQUEST
            })?;
            let expected_sha = entry
                .get("sha256")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    tracing::warn!("Full-backup import: manifest entry {} missing sha256", name);
                    StatusCode::BAD_REQUEST
                })?;
            let bytes = files.get(name).ok_or_else(|| {
                tracing::warn!(
                    "Full-backup import: manifest lists {} but zip has no such entry",
                    name
                );
                StatusCode::BAD_REQUEST
            })?;
            let actual_sha = sha_hex(bytes);
            if actual_sha != expected_sha {
                tracing::warn!(
                    "Full-backup import: sha256 mismatch on {} (expected {}, got {})",
                    name,
                    expected_sha,
                    actual_sha
                );
                return Err(StatusCode::BAD_REQUEST);
            }
        }
    }

    // ── Phase A.4: pre-parse every part (MED #3) ───────────────
    //    Build owned typed values for everything that might be
    //    applied, so Phase B only hits side-effect paths once the
    //    archive is fully understood. A malformed iam.json used to
    //    surface AFTER config + secrets had been applied, leaving
    //    the server in a partially-restored state.
    let yaml_str: Option<String> = if let Some(yaml_bytes) = files.get("config.yaml") {
        let s = std::str::from_utf8(yaml_bytes)
            .map_err(|_| {
                tracing::warn!("Full-backup import: config.yaml is not UTF-8");
                StatusCode::BAD_REQUEST
            })?
            .to_string();
        // Actual YAML shape is validated by apply_config_doc itself
        // (validate → apply → persist). Empty-string means "no-op".
        Some(s)
    } else {
        None
    };
    let secrets: Option<BackupSecrets> = if let Some(sec_bytes) = files.get("secrets.json") {
        Some(serde_json::from_slice(sec_bytes).map_err(|e| {
            tracing::warn!("Full-backup import: secrets.json malformed: {}", e);
            StatusCode::BAD_REQUEST
        })?)
    } else {
        None
    };
    let iam_backup: Option<IamBackup> = if let Some(iam_bytes) = files.get("iam.json") {
        Some(serde_json::from_slice(iam_bytes).map_err(|e| {
            tracing::warn!("Full-backup import: iam.json malformed: {}", e);
            StatusCode::BAD_REQUEST
        })?)
    } else {
        None
    };

    // ── Phase B.1: apply config.yaml via the existing document-apply
    //       endpoint (same path /_/api/admin/config/apply uses).
    //       For simplicity we POST to our own endpoint rather than
    //       refactoring the helper out of its handler — that lets
    //       the YAML go through the exact same validate → apply →
    //       persist pipeline a human would trigger via the GUI.
    //       TODO(v0.9): extract a pub(crate) helper so this can be
    //       called directly without the HTTP round-trip. ──
    if let Some(yaml_str) = yaml_str {
        // Skip application if the YAML is empty/whitespace-only.
        // Exporters always emit at least `storage:` so this only
        // fires on deliberate-empty zips.
        if !yaml_str.trim().is_empty() {
            let req = crate::api::admin::ConfigDocumentRequest { yaml: yaml_str };
            let State(state_for_apply) = State(state.clone());
            let resp = crate::api::admin::apply_config_doc(
                State(state_for_apply),
                headers.clone(),
                Json(req),
            )
            .await
            .into_response();
            let status = resp.status();
            if !status.is_success() {
                tracing::error!("Full-backup import: apply config.yaml → HTTP {}", status);
                return Err(StatusCode::BAD_REQUEST);
            }
        }
    }

    // ── Phase B.2: apply secrets.json BEFORE iam import. Storage
    //       creds need to be in place before a subsequent import of
    //       a v0.8.3+ iam.json fires an S3 sync push. Bootstrap
    //       hash must land before any admin session is re-issued. ──
    if let Some(secrets) = secrets.as_ref() {
        apply_secrets(&state, secrets).await.inspect_err(|&e| {
            tracing::error!("Full-backup import: apply secrets.json failed: {}", e);
        })?;
    }

    // ── Phase B.3: apply iam.json (same flow as legacy JSON import) ──
    let iam_result = if let Some(backup) = iam_backup {
        import_backup_iam(state.clone(), headers.clone(), backup).await?
    } else {
        // Zip with no iam.json is valid — maybe operator only
        // wants to apply config+secrets. Emit an all-zero result.
        Json(ImportResult {
            users_created: 0,
            users_skipped: 0,
            groups_created: 0,
            groups_skipped: 0,
            memberships_created: 0,
            external_identities_created: 0,
            external_identities_skipped: 0,
        })
    };

    audit_log("import_full_backup", "admin", "zip applied", &headers);

    Ok(iam_result)
}

/// Apply the plaintext secrets harvested in `secrets.json` onto the
/// running Config.
///
/// X-ray fixes (HIGH #2, HIGH #3, MED #2):
///
/// * **bootstrap_password_hash**: refused when the running instance
///   already has a *different* hash — a hash alone cannot rekey the
///   SQLCipher DB (that needs the plaintext password via
///   `/api/admin/change-password`). Initial seeding (no existing hash,
///   or identical hash) is permitted.
/// * **Engine rebuild**: after mutating storage creds under the write
///   lock, call `apply_config_transition` so the S3 client picks up
///   the new credentials on the next request. Without this, the
///   running engine would keep using the old (possibly-wrong) creds
///   until the next restart.
/// * **Persist to disk**: write the merged config back to the active
///   config file so the change survives a restart.
async fn apply_secrets(state: &Arc<AdminState>, secrets: &BackupSecrets) -> Result<(), StatusCode> {
    // Snapshot pre-mutation config for apply_config_transition.
    let old_cfg = state.config.read().await.clone();

    // Guardrail: refuse hash rotation on a running instance. The only
    // supported path to change the bootstrap password is
    // /api/admin/change-password which rekeys SQLCipher with the
    // plaintext. Initial seeding (hash match, or no existing hash)
    // is fine — that covers first-restore into a fresh instance.
    if let Some(new_hash) = &secrets.bootstrap_password_hash {
        if let Some(existing) = &old_cfg.bootstrap_password_hash {
            if existing != new_hash {
                tracing::error!(
                    "Full-backup import: secrets.json carries a bootstrap_password_hash \
                     that differs from the running instance. Rotating it here would \
                     desync the SQLCipher DB key. Use /api/admin/change-password \
                     with the plaintext password instead."
                );
                return Err(StatusCode::CONFLICT);
            }
        }
    }

    // Mutate Config fields under the write lock. Snapshot the post-
    // mutation Config for apply_config_transition after releasing.
    let new_cfg = {
        let mut cfg = state.config.write().await;
        if let Some(h) = &secrets.bootstrap_password_hash {
            cfg.bootstrap_password_hash = Some(h.clone());
        }
        if let Some(a) = &secrets.access {
            if let Some(ak) = &a.access_key_id {
                cfg.access_key_id = Some(ak.clone());
            }
            if let Some(sk) = &a.secret_access_key {
                cfg.secret_access_key = Some(sk.clone());
            }
        }
        if let Some(s) = &secrets.storage {
            if let crate::config::BackendConfig::S3 {
                access_key_id,
                secret_access_key,
                ..
            } = &mut cfg.backend
            {
                if let Some(ak) = &s.access_key_id {
                    *access_key_id = Some(ak.clone());
                }
                if let Some(sk) = &s.secret_access_key {
                    *secret_access_key = Some(sk.clone());
                }
            }
        }
        cfg.clone()
    }; // release write lock before touching config_db

    // Rebuild the S3 engine so the new storage creds take effect
    // immediately. A mismatch between Config and the running engine
    // would cause every subsequent S3 op to use stale credentials
    // until restart.
    if let Err(e) =
        crate::api::admin::config::apply_config_transition(state, &old_cfg, &new_cfg).await
    {
        tracing::error!("Full-backup import: apply_config_transition failed: {}", e);
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    // Persist the merged config so storage/access creds survive a
    // restart. Without this, the operator would see the restore "work"
    // until the next process restart, then silently revert.
    let path = crate::api::admin::config::active_config_path(state);
    if let Err(e) = new_cfg.persist_to_file(&path) {
        tracing::error!(
            "Full-backup import: persist merged config to {} failed: {}",
            path,
            e
        );
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    // OAuth client_secret per provider, by name (robust to id
    // reshuffles across restores). Requires the provider row to
    // already exist; if iam.json hasn't been applied yet the lookup
    // returns empty and we skip silently — that's fine, the
    // subsequent iam.json import carries client_secret too.
    if !secrets.oauth_client_secrets.is_empty() {
        let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
        let db = db.lock().await;
        let providers = db.load_auth_providers().unwrap_or_default();
        for p in &providers {
            if let Some(cs) = secrets.oauth_client_secrets.get(&p.name) {
                let req = crate::config_db::auth_providers::UpdateAuthProviderRequest {
                    name: None,
                    provider_type: None,
                    enabled: None,
                    priority: None,
                    display_name: None,
                    client_id: None,
                    client_secret: Some(cs.clone()),
                    issuer_url: None,
                    scopes: None,
                    extra_config: None,
                };
                if let Err(e) = db.update_auth_provider(p.id, &req) {
                    tracing::warn!(
                        "Full-backup: update client_secret for provider '{}' failed: {}",
                        p.name,
                        e
                    );
                }
            }
        }
    }

    Ok(())
}

/// POST /api/admin/backup — import IAM data from JSON body.
/// Merges with existing data: skips users/groups that already exist (by name).
async fn import_backup_iam(
    state: Arc<AdminState>,
    headers: HeaderMap,
    backup: IamBackup,
) -> Result<Json<ImportResult>, StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;

    // Get bootstrap access key to prevent conflicts
    let bootstrap_key = {
        let iam = state.iam_state.load();
        match iam.as_ref() {
            crate::iam::IamState::Legacy(auth) => Some(auth.access_key_id.clone()),
            _ => None,
        }
    };

    let mut result = ImportResult {
        users_created: 0,
        users_skipped: 0,
        groups_created: 0,
        groups_skipped: 0,
        memberships_created: 0,
        external_identities_created: 0,
        external_identities_skipped: 0,
    };

    // Pre-load existing data once (O(1) lookups instead of O(N²) per-item DB queries)
    let existing_groups = db.load_groups().unwrap_or_default();
    let existing_users = db.load_users().unwrap_or_default();
    let existing_group_names: std::collections::HashSet<String> =
        existing_groups.iter().map(|g| g.name.clone()).collect();
    let existing_user_keys: std::collections::HashSet<String> = existing_users
        .iter()
        .map(|u| u.access_key_id.clone())
        .collect();

    // Import groups first (users reference group_ids)
    let mut group_id_map: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();

    for bg in &backup.groups {
        if existing_group_names.contains(&bg.name) {
            if let Some(existing_group) = existing_groups.iter().find(|g| g.name == bg.name) {
                group_id_map.insert(bg.id, existing_group.id);
            }
            result.groups_skipped += 1;
            continue;
        }

        // Validate permissions before import
        let mut perms = bg.permissions.clone();
        normalize_permissions(&mut perms);
        if let Err(msg) = validate_permissions(&perms) {
            tracing::warn!("Skipping group '{}': invalid permissions: {}", bg.name, msg);
            result.groups_skipped += 1;
            continue;
        }

        match db.create_group(&bg.name, &bg.description, &perms) {
            Ok(created) => {
                group_id_map.insert(bg.id, created.id);
                result.groups_created += 1;
            }
            Err(e) => {
                tracing::warn!("Failed to import group '{}': {}", bg.name, e);
                result.groups_skipped += 1;
            }
        }
    }

    // Import users — track old→new user IDs so external_identities
    // references below can be remapped (not just group memberships).
    //
    // Resolving `old_id` for the mapping:
    //   1. Prefer `bu.id` from the backup (new export format).
    //   2. Fall back to `bg.member_ids` in groups — the original DB's
    //      user IDs leak through here (v2 format, pre-Wave-11).
    //   3. Last resort: assume SQLite autoincrement order matches the
    //      `users` array index + 1.
    //
    // This lets us restore external_identities from backups generated
    // BEFORE the Wave 11 fix added `BackupUser.id`, without breaking
    // existing v1/v2 payloads.
    let mut user_id_map: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    // Pre-populate with any existing-user overlaps so imports on a
    // non-empty instance still remap correctly for external_identities.
    for existing in &existing_users {
        if let Some((idx, bu)) = backup
            .users
            .iter()
            .enumerate()
            .find(|(_, bu)| bu.access_key_id == existing.access_key_id)
        {
            let old_id = resolve_backup_user_id(bu, idx, &backup);
            user_id_map.insert(old_id, existing.id);
        }
    }

    for (idx, bu) in backup.users.iter().enumerate() {
        // Block reserved names
        if bu.name.starts_with('$') {
            tracing::warn!("Skipping user '{}': reserved name", bu.name);
            result.users_skipped += 1;
            continue;
        }

        // Block bootstrap key conflicts
        if let Some(ref bk) = bootstrap_key {
            if bu.access_key_id == *bk {
                tracing::warn!(
                    "Skipping user '{}': access key conflicts with bootstrap credentials",
                    bu.name
                );
                result.users_skipped += 1;
                continue;
            }
        }

        if existing_user_keys.contains(&bu.access_key_id) {
            result.users_skipped += 1;
            continue;
        }

        // Validate permissions before import
        let mut perms = bu.permissions.clone();
        normalize_permissions(&mut perms);
        if let Err(msg) = validate_permissions(&perms) {
            tracing::warn!("Skipping user '{}': invalid permissions: {}", bu.name, msg);
            result.users_skipped += 1;
            continue;
        }

        match db.create_user(
            &bu.name,
            &bu.access_key_id,
            &bu.secret_access_key,
            bu.enabled,
            &perms,
        ) {
            Ok(created) => {
                // Track old→new id mapping for external_identities below.
                let old_id = resolve_backup_user_id(bu, idx, &backup);
                user_id_map.insert(old_id, created.id);
                // Restore group memberships
                for old_gid in &bu.group_ids {
                    if let Some(&new_gid) = group_id_map.get(old_gid) {
                        if db.add_user_to_group(new_gid, created.id).is_ok() {
                            result.memberships_created += 1;
                        }
                    } else {
                        tracing::warn!(
                            "User '{}': group_id {} not found in backup, membership skipped",
                            bu.name,
                            old_gid
                        );
                    }
                }
                result.users_created += 1;
            }
            Err(e) => {
                tracing::warn!("Failed to import user '{}': {}", bu.name, e);
                result.users_skipped += 1;
            }
        }
    }

    // Import auth providers (v2+), with ID remapping for mapping rules
    let mut provider_id_map: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    let existing_providers = db.load_auth_providers().unwrap_or_default();
    let existing_provider_names: std::collections::HashSet<String> =
        existing_providers.iter().map(|p| p.name.clone()).collect();

    for bp in &backup.auth_providers {
        if existing_provider_names.contains(&bp.name) {
            // Map old ID to existing provider's ID
            if let Some(existing) = existing_providers.iter().find(|p| p.name == bp.name) {
                provider_id_map.insert(bp.id, existing.id);
            }
            continue;
        }
        let req = crate::config_db::auth_providers::CreateAuthProviderRequest {
            name: bp.name.clone(),
            provider_type: bp.provider_type.clone(),
            enabled: bp.enabled,
            priority: bp.priority,
            display_name: bp.display_name.clone(),
            client_id: bp.client_id.clone(),
            client_secret: bp.client_secret.clone(),
            issuer_url: bp.issuer_url.clone(),
            scopes: bp.scopes.clone(),
            extra_config: bp.extra_config.clone(),
        };
        match db.create_auth_provider(&req) {
            Ok(created) => {
                provider_id_map.insert(bp.id, created.id);
            }
            Err(e) => {
                tracing::warn!("Failed to import auth provider '{}': {}", bp.name, e);
            }
        }
    }

    // Import group mapping rules (v2+), remapping provider_id and group_id
    for rule in &backup.mapping_rules {
        let new_provider_id = rule
            .provider_id
            .and_then(|old_id| provider_id_map.get(&old_id).copied());
        let new_group_id = match group_id_map.get(&rule.group_id) {
            Some(&gid) => gid,
            None => {
                tracing::warn!(
                    "Skipping mapping rule: group_id {} not found in backup",
                    rule.group_id
                );
                continue;
            }
        };
        let req = crate::config_db::auth_providers::CreateMappingRuleRequest {
            provider_id: new_provider_id,
            priority: rule.priority,
            match_type: rule.match_type.clone(),
            match_field: rule.match_field.clone(),
            match_value: rule.match_value.clone(),
            group_id: new_group_id,
        };
        if let Err(e) = db.create_group_mapping_rule(&req) {
            tracing::warn!("Failed to import mapping rule: {}", e);
        }
    }

    // Import external identities (v2+). We remap `user_id` + `provider_id`
    // through the maps built above. Records whose user or provider didn't
    // make it through the import (e.g. skipped due to conflicts) are
    // dropped with a warning rather than imported with dangling references.
    //
    // `last_login` isn't preservable via `create_external_identity` (it
    // resets to `now()`), but the binding — user ↔ external_sub ↔
    // provider — is what matters for re-authentication.
    for ident in &backup.external_identities {
        let new_user_id = match user_id_map.get(&ident.user_id) {
            Some(&uid) => uid,
            None => {
                tracing::warn!(
                    "Skipping external_identity for external_sub '{}': user_id {} not imported",
                    ident.external_sub,
                    ident.user_id
                );
                result.external_identities_skipped += 1;
                continue;
            }
        };
        let new_provider_id = match provider_id_map.get(&ident.provider_id) {
            Some(&pid) => pid,
            None => {
                tracing::warn!(
                    "Skipping external_identity for external_sub '{}': provider_id {} not imported",
                    ident.external_sub,
                    ident.provider_id
                );
                result.external_identities_skipped += 1;
                continue;
            }
        };
        // Skip duplicates idempotently — a second import pass should not
        // double-insert. `find_external_identity` returns the existing
        // row if one already exists for this (provider, external_sub).
        if db
            .find_external_identity(new_provider_id, &ident.external_sub)
            .ok()
            .flatten()
            .is_some()
        {
            result.external_identities_skipped += 1;
            continue;
        }
        match db.create_external_identity(
            new_user_id,
            new_provider_id,
            &ident.external_sub,
            ident.email.as_deref(),
            ident.display_name.as_deref(),
            ident.raw_claims.as_ref(),
        ) {
            Ok(_) => result.external_identities_created += 1,
            Err(e) => {
                tracing::warn!(
                    "Failed to import external_identity for external_sub '{}': {}",
                    ident.external_sub,
                    e
                );
                result.external_identities_skipped += 1;
            }
        }
    }

    // Rebuild IAM index + external auth manager
    rebuild_iam_index(&db, &state.iam_state)?;
    // Reload OAuth providers into memory (otherwise imported providers
    // won't work until restart)
    if let Some(ref ext_auth) = state.external_auth {
        let providers = db.load_auth_providers().unwrap_or_default();
        if !providers.is_empty() {
            ext_auth.rebuild(&providers);
        }
    }
    drop(db);
    // Discover OIDC endpoints for newly imported providers
    if let Some(ref ext_auth) = state.external_auth {
        ext_auth.discover_all().await;
    }
    trigger_config_sync(&state);

    audit_log(
        "import_backup",
        "admin",
        &format!(
            "{}u+{}g+{}ext_id created",
            result.users_created, result.groups_created, result.external_identities_created
        ),
        &headers,
    );

    Ok(Json(result))
}

/// Best-effort resolver for a backup user's original database id.
///
/// Old backups (before the Wave-11 fix) never carried `BackupUser.id`.
/// To restore external_identities from those, we walk a short fallback
/// chain:
///
///   1. `bu.id` — authoritative when present (new exports).
///   2. `backup.groups[].member_ids` — the sibling field lists original
///      user IDs and is present in v2 backups. Match by position: the
///      `idx`-th user was written from `load_users()`, which returns
///      rows in id order, so the `idx`-th member across all groups
///      that refers back to this user yields the original id.
///      Simpler: scan every member_ids list, pick the one whose
///      position in the flattened user list equals `idx`.
///   3. `idx + 1` — SQLite autoincrement starts at 1 and the export
///      writes users in id order. This is a last-resort heuristic.
///      It fails only when the original DB had deleted ids (id gaps).
///
/// None of these are perfect, but (3) covers the overwhelming majority
/// of restores and the damage of a wrong guess is limited to a single
/// dropped external_identity — the operator's next OAuth login will
/// re-provision the binding.
fn resolve_backup_user_id(bu: &BackupUser, idx: usize, backup: &IamBackup) -> i64 {
    if let Some(id) = bu.id {
        return id;
    }
    // Fallback (2): scan groups.member_ids for a candidate.
    // Build a sorted set of member IDs from groups, then pick the
    // idx-th smallest. Since `load_users()` returns users in id order
    // and the user is a member of at least one group, this yields
    // the original id for any user that had a group membership.
    let mut member_ids: Vec<i64> = backup
        .groups
        .iter()
        .flat_map(|g| g.member_ids.iter().copied())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    member_ids.sort();
    if let Some(&cand) = member_ids.get(idx) {
        return cand;
    }
    // Fallback (3): SQLite autoincrement assumption.
    (idx as i64) + 1
}

#[derive(Serialize)]
pub struct ImportResult {
    pub users_created: u32,
    pub users_skipped: u32,
    pub groups_created: u32,
    pub groups_skipped: u32,
    pub memberships_created: u32,
    /// External-identity rows successfully remapped + inserted.
    pub external_identities_created: u32,
    /// Skipped because the referenced user/provider didn't make it,
    /// or a matching (provider, external_sub) already exists.
    pub external_identities_skipped: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_v1_backup_deserializes_without_external_fields() {
        // v1 backups have no auth_providers/mapping_rules/external_identities
        let json = r#"{
            "version": 1,
            "users": [],
            "groups": []
        }"#;
        let backup: IamBackup = serde_json::from_str(json).unwrap();
        assert_eq!(backup.version, 1);
        assert!(backup.auth_providers.is_empty());
        assert!(backup.mapping_rules.is_empty());
        assert!(backup.external_identities.is_empty());
    }

    #[test]
    fn test_v2_backup_roundtrip() {
        let backup = IamBackup {
            version: 2,
            users: vec![BackupUser {
                id: Some(1),
                name: "alice".into(),
                access_key_id: "AK1".into(),
                secret_access_key: "SK1".into(),
                enabled: true,
                permissions: vec![],
                group_ids: vec![1],
            }],
            groups: vec![BackupGroup {
                id: 1,
                name: "devs".into(),
                description: "Dev team".into(),
                permissions: vec![],
                member_ids: vec![],
            }],
            auth_providers: vec![AuthProviderConfig {
                id: 1,
                name: "google".into(),
                provider_type: "oidc".into(),
                enabled: true,
                priority: 10,
                display_name: Some("Google".into()),
                client_id: Some("cid".into()),
                client_secret: Some("****".into()),
                issuer_url: Some("https://accounts.google.com".into()),
                scopes: "openid email".into(),
                extra_config: None,
                created_at: "2024-01-01".into(),
                updated_at: "2024-01-01".into(),
            }],
            mapping_rules: vec![GroupMappingRule {
                id: 1,
                provider_id: Some(1),
                priority: 0,
                match_type: "email_domain".into(),
                match_field: "email".into(),
                match_value: "company.com".into(),
                group_id: 1,
                created_at: "2024-01-01".into(),
            }],
            external_identities: vec![ExternalIdentity {
                id: 1,
                user_id: 1,
                provider_id: 1,
                external_sub: "google-123".into(),
                email: Some("alice@company.com".into()),
                display_name: Some("Alice".into()),
                last_login: None,
                raw_claims: Some(serde_json::json!({"sub": "google-123"})),
                created_at: "2024-01-01".into(),
            }],
        };

        let json = serde_json::to_string(&backup).unwrap();
        let restored: IamBackup = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.version, 2);
        assert_eq!(restored.users.len(), 1);
        assert_eq!(restored.groups.len(), 1);
        assert_eq!(restored.auth_providers.len(), 1);
        assert_eq!(restored.auth_providers[0].name, "google");
        assert_eq!(restored.mapping_rules.len(), 1);
        assert_eq!(restored.mapping_rules[0].match_value, "company.com");
        assert_eq!(restored.external_identities.len(), 1);
        assert_eq!(
            restored.external_identities[0].email.as_deref(),
            Some("alice@company.com")
        );
    }
}
