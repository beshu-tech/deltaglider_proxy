//! Admin GUI API handlers (separate from S3 SigV4 auth).

mod auth;
mod backup;
mod config;
mod groups;
mod scanner;
pub(crate) mod users;

use parking_lot::RwLock;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

use crate::api::handlers::AppState;
use crate::config::SharedConfig;
use crate::config_db::ConfigDb;
use crate::config_db_sync::ConfigDbSync;
use crate::iam::SharedIamState;
use crate::rate_limiter::RateLimiter;
use crate::session::SessionStore;
use crate::usage_scanner::UsageScanner;

// Re-export everything so external code doesn't need import changes.
pub use auth::{
    check_session, clear_s3_session_creds, get_s3_session_creds, login, login_as, logout,
    require_session, set_s3_session_creds, whoami, LoginAsRequest, LoginResponse, SessionResponse,
    WhoamiQuery, WhoamiResponse, WhoamiUser,
};
pub use backup::{export_backup, import_backup};
pub use config::{
    change_password, get_config, test_s3_connection, update_config, ConfigResponse,
    ConfigUpdateRequest, ConfigUpdateResponse, PasswordChangeRequest, PasswordChangeResponse,
    TestS3Request, TestS3Response,
};
pub use groups::{
    add_group_member, create_group, delete_group, list_groups, remove_group_member, update_group,
    AddGroupMemberRequest, CreateGroupRequest, UpdateGroupRequest,
};
pub use scanner::{get_usage, scan_usage, ScanUsageRequest, UsageQuery};
pub use users::{
    create_user, delete_user, get_canned_policies, list_users, rotate_user_keys, update_user,
    CreateUserRequest, RotateKeysRequest, UpdateUserRequest,
};

/// Type alias for the tracing reload handle.
pub type LogReloadHandle =
    tracing_subscriber::reload::Handle<EnvFilter, tracing_subscriber::Registry>;

/// Shared state for admin API routes.
pub struct AdminState {
    pub password_hash: RwLock<String>,
    pub sessions: Arc<SessionStore>,
    pub config: SharedConfig,
    pub log_reload: LogReloadHandle,
    pub s3_state: Arc<AppState>,
    pub iam_state: SharedIamState,
    /// Encrypted config database for IAM users (None in legacy/open-access mode).
    pub config_db: Option<Arc<tokio::sync::Mutex<ConfigDb>>>,
    /// Background usage scanner for computing prefix sizes.
    pub usage_scanner: Arc<UsageScanner>,
    /// Per-IP rate limiter for login endpoints and auth failures.
    pub rate_limiter: RateLimiter,
    /// S3 sync for the config database (None if DGP_CONFIG_SYNC_BUCKET is not set).
    pub config_sync: Option<Arc<ConfigDbSync>>,
}

/// Trigger an async config DB upload to S3 if sync is enabled.
/// Spawns a background task so the caller is not blocked.
pub(crate) fn trigger_config_sync(state: &Arc<AdminState>) {
    if let Some(ref sync) = state.config_sync {
        tokio::spawn({
            let sync = sync.clone();
            async move {
                if let Err(e) = sync.upload().await {
                    tracing::warn!("Config DB S3 sync upload failed: {}", e);
                }
            }
        });
    }
}

/// Admin audit log helper — delegates to `crate::audit::audit_log` with empty bucket/path.
/// Exists to avoid passing `"", ""` at every admin API call site.
pub(crate) fn audit_log(
    action: &str,
    admin_user: &str,
    target: &str,
    headers: &axum::http::HeaderMap,
) {
    crate::audit::audit_log(action, admin_user, target, headers, "", "");
}

/// Common password validation for both admin API and CLI.
/// Returns `Ok(())` if valid, `Err(message)` if invalid.
pub fn validate_password(password: &str) -> Result<(), &'static str> {
    if password.len() < 12 {
        return Err("Password must be at least 12 characters");
    }
    if password.len() > 128 {
        return Err("Password too long (max 128 characters)");
    }

    // Top 20 common passwords (12+ chars to match minimum length)
    const COMMON_PASSWORDS: &[&str] = &[
        "password1234",
        "123456789012",
        "admin1234567",
        "admin123456!",
        "password1234!",
        "qwerty123456",
        "letmein12345",
        "welcome12345",
        "monkey1234567",
        "dragon1234567",
        "master1234567",
        "1234567890ab",
        "changeme1234",
        "password12345",
        "adminadminadmin",
        "abcdefghijkl",
        "aaaaaaaaaaaa",
        "123456789abc",
        "passw0rd1234",
        "p@ssword1234",
    ];

    let lower = password.to_lowercase();
    if COMMON_PASSWORDS.iter().any(|p| lower == *p) {
        return Err("Password is too common");
    }

    Ok(())
}
