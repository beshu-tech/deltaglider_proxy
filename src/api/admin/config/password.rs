//! Password-change and config-database recovery handlers.
//!
//! These two flows share a responsibility — custody of the bootstrap
//! password hash, which is both the admin-GUI session key and the
//! SQLCipher cipher for the IAM database. They are split out of the
//! larger config module because their logic is security-critical,
//! orthogonal to the rest of the config surface, and benefits from
//! being reviewed as a single unit.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::super::{audit_log, trigger_config_sync, validate_password, AdminState};

#[derive(Deserialize)]
pub struct PasswordChangeRequest {
    current_password: String,
    new_password: String,
}

#[derive(Serialize)]
pub struct PasswordChangeResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Build a `PasswordChangeResponse` error response in one line.
fn password_err(status: StatusCode, msg: impl Into<String>) -> axum::response::Response {
    (
        status,
        Json(PasswordChangeResponse {
            ok: false,
            error: Some(msg.into()),
        }),
    )
        .into_response()
}

/// PUT /api/admin/password — change bootstrap password.
///
/// Ordering invariants (do NOT reorder without understanding recovery):
/// 1. Verify the current password before doing anything else.
/// 2. Re-encrypt the SQLCipher IAM database with the new hash. If this
///    fails, bail out WITHOUT touching the hash file or in-memory state —
///    the DB rekey is the only operation that can leave the system
///    unrecoverable if it partially succeeds.
/// 3. Persist the hash to disk. If the write fails, revert the DB
///    encryption to the old hash so startup still matches the on-disk
///    state file on next restart.
/// 4. Only after disk-persist succeeds do we swap the in-memory hash
///    and update `cfg.bootstrap_password_hash`.
pub async fn change_password(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Json(body): Json<PasswordChangeRequest>,
) -> impl IntoResponse {
    let current_hash = state.password_hash.read().clone();
    let valid = match bcrypt::verify(&body.current_password, &current_hash) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("bcrypt verify failed (corrupted hash?): {}", e);
            return password_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Password hash is corrupted. Delete .deltaglider_bootstrap_hash and restart.",
            );
        }
    };

    if !valid {
        return password_err(StatusCode::FORBIDDEN, "Current password is incorrect");
    }

    // Validate new password quality
    if let Err(msg) = validate_password(&body.new_password) {
        return password_err(StatusCode::BAD_REQUEST, msg.to_string());
    }

    let new_hash = match bcrypt::hash(&body.new_password, bcrypt::DEFAULT_COST) {
        Ok(h) => h,
        Err(e) => {
            return password_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Hashing failed: {}", e),
            );
        }
    };

    // Re-encrypt the IAM config database with the new password hash FIRST.
    // If this fails, we must NOT update the in-memory hash or persist — the DB
    // would become out of sync and the next restart would fail to open it.
    if let Some(ref db_mutex) = state.config_db {
        let db = db_mutex.lock().await;
        if let Err(e) = db.rekey(&new_hash) {
            tracing::error!(
                "Failed to re-encrypt config DB after password change: {}",
                e
            );
            return password_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to re-encrypt config database: {}", e),
            );
        }
        tracing::info!("Config DB re-encrypted with new bootstrap password hash");
        // Upload re-encrypted DB to S3
        trigger_config_sync(&state);
    }

    // Persist to state file BEFORE updating in-memory state — if this fails,
    // the DB was already re-keyed with new_hash but the file still has old_hash.
    // We must revert the DB encryption to avoid a mismatch on restart.
    let state_file = std::path::Path::new(".deltaglider_bootstrap_hash");
    if let Err(e) = crate::config::write_bootstrap_hash_file(state_file, &new_hash) {
        tracing::error!("Failed to persist new admin hash to disk: {}", e);
        // Revert DB encryption to match the old hash file
        if let Some(ref db_mutex) = state.config_db {
            let db = db_mutex.lock().await;
            if let Err(revert_err) = db.rekey(&current_hash) {
                tracing::error!(
                    "CRITICAL: Failed to revert DB encryption after hash file write failure: {}. \
                     The config DB may be inaccessible on next restart. \
                     Use --set-bootstrap-password or the recover-db endpoint.",
                    revert_err
                );
            } else {
                tracing::info!(
                    "DB encryption reverted to previous password after file write failure"
                );
            }
        }
        return password_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "Failed to persist hash file ({}). Password change aborted and reverted.",
                e
            ),
        );
    }

    // File written successfully — now update in-memory state
    *state.password_hash.write() = new_hash.clone();

    // Also update config
    {
        let mut cfg = state.config.write().await;
        cfg.bootstrap_password_hash = Some(new_hash);
    }

    audit_log("change_password", "bootstrap", "", &headers);

    (
        StatusCode::OK,
        Json(PasswordChangeResponse {
            ok: true,
            error: None,
        }),
    )
        .into_response()
}

// ============================================================================
// Config DB recovery
// ============================================================================

#[derive(Deserialize)]
pub struct RecoverDbRequest {
    candidate_password: String,
}

#[derive(Serialize)]
pub struct RecoverDbResponse {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    correct_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    correct_hash_base64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// POST /api/admin/recover-db — try a candidate password against the locked config DB.
///
/// Only available when `config_db_mismatch` is true. Returns the correct bcrypt
/// hash (and base64 version) if the candidate password successfully decrypts the DB.
pub async fn recover_db(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Json(body): Json<RecoverDbRequest>,
) -> impl IntoResponse {
    if !state.config_db_mismatch {
        return (
            StatusCode::NOT_FOUND,
            Json(RecoverDbResponse {
                success: false,
                correct_hash: None,
                correct_hash_base64: None,
                error: Some("No config DB mismatch detected".into()),
            }),
        );
    }

    // Brute-force protection. Unlike the previous hand-rolled pattern (which
    // *skipped* rate limiting when no client IP was extractable), the guard
    // always rate-limits — unextractable IPs share a single UNSPECIFIED
    // bucket. This is an intentional behavior improvement: recover_db is a
    // brute-force-sensitive endpoint, and "no proxy headers → no rate
    // limiting" used to leave deployments without a reverse proxy exposed.
    let guard = match crate::rate_limiter::RateLimitGuard::enter(
        &state.rate_limiter,
        &headers,
        "recover_db",
    )
    .await
    {
        Ok(g) => g,
        Err(_) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(RecoverDbResponse {
                    success: false,
                    correct_hash: None,
                    correct_hash_base64: None,
                    error: Some("Too many attempts — try again later".into()),
                }),
            );
        }
    };

    // The SQLCipher DB is encrypted with the bcrypt HASH string (not the plaintext
    // password). Accept the hash in either raw ($2b$12$...) or base64 form.
    let candidate = body.candidate_password.trim().to_string();
    let candidate_hash = if candidate.starts_with("$2") {
        // Raw bcrypt hash
        candidate.clone()
    } else {
        // Try base64 decode → UTF-8 → bcrypt hash prefix check
        let decoded =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &candidate)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .filter(|s| s.starts_with("$2"));

        match decoded {
            Some(hash) => hash,
            None => {
                guard.record_failure();
                return (
                    StatusCode::BAD_REQUEST,
                    Json(RecoverDbResponse {
                        success: false,
                        correct_hash: None,
                        correct_hash_base64: None,
                        error: Some(
                            "Input is not a bcrypt hash. Provide the hash ($2b$12$...) or its base64 encoding."
                                .into(),
                        ),
                    }),
                );
            }
        }
    };

    // Try local .db.bak first
    let bak_path = crate::config_db::config_db_path().with_extension("db.bak");
    let try_path = if bak_path.exists() {
        Some(bak_path)
    } else {
        // Try S3 fallback if config_sync is enabled
        if let Some(ref sync) = state.config_sync {
            match sync.download_raw().await {
                Ok(data) => {
                    let tmp_path = crate::config_db::config_db_path().with_extension("db.recovery");
                    if let Err(e) = std::fs::write(&tmp_path, &data) {
                        tracing::warn!("Failed to write recovery temp file: {}", e);
                        None
                    } else {
                        Some(tmp_path)
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to download config DB from S3 for recovery: {}", e);
                    None
                }
            }
        } else {
            None
        }
    };

    let Some(db_path) = try_path else {
        return (
            StatusCode::NOT_FOUND,
            Json(RecoverDbResponse {
                success: false,
                correct_hash: None,
                correct_hash_base64: None,
                error: Some(
                    "No config database found to recover (no .bak file and no S3 copy)".into(),
                ),
            }),
        );
    };

    // Try to open with the candidate hash
    let is_recovery_temp = db_path
        .extension()
        .map(|e| e == "recovery")
        .unwrap_or(false);
    let result = crate::config_db::ConfigDb::open_or_create(&db_path, &candidate_hash);

    // Always clean up the recovery temp file (from S3 download), regardless of outcome
    if is_recovery_temp {
        let _ = std::fs::remove_file(&db_path);
    }

    match result {
        Ok(_db) => {
            guard.record_success();

            let hash_base64 = base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                candidate_hash.as_bytes(),
            );

            audit_log("recover_db_success", "admin", "", &headers);

            (
                StatusCode::OK,
                Json(RecoverDbResponse {
                    success: true,
                    correct_hash: Some(candidate_hash),
                    correct_hash_base64: Some(hash_base64),
                    error: None,
                }),
            )
        }
        Err(_) => {
            guard.record_failure();

            (
                StatusCode::UNAUTHORIZED,
                Json(RecoverDbResponse {
                    success: false,
                    correct_hash: None,
                    correct_hash_base64: None,
                    error: Some("Password does not match the encrypted database".into()),
                }),
            )
        }
    }
}
