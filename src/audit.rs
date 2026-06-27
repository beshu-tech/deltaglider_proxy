// SPDX-License-Identifier: GPL-3.0-only

//! Audit logging helpers for security compliance.
//!
//! Provides a single `audit_log` function used by both S3 handlers and admin API
//! for structured audit log output.
//!
//! ## In-memory ring buffer (Wave 11)
//!
//! Every `audit_log()` call ALSO pushes a structured `AuditEntry` onto a
//! process-local ring buffer. The admin GUI's Diagnostics → Audit page
//! reads this via `GET /api/admin/audit`. The ring is bounded
//! (default 500 entries; override via `DGP_AUDIT_RING_SIZE`) so memory
//! stays flat under high traffic — older entries fall off the back.
//!
//! The ring is supplementary — the underlying `tracing::info!` call
//! still fires so operators grepping stdout / JSON logs in production
//! see nothing change. The ring is strictly a UX convenience for the
//! admin panel, not a compliance substitute.

use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::OnceLock;

/// Sanitize a value for structured audit log output.
/// Prevents newline injection and pipe-delimiter confusion.
pub fn sanitize(s: &str) -> String {
    s.replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('|', "\\|")
}

/// Truncated (≤256 char) user-agent from request headers.
fn extract_ua(headers: &HeaderMap) -> String {
    let ua_raw = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    ua_raw
        .get(..256.min(ua_raw.len()))
        .unwrap_or(ua_raw)
        .to_string()
}

/// Extract client IP and user-agent from request headers (headers-only).
/// Uses `rate_limiter::extract_client_ip` which respects `DGP_TRUST_PROXY_HEADERS`.
/// Prefer [`extract_client_info_with_peer`] where the TCP peer is available —
/// without it, a client behind a proxy with trust-proxy off resolves to
/// "unknown" here while the rate limiter keys on the peer IP, producing
/// inconsistent log lines for the same request.
pub fn extract_client_info(headers: &HeaderMap) -> (String, String) {
    let ip = crate::rate_limiter::extract_client_ip(headers)
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    (ip, extract_ua(headers))
}

/// Like [`extract_client_info`] but resolves the IP exactly as the rate limiter
/// does (`extract_client_ip_with_peer`) — XFF/X-Real-IP when trusted, else the
/// TCP peer. This keeps audit lines and rate-limit events in agreement about
/// the client IP for the same request.
pub fn extract_client_info_with_peer(
    headers: &HeaderMap,
    peer_ip: Option<std::net::IpAddr>,
) -> (String, String) {
    let ip = crate::rate_limiter::extract_client_ip_with_peer(headers, peer_ip)
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    (ip, extract_ua(headers))
}

/// One audit-log entry in the in-memory ring.
///
/// Serde-friendly — the admin API ships the whole struct straight
/// through to the GUI without further shaping. Fields are all
/// already-sanitised copies of what went into the tracing line, so
/// the GUI doesn't have to worry about control-chars / pipe glyphs.
#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    /// ISO-8601 UTC timestamp with millis — human-readable and
    /// trivial to sort client-side.
    pub timestamp: DateTime<Utc>,
    pub action: String,
    pub user: String,
    pub target: String,
    pub ip: String,
    pub ua: String,
    pub bucket: String,
    pub path: String,
}

/// Global audit ring. Bounded VecDeque under a parking_lot Mutex —
/// writes are cheap (push-pop O(1)) and the read side takes one
/// lock per admin-panel fetch, so contention is a non-issue.
///
/// Wrapped in `OnceLock` so the first push creates it lazily with
/// the right capacity (from `DGP_AUDIT_RING_SIZE`, default 500).
static AUDIT_RING: OnceLock<Mutex<VecDeque<AuditEntry>>> = OnceLock::new();

/// Default ring capacity when `DGP_AUDIT_RING_SIZE` is unset or
/// invalid. 500 entries is big enough for ad-hoc incident
/// debugging, small enough that the process memory footprint is
/// trivial (~500 * ~1KB per entry = 0.5 MB in the absolute worst
/// case of maxed-out sanitised strings).
const DEFAULT_RING_SIZE: usize = 500;

/// Resolve the audit-ring capacity from `DGP_AUDIT_RING_SIZE` (default
/// 500). Routed through `config::env_parse_with_default` for consistent
/// warn-on-invalid behaviour; a parsed `0` is also treated as invalid
/// (an empty ring would silently drop every entry) and falls back to
/// the default.
fn ring_capacity() -> usize {
    let cap = crate::config::env_parse_with_default("DGP_AUDIT_RING_SIZE", DEFAULT_RING_SIZE);
    if cap == 0 {
        DEFAULT_RING_SIZE
    } else {
        cap
    }
}

fn ring() -> &'static Mutex<VecDeque<AuditEntry>> {
    AUDIT_RING.get_or_init(|| Mutex::new(VecDeque::with_capacity(ring_capacity())))
}

/// Push a sanitised entry to the ring. When the ring is at
/// capacity, the oldest entry falls off the back (ring buffer
/// semantics) so pushes stay O(1) and memory stays flat.
fn push_ring(entry: AuditEntry) {
    let cap = ring_capacity();
    let mut guard = ring().lock();
    while guard.len() >= cap {
        guard.pop_front();
    }
    guard.push_back(entry);
}

/// Snapshot the most recent `limit` audit entries, newest first.
/// Returns a cloned Vec so the caller can serialise without
/// holding the lock across await points.
pub fn recent_audit(limit: usize) -> Vec<AuditEntry> {
    let guard = ring().lock();
    let take = limit.min(guard.len());
    guard.iter().rev().take(take).cloned().collect()
}

/// Emit a structured audit log line for any mutation operation.
///
/// Format: `AUDIT | action=X | user=X | target=X | ip=X | ua=X | bucket=X | path=X`
///
/// `bucket` and `path` default to `""` when not applicable (admin API calls).
/// Use `audit_log_admin()` for admin actions that don't involve S3 resources.
///
/// Also pushes a copy of the (sanitised) fields onto the in-memory
/// audit ring so the admin GUI's Diagnostics → Audit panel has
/// something to show. Tracing + ring share the exact same sanitised
/// payload — nothing in the GUI comes from a different source than
/// what hits stdout.
pub fn audit_log(
    action: &str,
    user: &str,
    target: &str,
    headers: &HeaderMap,
    bucket: &str,
    path: &str,
) {
    let (ip, ua) = extract_client_info(headers);
    let s_action = sanitize(action);
    let s_user = sanitize(user);
    let s_target = sanitize(target);
    let s_ip = sanitize(&ip);
    let s_ua = sanitize(&ua);
    let s_bucket = sanitize(bucket);
    let s_path = sanitize(path);
    tracing::info!(
        "AUDIT | action={} | user={} | target={} | ip={} | ua={} | bucket={} | path={}",
        &s_action,
        &s_user,
        &s_target,
        &s_ip,
        &s_ua,
        &s_bucket,
        &s_path
    );
    push_ring(AuditEntry {
        timestamp: Utc::now(),
        action: s_action,
        user: s_user,
        target: s_target,
        ip: s_ip,
        ua: s_ua,
        bucket: s_bucket,
        path: s_path,
    });
}
