//! Admin GUI API handlers (separate from S3 SigV4 auth).

mod audit;
mod auth;
pub(crate) mod backends;
mod backup;
mod config;
pub mod external_auth;
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
use crate::iam::external_auth::ExternalAuthManager;
use crate::iam::SharedIamState;
use crate::rate_limiter::RateLimiter;
use crate::session::SessionStore;
use crate::usage_scanner::UsageScanner;

// Re-export everything so external code doesn't need import changes.
pub use audit::get_audit;
pub use auth::{
    check_session, clear_s3_session_creds, get_s3_session_creds, login, login_as, logout,
    require_not_declarative, require_session, set_s3_session_creds, whoami, LoginAsRequest,
    LoginResponse, SessionResponse, WhoamiQuery, WhoamiResponse,
};
pub use backends::{create_backend, delete_backend, list_backends};
pub use backup::{export_backup, import_backup};
pub use config::{
    apply_config_doc, change_password, config_defaults, export_config, get_config, get_section,
    put_section, recover_db, test_s3_connection, trace_config, trace_config_get, update_config,
    validate_config_doc, validate_section, BackendInfoResponse, ConfigApplyResponse,
    ConfigDocumentRequest, ConfigResponse, ConfigUpdateRequest, ConfigUpdateResponse,
    ConfigValidateResponse, PasswordChangeRequest, PasswordChangeResponse, SectionApplyResponse,
    TestS3Request, TestS3Response, TraceRequest, TraceResolved, TraceResponse,
};
pub use groups::{
    add_group_member, create_group, delete_group, list_groups, remove_group_member, update_group,
    AddGroupMemberRequest, CreateGroupRequest, UpdateGroupRequest,
};
pub use scanner::{get_usage, migrate_legacy, scan_usage, ScanUsageRequest, UsageQuery};
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
    /// Absolute path of the config file the server was started with, if any.
    /// When present, this is the authoritative target for config persistence —
    /// the admin API must not silently write to a different file resolved via
    /// `DGP_CONFIG` or the default-search-paths list. `None` means the server
    /// started with neither `--config` nor any file found on the search path,
    /// and any persist attempt should fall back to the canonical default.
    pub config_file_path: Option<String>,
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
    /// True if the bootstrap password hash doesn't match the existing config DB.
    /// When set, config sync is blocked and a recovery wizard is shown in the GUI.
    pub config_db_mismatch: bool,
    /// External authentication manager (OAuth/OIDC). None if no providers configured.
    pub external_auth: Option<Arc<ExternalAuthManager>>,
    /// Public prefix snapshot for unauthenticated read-only access. Hot-swappable.
    pub public_prefix_snapshot: crate::bucket_policy::SharedPublicPrefixSnapshot,
    /// Admission chain — pre-auth request gating. Hot-swappable via
    /// `arc_swap::ArcSwap`; readers call `load_full()` lock-free. Rebuilt
    /// whenever the bucket policy set changes (which is what the chain is
    /// currently derived from). See [`crate::admission`] for the type
    /// shape and evaluator.
    pub admission_chain: crate::admission::SharedAdmissionChain,
}

/// Trigger an async config DB upload to S3 if sync is enabled.
/// Spawns a background task so the caller is not blocked.
/// No-op when config_db_mismatch is true (prevents overwriting good DB with empty one).
pub(crate) fn trigger_config_sync(state: &Arc<AdminState>) {
    if state.config_db_mismatch {
        tracing::warn!("Config sync blocked — bootstrap password mismatch (recovery required)");
        return;
    }
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

/// Run a synchronous operation against the locked config DB.
///
/// Wraps the boilerplate that otherwise repeats in every admin IAM
/// handler: "pull the Option<Arc<Mutex<ConfigDb>>> out of the state
/// or return 404, lock it, run the op, on error log and return 500."
///
/// Post-mutation hooks (`rebuild_external_auth`, `rebuild_iam_index`,
/// `trigger_config_sync`, `audit_log`) stay explicit at the handler
/// level — they're called AFTER the lock is released and the order
/// between them matters, so hiding them behind this helper would
/// trade one kind of boilerplate for another. The important gain
/// here is a uniform "no DB" / "DB error" contract.
///
/// `op_label` is the noun that appears in the error log if the
/// closure returns `Err` — e.g. `"load auth providers"` produces
/// `Failed to load auth providers: <err>`.
///
/// # Example
///
/// ```ignore
/// pub async fn list_providers(
///     State(state): State<Arc<AdminState>>,
/// ) -> Result<impl IntoResponse, StatusCode> {
///     let providers = with_config_db(&state, "load auth providers", |db| {
///         db.load_auth_providers()
///     })
///     .await?;
///     Ok(Json(providers))
/// }
/// ```
pub(crate) async fn with_config_db<T, F, E>(
    state: &Arc<AdminState>,
    op_label: &str,
    f: F,
) -> Result<T, axum::http::StatusCode>
where
    F: FnOnce(&ConfigDb) -> Result<T, E>,
    E: std::fmt::Display,
{
    let db = state
        .config_db
        .as_ref()
        .ok_or(axum::http::StatusCode::NOT_FOUND)?;
    let db = db.lock().await;
    f(&db).map_err(|e| {
        tracing::error!("Failed to {op_label}: {e}");
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    })
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
