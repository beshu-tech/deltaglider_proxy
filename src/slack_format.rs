// SPDX-License-Identifier: BUSL-1.1

//! Slack message formatting + notification filtering for event delivery.
//!
//! When `event_delivery.format = slack`, each outbox event is rendered as a
//! Slack message (Block Kit blocks + a `text` fallback for notifications and
//! screen readers) instead of the raw `{schema,event}` JSON envelope. Delivery
//! goes to either an Incoming Webhook URL or the Slack Web API
//! (`chat.postMessage`) — see `event_delivery.rs`.
//!
//! This module is PURE: `slack_message` builds a JSON value from an event +
//! config, and `should_notify` decides whether an event posts at all. Both are
//! unit-tested without any HTTP.

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde_json::{json, Value};

use crate::config_sections::EventDeliveryConfig;
use crate::event_outbox::EventOutboxRecord;
use crate::replication::event_consumer::is_user_object_key;

/// Build a globset from patterns; an empty pattern list yields an empty set
/// (matches nothing — callers treat "empty include" as "match all").
fn build_globset(patterns: &[String]) -> Result<GlobSet, String> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p).map_err(|e| format!("invalid slack glob {p:?}: {e}"))?);
    }
    b.build()
        .map_err(|e| format!("slack globset build failed: {e}"))
}

/// Whether `event` should post to Slack under `cfg`'s filters: the kind must be
/// in `slack_notify_kinds`, the key must be a real user object, and it must pass
/// the include/exclude globs (empty include = all; exclude always wins).
///
/// `include`/`exclude` are passed pre-compiled so a drain can build them once.
pub fn should_notify(
    event: &EventOutboxRecord,
    cfg: &EventDeliveryConfig,
    include: &GlobSet,
    exclude: &GlobSet,
) -> bool {
    if !cfg.slack_notify_kinds.iter().any(|k| k == &event.kind) {
        return false;
    }
    if !is_user_object_key(&event.key) {
        return false;
    }
    if exclude.is_match(&event.key) {
        return false;
    }
    cfg.slack_include_globs.is_empty() || include.is_match(&event.key)
}

/// Compile the (include, exclude) globsets for a Slack config. Done once per
/// delivery batch by the caller.
pub fn compile_slack_globs(cfg: &EventDeliveryConfig) -> Result<(GlobSet, GlobSet), String> {
    Ok((
        build_globset(&cfg.slack_include_globs)?,
        build_globset(&cfg.slack_exclude_globs)?,
    ))
}

/// Resolve which Slack channel(s) an eligible event posts to (bot-token mode).
///
/// - If `slack_routes` is non-empty, return the channel of EVERY route the event
///   matches (bucket match — `None` = any; AND prefix globs — empty = any),
///   deduped, order-preserving. An event can hit several channels, or none.
/// - If `slack_routes` is empty, fall back to the single `slack_channel`
///   (the default single-destination behavior) when set.
///
/// Returns an empty Vec when nothing matches (caller treats it as "delivered,
/// posted nowhere" — not an error).
pub fn resolve_channels(event: &EventOutboxRecord, cfg: &EventDeliveryConfig) -> Vec<String> {
    if cfg.slack_routes.is_empty() {
        return cfg
            .slack_channel
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(|c| vec![c.to_string()])
            .unwrap_or_default();
    }
    let mut out: Vec<String> = Vec::new();
    for route in &cfg.slack_routes {
        // Bucket constraint.
        if let Some(b) = route.bucket.as_deref() {
            if !b.trim().is_empty() && b != event.bucket {
                continue;
            }
        }
        // Prefix-glob constraint (empty = any key).
        if !route.prefix_globs.is_empty() {
            let Ok(set) = build_globset(&route.prefix_globs) else {
                continue; // an invalid route glob never matches (also surfaced as a config warning)
            };
            if !set.is_match(&event.key) {
                continue;
            }
        }
        let ch = route.channel.trim();
        if !ch.is_empty() && !out.iter().any(|c| c == ch) {
            out.push(ch.to_string());
        }
    }
    out
}

/// Emoji + human title for an event kind.
fn kind_presentation(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "ObjectCreated" => ("📦", "New object"),
        "ObjectCopied" | "ReplicationObjectCopied" => ("📑", "Object copied"),
        "ObjectDeleted" => ("🗑️", "Object deleted"),
        "LifecycleTransitioned" => ("➿", "Lifecycle transition"),
        "LifecycleExpired" => ("⌛", "Lifecycle expiry"),
        _ => ("🔔", "Object event"),
    }
}

/// Humanize a byte count for the message (e.g. 1536 → "1.5 KB").
fn human_size(bytes: i64) -> String {
    if bytes < 0 {
        return "unknown size".to_string();
    }
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Slack escapes `&`, `<`, `>` in message text (mrkdwn).
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Build the Slack message body (without `channel`/`username` — the delivery
/// layer adds those). Returns `{ text, blocks }`:
/// - `text` is the screen-reader / notification fallback.
/// - `blocks` is the rich Block Kit rendering (header + section + context).
pub fn slack_message(event: &EventOutboxRecord, _cfg: &EventDeliveryConfig) -> Value {
    let (emoji, title) = kind_presentation(&event.kind);
    let bucket = escape(&event.bucket);
    let key = escape(&event.key);

    // Pull optional details from the event payload (ObjectCreated carries these).
    let size = event
        .payload
        .get("content_length")
        .and_then(|v| v.as_i64())
        .map(human_size);
    let storage = event
        .payload
        .get("storage_type")
        .and_then(|v| v.as_str())
        .map(escape);
    let etag = event
        .payload
        .get("etag")
        .and_then(|v| v.as_str())
        .map(|e| escape(e.trim_matches('"')));

    // Plain-text fallback (accessibility / notification preview).
    let mut text = format!("{emoji} {title}: {bucket}/{key}");
    if let Some(s) = &size {
        text.push_str(&format!(" ({s})"));
    }

    // Context line: storage strategy · etag · timestamp.
    let mut context_bits: Vec<String> = Vec::new();
    if let Some(s) = &storage {
        context_bits.push(format!("storage: {s}"));
    }
    if let Some(e) = &etag {
        let short = if e.len() > 12 { &e[..12] } else { e.as_str() };
        context_bits.push(format!("etag: {short}"));
    }
    context_bits.push(format!("at: {}", iso8601(event.occurred_at)));

    let mut section_text = format!("*{bucket}*/`{key}`");
    if let Some(s) = &size {
        section_text.push_str(&format!("\nSize: *{s}*"));
    }

    json!({
        "text": text,
        "blocks": [
            {
                "type": "header",
                "text": { "type": "plain_text", "text": format!("{emoji} {title}"), "emoji": true }
            },
            {
                "type": "section",
                "text": { "type": "mrkdwn", "text": section_text }
            },
            {
                "type": "context",
                "elements": [
                    { "type": "mrkdwn", "text": context_bits.join("  ·  ") }
                ]
            }
        ]
    })
}

/// Format a unix timestamp as ISO-8601 UTC (no chrono dependency churn — we use
/// the same approach as the audit log).
fn iso8601(unix_secs: i64) -> String {
    use chrono::{TimeZone, Utc};
    Utc.timestamp_opt(unix_secs, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| unix_secs.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: &str, bucket: &str, key: &str, payload: Value) -> EventOutboxRecord {
        EventOutboxRecord {
            id: 1,
            kind: kind.to_string(),
            bucket: bucket.to_string(),
            key: key.to_string(),
            source: "s3_api".to_string(),
            occurred_at: 1_700_000_000,
            payload,
            status: "pending".to_string(),
            attempts: 0,
            next_attempt_at: None,
            claimed_by: None,
            claimed_at: None,
            delivered_at: None,
            last_error: None,
            created_at: 0,
        }
    }

    fn cfg_with(kinds: &[&str], include: &[&str], exclude: &[&str]) -> EventDeliveryConfig {
        EventDeliveryConfig {
            slack_notify_kinds: kinds.iter().map(|s| s.to_string()).collect(),
            slack_include_globs: include.iter().map(|s| s.to_string()).collect(),
            slack_exclude_globs: exclude.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn human_size_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(human_size(-1), "unknown size");
    }

    #[test]
    fn message_has_text_fallback_and_blocks() {
        let e = rec(
            "ObjectCreated",
            "builds",
            "ror/1.0/app.zip",
            json!({ "content_length": 1536, "storage_type": "delta", "etag": "\"abc123def456gh\"" }),
        );
        let m = slack_message(&e, &EventDeliveryConfig::default());
        let text = m["text"].as_str().unwrap();
        assert!(text.contains("New object"), "text fallback: {text}");
        assert!(text.contains("builds/ror/1.0/app.zip"));
        assert!(text.contains("1.5 KB"));
        let blocks = m["blocks"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "header");
        assert_eq!(blocks[1]["type"], "section");
        assert_eq!(blocks[2]["type"], "context");
        // section carries bucket/key + size; context carries storage + etag (truncated) + ts.
        let section = blocks[1]["text"]["text"].as_str().unwrap();
        assert!(section.contains("*builds*"));
        assert!(section.contains("`ror/1.0/app.zip`"));
        let ctx = blocks[2]["elements"][0]["text"].as_str().unwrap();
        assert!(ctx.contains("storage: delta"));
        assert!(ctx.contains("etag: abc123def456")); // 12-char truncation, quotes stripped
        assert!(ctx.contains("2023-11-14")); // iso8601 of 1_700_000_000
    }

    #[test]
    fn message_escapes_mrkdwn_special_chars() {
        let e = rec("ObjectCreated", "b<ad", "a&b>c", json!({}));
        let m = slack_message(&e, &EventDeliveryConfig::default());
        let text = m["text"].as_str().unwrap();
        assert!(text.contains("b&lt;ad"));
        assert!(text.contains("a&amp;b&gt;c"));
    }

    #[test]
    fn should_notify_kind_filter() {
        let cfg = cfg_with(&["ObjectCreated"], &[], &[]);
        let (inc, exc) = compile_slack_globs(&cfg).unwrap();
        assert!(should_notify(
            &rec("ObjectCreated", "b", "k.zip", json!({})),
            &cfg,
            &inc,
            &exc
        ));
        // ObjectDeleted not in notify_kinds → skip.
        assert!(!should_notify(
            &rec("ObjectDeleted", "b", "k.zip", json!({})),
            &cfg,
            &inc,
            &exc
        ));
    }

    #[test]
    fn should_notify_skips_internal_keys() {
        let cfg = cfg_with(&["ObjectCreated"], &[], &[]);
        let (inc, exc) = compile_slack_globs(&cfg).unwrap();
        assert!(!should_notify(
            &rec("ObjectCreated", "b", "ror/.dg/reference.bin", json!({})),
            &cfg,
            &inc,
            &exc
        ));
        assert!(!should_notify(
            &rec("ObjectCreated", "b", "dir/", json!({})),
            &cfg,
            &inc,
            &exc
        ));
        assert!(should_notify(
            &rec("ObjectCreated", "b", "real.zip", json!({})),
            &cfg,
            &inc,
            &exc
        ));
    }

    #[test]
    fn should_notify_include_exclude_globs() {
        // include only builds/**, exclude **/*.tmp
        let cfg = cfg_with(&["ObjectCreated"], &["builds/**"], &["**/*.tmp"]);
        let (inc, exc) = compile_slack_globs(&cfg).unwrap();
        assert!(should_notify(
            &rec("ObjectCreated", "b", "builds/app.zip", json!({})),
            &cfg,
            &inc,
            &exc
        ));
        // outside include → skip
        assert!(!should_notify(
            &rec("ObjectCreated", "b", "logs/x.txt", json!({})),
            &cfg,
            &inc,
            &exc
        ));
        // matches include but also exclude → exclude wins
        assert!(!should_notify(
            &rec("ObjectCreated", "b", "builds/scratch.tmp", json!({})),
            &cfg,
            &inc,
            &exc
        ));
    }

    #[test]
    fn empty_include_matches_all_user_objects() {
        let cfg = cfg_with(&["ObjectCreated"], &[], &[]);
        let (inc, exc) = compile_slack_globs(&cfg).unwrap();
        assert!(should_notify(
            &rec("ObjectCreated", "b", "anything/deep/x.bin", json!({})),
            &cfg,
            &inc,
            &exc
        ));
    }

    // ── resolve_channels (per-bucket / per-prefix routing) ──
    use crate::config_sections::SlackRoute;

    fn route(name: &str, bucket: Option<&str>, globs: &[&str], channel: &str) -> SlackRoute {
        SlackRoute {
            name: Some(name.to_string()),
            bucket: bucket.map(String::from),
            prefix_globs: globs.iter().map(|s| s.to_string()).collect(),
            channel: channel.to_string(),
        }
    }

    #[test]
    fn resolve_channels_falls_back_to_single_channel_when_no_routes() {
        let cfg = EventDeliveryConfig {
            slack_channel: Some("C_DEFAULT".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve_channels(&rec("ObjectCreated", "b", "k.zip", json!({})), &cfg),
            vec!["C_DEFAULT".to_string()]
        );
        // No channel + no routes → nowhere.
        let empty = EventDeliveryConfig::default();
        assert!(
            resolve_channels(&rec("ObjectCreated", "b", "k.zip", json!({})), &empty).is_empty()
        );
    }

    #[test]
    fn resolve_channels_routes_by_bucket() {
        let cfg = EventDeliveryConfig {
            slack_channel: Some("C_IGNORED".to_string()), // ignored once routes exist
            slack_routes: vec![
                route("rel", Some("releases"), &[], "C_RELEASES"),
                route("aud", Some("audit"), &[], "C_SECURITY"),
            ],
            ..Default::default()
        };
        assert_eq!(
            resolve_channels(&rec("ObjectCreated", "releases", "x.zip", json!({})), &cfg),
            vec!["C_RELEASES".to_string()]
        );
        assert_eq!(
            resolve_channels(&rec("ObjectCreated", "audit", "log.json", json!({})), &cfg),
            vec!["C_SECURITY".to_string()]
        );
        // A bucket no route covers → nowhere (routed config, no fallback).
        assert!(
            resolve_channels(&rec("ObjectCreated", "scratch", "x", json!({})), &cfg).is_empty()
        );
    }

    #[test]
    fn resolve_channels_fans_out_to_multiple_matches() {
        let cfg = EventDeliveryConfig {
            slack_routes: vec![
                route("all-releases", Some("releases"), &[], "C_TEAM"),
                route("builds-only", Some("releases"), &["builds/**"], "C_CI"),
            ],
            ..Default::default()
        };
        // A builds key matches BOTH routes → fans out to both channels.
        assert_eq!(
            resolve_channels(
                &rec("ObjectCreated", "releases", "builds/app.zip", json!({})),
                &cfg
            ),
            vec!["C_TEAM".to_string(), "C_CI".to_string()]
        );
        // A non-builds key matches only the first.
        assert_eq!(
            resolve_channels(
                &rec("ObjectCreated", "releases", "docs/readme.md", json!({})),
                &cfg
            ),
            vec!["C_TEAM".to_string()]
        );
    }

    #[test]
    fn resolve_channels_prefix_glob_and_any_bucket() {
        let cfg = EventDeliveryConfig {
            slack_routes: vec![
                // any bucket, only *.zip → one channel
                route("zips", None, &["**/*.zip"], "C_ARTIFACTS"),
            ],
            ..Default::default()
        };
        assert_eq!(
            resolve_channels(
                &rec("ObjectCreated", "anybucket", "deep/x.zip", json!({})),
                &cfg
            ),
            vec!["C_ARTIFACTS".to_string()]
        );
        assert!(resolve_channels(
            &rec("ObjectCreated", "anybucket", "deep/x.txt", json!({})),
            &cfg
        )
        .is_empty());
    }

    #[test]
    fn resolve_channels_dedupes_same_channel() {
        let cfg = EventDeliveryConfig {
            slack_routes: vec![
                route("a", Some("b"), &[], "C_SAME"),
                route("b", Some("b"), &["**"], "C_SAME"), // both match → dedupe
            ],
            ..Default::default()
        };
        assert_eq!(
            resolve_channels(&rec("ObjectCreated", "b", "x", json!({})), &cfg),
            vec!["C_SAME".to_string()]
        );
    }
}
