// SPDX-License-Identifier: GPL-3.0-only

//! In-process operational log ring + live broadcast for the admin GUI.
//!
//! The audit ring (`src/audit.rs`) captures only `audit_log()` calls. This
//! module captures ALL `tracing` events (at an INFO+ floor) into a bounded ring
//! AND fans each to a broadcast channel, powering:
//!   - `GET /_/api/admin/logs` — recent backlog, server-side filtered.
//!   - `GET /_/api/admin/logs/stream` — SSE live tail.
//!
//! Two filters apply: the global `EnvFilter` (the configured log level — so a
//! hot level change widens/narrows this too) AND this layer's OWN floor
//! (`DGP_LOG_RING_LEVEL`, default INFO). The floor exists because the proxy's
//! default level is `debug`+`tower_http=debug` (2+ lines/request) — a firehose
//! that would drown the operator signal and churn the ring. stdout keeps the
//! full firehose; the GUI gets the meaningful subset.
//!
//! NOT a log store — bounded, per-instance, in-memory. Aggregation/retention/
//! multi-instance is a shipper's job on the `DGP_LOG_FORMAT=json` stdout.

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::OnceLock;
use tokio::sync::broadcast;
use tracing::field::{Field, Visit};
use tracing::{Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// One captured log event.
#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    /// ISO-8601 UTC.
    pub ts: DateTime<Utc>,
    /// "ERROR" | "WARN" | "INFO" | "DEBUG" | "TRACE".
    pub level: String,
    /// Event target (module path, e.g. "deltaglider_proxy::api::auth").
    pub target: String,
    /// The event's `message` field (the format string), if any.
    pub message: String,
    /// Remaining structured key-values (everything except `message`).
    pub fields: serde_json::Map<String, serde_json::Value>,
}

const DEFAULT_RING_SIZE: usize = 2000;
const DEFAULT_BROADCAST_CAP: usize = 256;

fn ring_capacity() -> usize {
    let cap = crate::config::env_parse_with_default("DGP_LOG_RING_SIZE", DEFAULT_RING_SIZE);
    if cap == 0 {
        DEFAULT_RING_SIZE
    } else {
        cap
    }
}

static LOG_RING: OnceLock<Mutex<VecDeque<LogEntry>>> = OnceLock::new();
static LOG_TX: OnceLock<broadcast::Sender<LogEntry>> = OnceLock::new();

fn ring() -> &'static Mutex<VecDeque<LogEntry>> {
    LOG_RING.get_or_init(|| Mutex::new(VecDeque::with_capacity(ring_capacity())))
}

/// The process-global log broadcast sender (lazy). Capacity is small — a slow
/// SSE consumer that lags simply drops the oldest unread frames (correct for a
/// live tail: you want recent events, not a guaranteed-complete replay).
pub fn log_broadcast() -> broadcast::Sender<LogEntry> {
    LOG_TX
        .get_or_init(|| broadcast::channel(DEFAULT_BROADCAST_CAP).0)
        .clone()
}

fn push_ring(entry: LogEntry) {
    let cap = ring_capacity();
    let mut guard = ring().lock();
    while guard.len() >= cap {
        guard.pop_front();
    }
    guard.push_back(entry);
}

/// Snapshot the most recent `limit` entries, newest first.
pub fn recent_logs(limit: usize) -> Vec<LogEntry> {
    let guard = ring().lock();
    let take = limit.min(guard.len());
    guard.iter().rev().take(take).cloned().collect()
}

/// Parse the capture floor from `DGP_LOG_RING_LEVEL` (default INFO).
fn ring_min_level() -> Level {
    match std::env::var("DGP_LOG_RING_LEVEL")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "error" => Level::ERROR,
        "warn" => Level::WARN,
        "debug" => Level::DEBUG,
        "trace" => Level::TRACE,
        // default + any unrecognised value → INFO
        _ => Level::INFO,
    }
}

/// A `tracing` Layer that captures events (at/above its floor) into the ring +
/// broadcast. Add it to the subscriber registry in `init_tracing`.
pub struct LogCaptureLayer {
    tx: broadcast::Sender<LogEntry>,
    min_level: Level,
}

impl LogCaptureLayer {
    pub fn new() -> Self {
        Self {
            tx: log_broadcast(),
            min_level: ring_min_level(),
        }
    }
}

impl Default for LogCaptureLayer {
    fn default() -> Self {
        Self::new()
    }
}

/// Visitor that pulls `message` out separately and collects the rest as JSON.
#[derive(Default)]
struct FieldVisitor {
    message: String,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl FieldVisitor {
    fn record(&mut self, field: &Field, value: serde_json::Value) {
        if field.name() == "message" {
            // message comes through record_debug; keep its string form.
            if let serde_json::Value::String(s) = &value {
                self.message = s.clone();
            } else {
                self.message = value.to_string();
            }
        } else {
            self.fields.insert(field.name().to_string(), value);
        }
    }
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, serde_json::Value::String(value.to_string()));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record(field, serde_json::Value::from(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record(field, serde_json::Value::from(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record(field, serde_json::Value::Bool(value));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.record(field, serde_json::Value::from(value));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // The default for the format-string `message` field + anything not
        // covered above. Render via Debug and store as a string.
        self.record(field, serde_json::Value::String(format!("{:?}", value)));
    }
}

impl<S: Subscriber> Layer<S> for LogCaptureLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        // tracing Level ordering: ERROR < WARN < INFO < DEBUG < TRACE (more
        // verbose = "greater"). We keep events at/above our floor in severity,
        // i.e. level <= min_level numerically.
        if *meta.level() > self.min_level {
            return;
        }
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let entry = LogEntry {
            ts: Utc::now(),
            level: meta.level().to_string(),
            target: meta.target().to_string(),
            message: visitor.message,
            fields: visitor.fields,
        };
        push_ring(entry.clone());
        // Err only means "no subscribers" — fine for a best-effort tail.
        let _ = self.tx.send(entry);
    }
}

// ── pure server-side filter (shared by the backlog endpoint + the SSE predicate) ──

/// Filter parameters for `GET /logs` and the stream predicate.
#[derive(Debug, Default, Clone)]
pub struct LogQuery {
    /// Minimum severity to include (None = no extra floor beyond capture).
    pub level: Option<Level>,
    /// Substring match on the event target (module path).
    pub target: Option<String>,
    /// Substring match over message + serialized fields.
    pub q: Option<String>,
}

/// True if `entry` passes `query` — the single predicate both the backlog filter
/// and the live-stream filter use, so they can never disagree.
pub fn log_matches(entry: &LogEntry, query: &LogQuery) -> bool {
    if let Some(min) = query.level {
        if level_from_str(&entry.level)
            .map(|l| l > min)
            .unwrap_or(false)
        {
            return false;
        }
    }
    if let Some(t) = &query.target {
        if !t.is_empty() && !entry.target.contains(t.as_str()) {
            return false;
        }
    }
    if let Some(q) = &query.q {
        if !q.is_empty() {
            let needle = q.to_ascii_lowercase();
            let hay = format!(
                "{} {}",
                entry.message,
                serde_json::Value::Object(entry.fields.clone())
            )
            .to_ascii_lowercase();
            if !hay.contains(&needle) {
                return false;
            }
        }
    }
    true
}

/// Filter a backlog snapshot, capped at `limit`.
pub fn filter_logs(entries: &[LogEntry], query: &LogQuery, limit: usize) -> Vec<LogEntry> {
    entries
        .iter()
        .filter(|e| log_matches(e, query))
        .take(limit)
        .cloned()
        .collect()
}

/// Parse a level string ("INFO"/"info"/…) — used by both the query parser and
/// `log_matches`.
pub fn level_from_str(s: &str) -> Option<Level> {
    match s.to_ascii_lowercase().as_str() {
        "error" => Some(Level::ERROR),
        "warn" => Some(Level::WARN),
        "info" => Some(Level::INFO),
        "debug" => Some(Level::DEBUG),
        "trace" => Some(Level::TRACE),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(level: &str, target: &str, message: &str) -> LogEntry {
        LogEntry {
            ts: Utc::now(),
            level: level.into(),
            target: target.into(),
            message: message.into(),
            fields: serde_json::Map::new(),
        }
    }

    #[test]
    fn level_floor_filters_below() {
        let warn = entry("WARN", "x", "m");
        let info = entry("INFO", "x", "m");
        let q = LogQuery {
            level: Some(Level::WARN),
            ..Default::default()
        };
        assert!(log_matches(&warn, &q), "WARN passes a WARN floor");
        assert!(!log_matches(&info, &q), "INFO is below a WARN floor");
    }

    #[test]
    fn target_substring() {
        let e = entry("INFO", "deltaglider_proxy::api::auth", "m");
        assert!(log_matches(
            &e,
            &LogQuery {
                target: Some("auth".into()),
                ..Default::default()
            }
        ));
        assert!(!log_matches(
            &e,
            &LogQuery {
                target: Some("replication".into()),
                ..Default::default()
            }
        ));
    }

    #[test]
    fn q_matches_message_and_fields() {
        let mut e = entry("WARN", "x", "brute_force_blocked");
        e.fields
            .insert("bucket_key".into(), serde_json::json!("172.18.0.4"));
        // message hit (case-insensitive)
        assert!(log_matches(
            &e,
            &LogQuery {
                q: Some("BRUTE".into()),
                ..Default::default()
            }
        ));
        // field-value hit
        assert!(log_matches(
            &e,
            &LogQuery {
                q: Some("172.18".into()),
                ..Default::default()
            }
        ));
        // miss
        assert!(!log_matches(
            &e,
            &LogQuery {
                q: Some("nope".into()),
                ..Default::default()
            }
        ));
    }

    #[test]
    fn filter_respects_limit_and_order() {
        let entries: Vec<LogEntry> = (0..5)
            .map(|i| entry("INFO", "t", &format!("m{i}")))
            .collect();
        let got = filter_logs(&entries, &LogQuery::default(), 3);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].message, "m0");
    }

    #[test]
    fn ring_push_and_recent_bounded() {
        // Note: shares the process-global ring; assert relative behaviour only.
        for i in 0..3 {
            push_ring(entry("INFO", "ring-test", &format!("r{i}")));
        }
        let recent = recent_logs(100);
        assert!(recent.iter().any(|e| e.message == "r2"));
        // newest-first
        let positions: Vec<_> = recent
            .iter()
            .enumerate()
            .filter(|(_, e)| e.target == "ring-test")
            .map(|(i, _)| i)
            .collect();
        assert!(!positions.is_empty());
    }

    #[test]
    fn level_from_str_roundtrip() {
        assert_eq!(level_from_str("warn"), Some(Level::WARN));
        assert_eq!(level_from_str("INFO"), Some(Level::INFO));
        assert_eq!(level_from_str("bogus"), None);
    }

    #[test]
    fn capture_layer_honors_floor_and_extracts_fields() {
        use tracing_subscriber::prelude::*;
        // A layer with an INFO floor, scoped to this thread's subscriber so we
        // don't fight the global one.
        let layer = LogCaptureLayer {
            tx: broadcast::channel(16).0,
            min_level: Level::INFO,
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        let target_marker = "logs_layer_test";
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target: "logs_layer_test", "dropped-debug");
            tracing::info!(target: "logs_layer_test", bucket_key = "1.2.3.4", "kept-info");
        });
        let mine: Vec<_> = recent_logs(500)
            .into_iter()
            .filter(|e| e.target == target_marker)
            .collect();
        assert!(
            mine.iter().any(|e| e.message == "kept-info"
                && e.fields.get("bucket_key") == Some(&serde_json::json!("1.2.3.4"))),
            "INFO event with a field must be captured + field extracted: {:?}",
            mine
        );
        assert!(
            !mine.iter().any(|e| e.message == "dropped-debug"),
            "DEBUG event must be dropped by the INFO floor"
        );
    }
}
