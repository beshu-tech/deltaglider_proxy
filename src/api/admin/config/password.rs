// SPDX-License-Identifier: BUSL-1.1

//! Password-change and config-database recovery handlers.
//!
//! The bootstrap password hash verifies admin-GUI logins and signs session
//! cookies. It is NOT the SQLCipher key of the IAM database (that is
//! `DGP_CONFIG_DB_KEY` or the key file, see `config_db::key`), so a password
//! change never touches the database. Recovery tries a candidate key (or a
//! legacy bootstrap hash) against a parked config DB.

use crate::api::admin::extract::AdminJson;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;

use super::super::{audit_log, validate_password, AdminState};

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

/// D15: the env var that pins the bootstrap hash, if one is set. That hash
/// wins at every boot, so a GUI change would be lost at the next start.
pub(crate) fn env_pinned_hash_var(env: impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    ["DGP_BOOTSTRAP_PASSWORD_HASH", "DGP_ADMIN_PASSWORD_HASH"]
        .into_iter()
        .find(|n| env(n).is_some_and(|v| !v.trim().is_empty()))
}

/// Make `hash` the bootstrap hash: the state file first (the next boot
/// reads it), then the in-memory login verifier and the config. Shared by
/// the password change and the backup restore.
pub(crate) async fn install_bootstrap_hash(state: &AdminState, hash: &str) -> std::io::Result<()> {
    let state_file = std::path::Path::new(".deltaglider_bootstrap_hash");
    crate::config::write_bootstrap_hash_file(state_file, hash)?;
    *state.password_hash.write() = hash.to_string();
    state.config.write().await.bootstrap_password_hash = Some(hash.to_string());
    Ok(())
}

/// PUT /api/admin/password — change bootstrap password.
///
/// Verify the current password, persist the new hash to the state file, and
/// only then swap the in-memory hash. The IAM database is not involved: its
/// key does not depend on the password.
pub async fn change_password(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    AdminJson(body): AdminJson<PasswordChangeRequest>,
) -> impl IntoResponse {
    if let Some(var) = env_pinned_hash_var(|n| std::env::var(n).ok()) {
        return password_err(
            StatusCode::CONFLICT,
            format!(
                "{var} is set, and it sets the bootstrap password hash at every start, \
                 so a change here would be lost at the next start. To change the \
                 password: run `deltaglider_proxy --set-bootstrap-password` (it reads \
                 the new password from stdin and prints its hash), set {var} to the new \
                 hash on every instance, and restart. The IAM database is not affected: \
                 its key does not depend on the bootstrap password."
            ),
        );
    }
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

    if let Err(e) = install_bootstrap_hash(&state, &new_hash).await {
        tracing::error!("Failed to persist new admin hash to disk: {}", e);
        return password_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "Failed to persist hash file ({}). Password change aborted.",
                e
            ),
        );
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

#[derive(Serialize, Default)]
pub struct RecoverDbResponse {
    success: bool,
    /// What the candidate is: `config_db_key` (set it as `DGP_CONFIG_DB_KEY`
    /// or write it to the key file) or `bootstrap_hash` (a DB from before S8,
    /// keyed with the bootstrap password hash: set it as the bootstrap hash).
    #[serde(skip_serializing_if = "Option::is_none")]
    key_kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    correct_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    correct_hash_base64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Pure: the keys to try for a recovery candidate, with the kind of each.
/// The candidate as typed is a config DB key; a bcrypt hash (raw or base64)
/// is also tried as the legacy key.
fn recovery_candidates(candidate: &str) -> Vec<(&'static str, String)> {
    let candidate = candidate.trim();
    if candidate.is_empty() {
        return Vec::new();
    }
    if candidate.starts_with("$2") {
        return vec![("bootstrap_hash", candidate.to_string())];
    }
    let mut out = vec![("config_db_key", candidate.to_string())];
    let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, candidate)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .filter(|s| s.starts_with("$2"));
    if let Some(hash) = decoded {
        out.push(("bootstrap_hash", hash));
    }
    out
}

/// POST /api/admin/recover-db — try a candidate key against the parked config DB.
///
/// Only available when `config_db_mismatch` is true. The candidate is a config
/// DB key (an earlier `DGP_CONFIG_DB_KEY` or key-file value) or, for a DB from
/// before S8, the bootstrap password hash (raw or base64). Read-only: the
/// response says which it is; the operator sets it and restarts, and the boot
/// promotes the parked DB.
pub async fn recover_db(
    State(state): State<Arc<AdminState>>,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    AdminJson(body): AdminJson<RecoverDbRequest>,
) -> impl IntoResponse {
    if !state.config_db_mismatch {
        return (
            StatusCode::NOT_FOUND,
            Json(RecoverDbResponse {
                error: Some("No config DB mismatch detected".into()),
                ..Default::default()
            }),
        )
            .into_response();
    }

    // Brute-force protection. Unlike the previous hand-rolled pattern (which
    // *skipped* rate limiting when no client IP was extractable), the guard
    // always rate-limits — unextractable IPs share a single UNSPECIFIED
    // bucket. This is an intentional behavior improvement: recover_db is a
    // brute-force-sensitive endpoint, and "no proxy headers → no rate
    // limiting" used to leave deployments without a reverse proxy exposed.
    // Per-IP + per-account: recover_db gates DB decryption with the
    // bcrypt hash that ALSO encrypts the config DB. A distributed
    // attack against the same proxy's recovery flow could otherwise
    // burn through the per-IP budget across a botnet — the
    // "bootstrap" subject ties this back to the single account.
    let guard = match crate::rate_limiter::RateLimitGuard::enter_with_account(
        &state.rate_limiter,
        &headers,
        connect_info.as_ref().map(|ci| ci.0.ip()),
        "bootstrap",
        "recover_db",
    )
    .await
    {
        Ok(g) => g,
        Err(blocked) => return blocked.into_response(),
    };

    let candidates = recovery_candidates(&body.candidate_password);
    if candidates.is_empty() {
        guard.record_failure();
        return (
            StatusCode::BAD_REQUEST,
            Json(RecoverDbResponse {
                error: Some("Provide a config DB key or a bootstrap password hash.".into()),
                ..Default::default()
            }),
        )
            .into_response();
    }

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
                error: Some(
                    "No config database found to recover (no .bak file and no S3 copy)".into(),
                ),
                ..Default::default()
            }),
        )
            .into_response();
    };

    let is_recovery_temp = db_path
        .extension()
        .map(|e| e == "recovery")
        .unwrap_or(false);
    // Read-only probe: the parked DB is never migrated or re-encrypted here.
    let matched = candidates
        .into_iter()
        .find(|(_, key)| crate::config_db::probe_key(&db_path, key).unwrap_or(false));

    // Always clean up the recovery temp file (from S3 download), regardless of outcome
    if is_recovery_temp {
        let _ = std::fs::remove_file(&db_path);
    }

    match matched {
        Some((kind, key)) => {
            guard.record_success();
            audit_log("recover_db_success", "admin", "", &headers);
            let (correct_hash, correct_hash_base64) = if kind == "bootstrap_hash" {
                let b64 = base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    key.as_bytes(),
                );
                (Some(key), Some(b64))
            } else {
                // The operator typed the key; never echo it back.
                (None, None)
            };
            // `no-store` keeps intermediaries and the browser's bfcache from
            // retaining a recovered hash.
            (
                StatusCode::OK,
                [
                    (
                        "cache-control",
                        "no-store, no-cache, must-revalidate, private",
                    ),
                    ("pragma", "no-cache"),
                ],
                Json(RecoverDbResponse {
                    success: true,
                    key_kind: Some(kind),
                    correct_hash,
                    correct_hash_base64,
                    error: None,
                }),
            )
                .into_response()
        }
        None => {
            guard.record_failure();
            (
                StatusCode::UNAUTHORIZED,
                Json(RecoverDbResponse {
                    error: Some("The key does not open the encrypted database".into()),
                    ..Default::default()
                }),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    /// S8: the password handler never re-encrypts the config DB (its key does
    /// not depend on the password). Source guard, so a revert is caught.
    #[test]
    fn password_change_never_rekeys_the_config_db() {
        let src = include_str!("password.rs");
        let handler = &src[..src.find("mod tests").unwrap()];
        assert!(
            !handler.contains(concat!(".re", "key(")),
            "password.rs calls rekey"
        );
    }

    #[test]
    fn recovery_candidates_truth_table() {
        use super::recovery_candidates as f;
        assert!(f("  ").is_empty());
        assert_eq!(
            f("$2b$04$abc"),
            vec![("bootstrap_hash", "$2b$04$abc".to_string())]
        );
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "$2b$04$abc");
        assert_eq!(
            f(&b64),
            vec![
                ("config_db_key", b64.clone()),
                ("bootstrap_hash", "$2b$04$abc".to_string())
            ]
        );
        assert_eq!(
            f(" 0123abcd "),
            vec![("config_db_key", "0123abcd".to_string())]
        );
    }

    #[test]
    fn env_pinned_hash_blocks_password_change() {
        use super::env_pinned_hash_var as f;
        assert_eq!(f(|_| None), None);
        assert_eq!(
            f(|n| (n == "DGP_BOOTSTRAP_PASSWORD_HASH").then(|| " ".into())),
            None
        );
        assert_eq!(
            f(|n| (n == "DGP_ADMIN_PASSWORD_HASH").then(|| "$2b$x".into())),
            Some("DGP_ADMIN_PASSWORD_HASH")
        );
    }
}
