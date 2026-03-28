//! Audit logging helpers for security compliance.
//!
//! Provides a single `audit_log` function used by both S3 handlers and admin API
//! for structured audit log output.

use axum::http::HeaderMap;

/// Sanitize a value for structured audit log output.
/// Prevents newline injection and pipe-delimiter confusion.
pub fn sanitize(s: &str) -> String {
    s.replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('|', "\\|")
}

/// Extract client IP and user-agent from request headers.
/// Uses `rate_limiter::extract_client_ip` which respects `DGP_TRUST_PROXY_HEADERS`.
pub fn extract_client_info(headers: &HeaderMap) -> (String, String) {
    let ip = crate::rate_limiter::extract_client_ip(headers)
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let ua_raw = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let ua = ua_raw
        .get(..256.min(ua_raw.len()))
        .unwrap_or(ua_raw)
        .to_string();
    (ip, ua)
}

/// Emit a structured audit log line for any mutation operation.
///
/// Format: `AUDIT | action=X | user=X | target=X | ip=X | ua=X | bucket=X | path=X`
///
/// `bucket` and `path` default to `""` when not applicable (admin API calls).
/// Use `audit_log_admin()` for admin actions that don't involve S3 resources.
pub fn audit_log(
    action: &str,
    user: &str,
    target: &str,
    headers: &HeaderMap,
    bucket: &str,
    path: &str,
) {
    let (ip, ua) = extract_client_info(headers);
    tracing::info!(
        "AUDIT | action={} | user={} | target={} | ip={} | ua={} | bucket={} | path={}",
        sanitize(action),
        sanitize(user),
        sanitize(target),
        sanitize(&ip),
        sanitize(&ua),
        sanitize(bucket),
        sanitize(path)
    );
}
