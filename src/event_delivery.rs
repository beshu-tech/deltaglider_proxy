// SPDX-License-Identifier: GPL-3.0-only

//! Background delivery for the durable event outbox.
//!
//! The dispatcher is intentionally conservative: it is disabled unless
//! `advanced.event_delivery.enabled=true` and a delivery target is set (a
//! webhook URL, or — in `format = slack` bot-token mode — a Slack bot token).
//! Request handlers never call this module; they only append to `event_outbox`.

use crate::background::parse_duration_or;
use crate::config::SharedConfig;
use crate::config_db::ConfigDb;
use crate::config_sections::{EventDeliveryConfig, EventDeliveryFormat};
use crate::event_outbox::{
    current_unix_seconds, EventOutboxRecord, STATUS_DELIVERED, STATUS_FAILED, STATUS_IN_PROGRESS,
    STATUS_PENDING,
};
use crate::security::{validate_outbound_url, UrlKind};
use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::Url;
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{info, warn};

/// Map a Slack Web API JSON response to a delivery result. The API returns HTTP
/// 200 even on failure; the authoritative status is the `ok` boolean, with the
/// reason in `error`. Pure so the bot-token path's success/retry decision is
/// unit-testable without a live Slack.
fn slack_api_result(body: &Value) -> Result<(), String> {
    if body.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        let err = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        Err(format!("slack chat.postMessage error: {err}"))
    }
}

const DEFAULT_TICK: Duration = Duration::from_secs(10);
const MIN_TICK: Duration = Duration::from_secs(1);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const MIN_TIMEOUT: Duration = Duration::from_millis(500);
const DEFAULT_RETRY_BASE: Duration = Duration::from_secs(5);
const DEFAULT_RETRY_MAX: Duration = Duration::from_secs(300);
const DEFAULT_STALE_CLAIM_AFTER: Duration = Duration::from_secs(60);
const DEFAULT_DELIVERED_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// A listener cursor older than this (no advance in 1h) is treated as inactive
/// and no longer pins the prune floor. A healthy consumer advances every tick
/// (seconds), so this only ever fires for a stuck/dead/disabled listener.
pub(crate) const LISTENER_CURSOR_STALE_SECS: i64 = 60 * 60;
/// Failed (retries-exhausted) rows age out after this even when
/// `delivered_retention` is 0 — bounds DB growth from a dead delivery target.
/// Only rows at or below the listener-cursor floor are eligible (never drops an
/// event replication hasn't consumed). 24h matches the default delivered window.
const FAILED_ROW_MAX_AGE_SECS: i64 = 24 * 60 * 60;

#[derive(Debug, Clone, Serialize)]
pub struct EventWebhookPayload<'a> {
    pub schema: &'static str,
    pub event: &'a EventOutboxRecord,
}

#[async_trait]
pub trait EventDeliveryClient: Send + Sync + 'static {
    async fn deliver(
        &self,
        config: &EventDeliveryConfig,
        event: &EventOutboxRecord,
    ) -> Result<(), String>;
}

#[derive(Clone)]
pub struct HttpWebhookDeliveryClient {
    client: reqwest::Client,
    /// When true, skip the per-URL SSRF `validate_outbound_url` check. ONLY set
    /// by tests that deliver to a local mock server; the production constructor
    /// (`Default`) leaves it false so private/metadata targets are rejected.
    skip_ssrf_check: bool,
}

impl Default for HttpWebhookDeliveryClient {
    fn default() -> Self {
        // Do NOT follow redirects: an operator-configured webhook URL is an
        // SSRF surface, and a redirect could bounce a request that passed
        // validate_outbound_url onto a private/metadata address. Pair with the
        // per-URL validate_outbound_url(_, UrlKind::Webhook) checks below.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            // Close the DNS-rebinding gap: a webhook hostname that resolves to a
            // metadata/private address fails closed at connect time (the
            // literal-IP validate_outbound_url check can't see DNS).
            .dns_resolver(std::sync::Arc::new(
                crate::security::SsrfGuardedResolver::new(crate::security::UrlKind::Webhook),
            ))
            .build()
            .unwrap_or_default();
        Self {
            client,
            skip_ssrf_check: false,
        }
    }
}

impl HttpWebhookDeliveryClient {
    /// Validate an operator-supplied delivery URL unless SSRF checks are
    /// disabled (tests only). Centralises the guard so both the raw-webhook and
    /// slack-incoming-webhook paths apply it identically.
    fn check_ssrf(&self, endpoint: &str, what: &str) -> Result<(), String> {
        if self.skip_ssrf_check {
            return Ok(());
        }
        validate_outbound_url(endpoint, UrlKind::Webhook)
            .map_err(|e| format!("{what} rejected: {e}"))
    }

    /// Test-only: like `default()` but skips the SSRF guard so tests can deliver
    /// to a `127.0.0.1` mock server. Never used in production code.
    #[cfg(test)]
    fn for_tests() -> Self {
        Self {
            skip_ssrf_check: true,
            ..Self::default()
        }
    }
}

/// Redact a delivery URL for use in an error string that is PERSISTED to the
/// outbox (`last_error`) and shown in the admin API. A Slack incoming-webhook
/// URL is bearer-equivalent (the `hooks.slack.com` path token is the secret), so
/// emitting the full URL would leak it. Keep `scheme://host` for diagnosability,
/// replace the path/query with `/<redacted>`. Pure + used by every error site.
fn redact_url_for_error(raw: &str) -> String {
    match Url::parse(raw) {
        Ok(u) => {
            let host = u.host_str().unwrap_or("");
            let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
            // Only redact when there's a non-trivial path/query to hide.
            if u.path().trim_matches('/').is_empty() && u.query().is_none() {
                format!("{}://{host}{port}", u.scheme())
            } else {
                format!("{}://{host}{port}/<redacted>", u.scheme())
            }
        }
        // Unparseable → don't echo it verbatim (could itself be a malformed
        // secret); show a fixed placeholder.
        Err(_) => "<invalid-url>".to_string(),
    }
}

#[async_trait]
impl EventDeliveryClient for HttpWebhookDeliveryClient {
    async fn deliver(
        &self,
        config: &EventDeliveryConfig,
        event: &EventOutboxRecord,
    ) -> Result<(), String> {
        let timeout = parse_duration_or(
            &config.request_timeout,
            DEFAULT_TIMEOUT,
            MIN_TIMEOUT,
            "event_delivery.request_timeout",
        );

        match config.format {
            EventDeliveryFormat::Slack => self.deliver_slack(config, event, timeout).await,
            EventDeliveryFormat::Raw => self.deliver_raw(config, event, timeout).await,
        }
    }
}

impl HttpWebhookDeliveryClient {
    /// Existing behavior: POST the `{schema,event}` envelope to every webhook
    /// endpoint with the configured static headers.
    async fn deliver_raw(
        &self,
        config: &EventDeliveryConfig,
        event: &EventOutboxRecord,
        timeout: Duration,
    ) -> Result<(), String> {
        let endpoints = config.webhook_endpoints();
        if endpoints.is_empty() {
            return Err("event delivery enabled without webhook endpoint".to_string());
        }
        let payload = EventWebhookPayload {
            schema: "deltaglider.event.v1",
            event,
        };
        for endpoint in endpoints {
            // SSRF guard: reject private/loopback/metadata targets before any
            // outbound request (the client also refuses to follow redirects).
            self.check_ssrf(endpoint, "webhook endpoint")?;
            let url = Url::parse(endpoint).map_err(|e| format!("invalid webhook endpoint: {e}"))?;
            let mut request = self
                .client
                .post(url)
                .timeout(timeout)
                .header("user-agent", "deltaglider-proxy-event-outbox");
            for (name, value) in &config.webhook_headers {
                let name = HeaderName::from_bytes(name.as_bytes())
                    .map_err(|e| format!("invalid webhook header name {name:?}: {e}"))?;
                let value = HeaderValue::from_str(value)
                    .map_err(|e| format!("invalid webhook header value for {name}: {e}"))?;
                request = request.header(name, value);
            }
            let response = request
                .json(&payload)
                .send()
                .await
                .map_err(|e| format!("{}: {e}", redact_url_for_error(endpoint)))?;
            if !response.status().is_success() {
                return Err(format!(
                    "{}: webhook returned HTTP {}",
                    redact_url_for_error(endpoint),
                    response.status()
                ));
            }
        }
        Ok(())
    }

    /// Slack delivery: format the event as a Slack message and POST it either to
    /// the Incoming Webhook URLs or, when a bot token is set, to the Slack Web
    /// API `chat.postMessage`. Events filtered out by `should_notify` are a
    /// silent success (consumed, not posted).
    async fn deliver_slack(
        &self,
        config: &EventDeliveryConfig,
        event: &EventOutboxRecord,
        timeout: Duration,
    ) -> Result<(), String> {
        let (include, exclude) = crate::slack_format::compile_slack_globs(config)?;
        if !crate::slack_format::should_notify(event, config, &include, &exclude) {
            return Ok(()); // not a notifying event — consume without posting
        }
        let mut body = crate::slack_format::slack_message(event, config);

        if config.uses_slack_bot_token() {
            // Slack Web API: chat.postMessage. Returns HTTP 200 even on error —
            // the real status is in the JSON `{ "ok": bool, "error": ... }`.
            let token = config
                .slack_bot_token
                .as_deref()
                .unwrap_or_default()
                .trim()
                .to_string();
            // Resolve target channel(s): per-route fan-out, or the single
            // slack_channel fallback. An event may hit several channels.
            let channels = crate::slack_format::resolve_channels(event, config);
            if channels.is_empty() {
                // No route matched and no fallback channel — for a routed config
                // this is a legitimate "post nowhere". For a misconfigured single
                // destination, surface the missing channel.
                if config.slack_routes.is_empty() {
                    return Err("slack bot-token mode requires slack_channel".to_string());
                }
                return Ok(()); // routed, but this event matched no route
            }
            // Post to each resolved channel, collecting per-channel outcomes.
            //
            // CRITICAL (at-least-once + fan-out): the outbox tracks ONE status
            // per event row, not per channel. If we returned Err the moment any
            // single channel failed, the whole row would re-queue and the next
            // retry would re-post to the channels that ALREADY succeeded —
            // duplicate Slack spam (chat.postMessage has no idempotency key).
            //
            // So: a PARTIAL success counts as delivered. We only return Err (→
            // retry) when EVERY channel failed — in which case a retry re-posts
            // to all, but none had succeeded, so there's no duplication. Channels
            // that fail while others succeed are logged and dropped (a missed
            // notification to one bad channel beats duplicating to the good ones).
            let mut any_ok = false;
            let mut failures: Vec<String> = Vec::new();
            for channel in &channels {
                let mut msg = body.clone();
                if let Value::Object(ref mut map) = msg {
                    map.insert("channel".to_string(), Value::String(channel.clone()));
                }
                let result: Result<(), String> = async {
                    let response = self
                        .client
                        .post("https://slack.com/api/chat.postMessage")
                        .timeout(timeout)
                        .bearer_auth(&token)
                        .json(&msg)
                        .send()
                        .await
                        .map_err(|e| format!("{e}"))?;
                    if !response.status().is_success() {
                        return Err(format!("HTTP {}", response.status()));
                    }
                    let parsed: Value = response.json().await.map_err(|e| format!("parse: {e}"))?;
                    slack_api_result(&parsed)
                }
                .await;
                match result {
                    Ok(()) => any_ok = true,
                    Err(e) => {
                        warn!("slack chat.postMessage to {channel} failed: {e}");
                        failures.push(format!("{channel}: {e}"));
                    }
                }
            }
            // Partial success (≥1 channel OK) or all OK → delivered. Only when
            // EVERY channel failed do we fail the row for retry (no dup risk —
            // nothing was delivered yet).
            if any_ok {
                return Ok(());
            }
            return Err(format!(
                "slack chat.postMessage failed for all {} channel(s): {}",
                channels.len(),
                failures.join("; ")
            ));
        }

        // Incoming Webhook mode: POST {text, blocks, username?, icon_emoji?} to
        // every configured hooks.slack.com URL. 2xx = delivered.
        let endpoints = config.webhook_endpoints();
        if endpoints.is_empty() {
            return Err("slack delivery enabled without a webhook URL or bot token".to_string());
        }
        if let Value::Object(ref mut map) = body {
            if let Some(u) = config.slack_username.as_deref().filter(|s| !s.is_empty()) {
                map.insert("username".to_string(), Value::String(u.to_string()));
            }
            if let Some(i) = config.slack_icon_emoji.as_deref().filter(|s| !s.is_empty()) {
                map.insert("icon_emoji".to_string(), Value::String(i.to_string()));
            }
        }
        // Validate ALL endpoints (SSRF + URL parse) BEFORE sending anything, so
        // one bad URL doesn't leave a partial fan-out that the retry then
        // duplicates. A config-level rejection is not a delivery attempt.
        let mut urls = Vec::with_capacity(endpoints.len());
        for endpoint in &endpoints {
            self.check_ssrf(endpoint, "slack webhook URL")?;
            urls.push(Url::parse(endpoint).map_err(|e| format!("invalid slack webhook URL: {e}"))?);
        }
        // Attempt EVERY endpoint (don't abort on the first failure) so a
        // transient error on one doesn't skip the rest, then fail if any failed.
        // ponytail: the outbox row has a single status, so a retry re-POSTs to
        // endpoints that already succeeded — duplicate Slack messages on partial
        // failure. Eliminating that needs per-endpoint delivery tracking (a
        // schema change); until then multi-URL Slack is at-least-once per URL.
        let mut errors: Vec<String> = Vec::new();
        for (endpoint, url) in endpoints.iter().zip(urls) {
            let result = self
                .client
                .post(url)
                .timeout(timeout)
                .json(&body)
                .send()
                .await;
            match result {
                Err(e) => errors.push(format!("{}: {e}", redact_url_for_error(endpoint))),
                Ok(response) if !response.status().is_success() => errors.push(format!(
                    "{}: slack webhook returned HTTP {}",
                    redact_url_for_error(endpoint),
                    response.status()
                )),
                Ok(_) => {}
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

pub fn spawn_dispatcher(
    config: SharedConfig,
    db: Arc<Mutex<ConfigDb>>,
) -> tokio::task::JoinHandle<()> {
    spawn_dispatcher_with_client(config, db, Arc::new(HttpWebhookDeliveryClient::default()))
}

pub fn spawn_dispatcher_with_client(
    config: SharedConfig,
    db: Arc<Mutex<ConfigDb>>,
    client: Arc<dyn EventDeliveryClient>,
) -> tokio::task::JoinHandle<()> {
    let claimant = format!("event-delivery:{}", uuid::Uuid::new_v4());
    tokio::spawn(async move {
        info!("Event outbox dispatcher started: claimant={}", claimant);
        loop {
            let cfg = { config.read().await.event_delivery.clone() };
            tokio::time::sleep(dispatcher_tick(&cfg)).await;
            if !cfg.is_active() {
                // Delivery disabled — but STILL prune the outbox below the
                // active-listener floor, or pending rows consumed by
                // replication (but never delivered) grow without bound
                // (X-ray H14). No dispatch, just the floor prune.
                prune_below_floor(&db, &cfg, current_unix_seconds()).await;
                continue;
            }
            dispatch_once(
                &db,
                client.as_ref(),
                &cfg,
                &claimant,
                current_unix_seconds(),
            )
            .await;
        }
    })
}

/// Prune outbox rows at or below the active-listener floor, regardless of
/// whether delivery is active. Bounds the append-only outbox when delivery is
/// disabled (pending rows consumed by replication but never delivered).
pub async fn prune_below_floor(db: &Arc<Mutex<ConfigDb>>, config: &EventDeliveryConfig, now: i64) {
    let batch = config.prune_batch.max(1);
    let db = db.lock().await;
    let min_keep_id = db
        .event_outbox_min_active_listener_cursor(now, LISTENER_CURSOR_STALE_SECS)
        .unwrap_or(None)
        .unwrap_or(i64::MAX);
    if let Err(err) = db.event_outbox_prune_below_floor(min_keep_id, batch) {
        warn!("Event outbox floor prune failed: {}", err);
    }
}

pub async fn dispatch_once(
    db: &Arc<Mutex<ConfigDb>>,
    client: &dyn EventDeliveryClient,
    config: &EventDeliveryConfig,
    claimant: &str,
    now: i64,
) {
    if !config.is_active() {
        return;
    }

    let claimed = {
        let db = db.lock().await;
        match db.event_outbox_claim_due(
            claimant,
            now,
            stale_claim_after_secs(config),
            config.batch_size.clamp(1, 500),
        ) {
            Ok(rows) => rows,
            Err(err) => {
                warn!("Event outbox claim failed: {}", err);
                return;
            }
        }
    };

    for event in claimed {
        let outcome = client.deliver(config, &event).await;
        let db = db.lock().await;
        match outcome {
            Ok(()) => {
                if let Err(err) = db.event_outbox_mark_delivered(event.id, current_unix_seconds()) {
                    warn!(
                        "Event outbox mark delivered failed for {}: {}",
                        event.id, err
                    );
                }
            }
            Err(err) => {
                let next_attempt_at = next_attempt_after(config, event.attempts, now);
                if let Err(mark_err) =
                    db.event_outbox_mark_failed(event.id, &truncate_error(&err), next_attempt_at)
                {
                    warn!(
                        "Event outbox mark failed failed for {}: {}",
                        event.id, mark_err
                    );
                }
            }
        }
    }

    // Prune FLOOR: never delete a delivered row that a slower listener (e.g.
    // event-driven replication) hasn't consumed yet. We may only remove rows at
    // or below the smallest ACTIVE listener cursor. A cursor that hasn't
    // advanced within LISTENER_CURSOR_STALE_SECS (consumer disabled, a wedged
    // rule, or a dead instance holding the lease) is treated as inactive and no
    // longer pins the floor — otherwise the append-only outbox would grow
    // without bound. No active listeners → no floor.
    let min_keep_id = {
        let db = db.lock().await;
        db.event_outbox_min_active_listener_cursor(now, LISTENER_CURSOR_STALE_SECS)
            .unwrap_or(None)
            .unwrap_or(i64::MAX)
    };

    let retention = delivered_retention_secs(config);
    if retention > 0 {
        let before = now.saturating_sub(retention);
        let db = db.lock().await;
        if let Err(err) =
            db.event_outbox_prune_delivered_before(before, config.prune_batch, min_keep_id)
        {
            warn!("Event outbox delivered prune failed: {}", err);
        }
    }
    // Age out terminal failed rows independently of `delivered_retention` — a dead
    // target grows the DB even when delivered-retention is 0. Uses a fixed window
    // (or the delivered retention if larger) and the SAME listener-cursor floor so
    // it never drops an event replication hasn't consumed.
    {
        let failed_window = retention.max(FAILED_ROW_MAX_AGE_SECS);
        let before = now.saturating_sub(failed_window);
        let db = db.lock().await;
        if let Err(err) =
            db.event_outbox_prune_failed_before(before, config.prune_batch, min_keep_id)
        {
            warn!("Event outbox failed prune failed: {}", err);
        }
    }
    if config.prune_batch > 0 {
        let db = db.lock().await;
        if let Err(err) = db.event_outbox_prune_delivered_over_count(
            config.delivered_max_rows,
            config.prune_batch,
            min_keep_id,
        ) {
            warn!("Event outbox delivered count-prune failed: {}", err);
        }
        // Also prune any-status rows fully below the floor (consumed by every
        // active listener), so pending rows that were consumed but not
        // delivered don't accumulate.
        if let Err(err) = db.event_outbox_prune_below_floor(min_keep_id, config.prune_batch) {
            warn!("Event outbox floor prune failed: {}", err);
        }
    }
}

pub(crate) fn dispatcher_tick(config: &EventDeliveryConfig) -> Duration {
    parse_duration_or(
        &config.tick_interval,
        DEFAULT_TICK,
        MIN_TICK,
        "event_delivery.tick_interval",
    )
}

pub(crate) fn next_attempt_after(
    config: &EventDeliveryConfig,
    attempts_after_claim: i64,
    now: i64,
) -> Option<i64> {
    if attempts_after_claim >= config.max_attempts.max(1) as i64 {
        return None;
    }
    let base = parse_duration_or(
        &config.retry_base,
        DEFAULT_RETRY_BASE,
        Duration::from_secs(1),
        "event_delivery.retry_base",
    )
    .as_secs();
    let max = parse_duration_or(
        &config.retry_max,
        DEFAULT_RETRY_MAX,
        Duration::from_secs(1),
        "event_delivery.retry_max",
    )
    .as_secs();
    let exponent = attempts_after_claim.saturating_sub(1).clamp(0, 20) as u32;
    let delay = base.saturating_mul(2_u64.saturating_pow(exponent)).min(max);
    Some(now.saturating_add(delay as i64))
}

pub(crate) fn stale_claim_after_secs(config: &EventDeliveryConfig) -> i64 {
    parse_duration_or(
        &config.stale_claim_after,
        DEFAULT_STALE_CLAIM_AFTER,
        Duration::from_secs(1),
        "event_delivery.stale_claim_after",
    )
    .as_secs() as i64
}

pub(crate) fn delivered_retention_secs(config: &EventDeliveryConfig) -> i64 {
    parse_duration_or(
        &config.delivered_retention,
        DEFAULT_DELIVERED_RETENTION,
        Duration::from_secs(0),
        "event_delivery.delivered_retention",
    )
    .as_secs() as i64
}

fn truncate_error(error: &str) -> String {
    const MAX_ERROR_LEN: usize = 1000;
    if error.len() <= MAX_ERROR_LEN {
        return error.to_string();
    }
    // Slice on a CHAR boundary: a naive `&error[..1000]` panics when byte 1000
    // lands mid-UTF-8-char (e.g. a backend error carrying multi-byte text),
    // which would kill the delivery dispatcher permanently. Walk back to the
    // nearest boundary at or below the cap.
    let mut end = MAX_ERROR_LEN;
    while end > 0 && !error.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &error[..end])
}

pub fn known_status(status: &str) -> bool {
    matches!(
        status,
        STATUS_PENDING | STATUS_IN_PROGRESS | STATUS_DELIVERED | STATUS_FAILED
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_db::ConfigDb;

    #[test]
    fn truncate_error_never_panics_on_multibyte_boundary() {
        // A multi-byte char straddling the 1000-byte cap must not panic.
        let s = format!("{}é{}", "a".repeat(999), "b".repeat(50));
        let out = truncate_error(&s);
        assert!(out.ends_with("..."));
        assert!(out.len() <= 1003);
        // A short string is returned verbatim.
        assert_eq!(truncate_error("short"), "short");
        // An all-multibyte string longer than the cap still truncates cleanly.
        let multi = "€".repeat(500); // 1500 bytes
        let out = truncate_error(&multi);
        assert!(out.ends_with("..."));
    }
    use crate::event_outbox::{EventKind, EventSource, NewEvent};
    use axum::{
        http::{HeaderMap, StatusCode},
        routing::post,
        Json, Router,
    };
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::{timeout, Duration};

    struct FakeClient {
        failures_before_success: usize,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl EventDeliveryClient for FakeClient {
        async fn deliver(
            &self,
            _config: &EventDeliveryConfig,
            _event: &EventOutboxRecord,
        ) -> Result<(), String> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < self.failures_before_success {
                Err("boom".to_string())
            } else {
                Ok(())
            }
        }
    }

    fn cfg() -> EventDeliveryConfig {
        EventDeliveryConfig {
            enabled: true,
            webhook_url: Some("http://example.invalid/hook".to_string()),
            tick_interval: "1s".to_string(),
            batch_size: 10,
            request_timeout: "1s".to_string(),
            max_attempts: 2,
            retry_base: "5s".to_string(),
            retry_max: "30s".to_string(),
            stale_claim_after: "60s".to_string(),
            delivered_retention: "1h".to_string(),
            delivered_max_rows: 10_000,
            prune_batch: 100,
            ..Default::default()
        }
    }

    fn event(key: &str) -> NewEvent {
        NewEvent::new(
            EventKind::ObjectCreated,
            "bucket",
            key,
            EventSource::S3Api,
            100,
            json!({ "size": 1 }),
        )
    }

    /// M2: error strings persisted to the outbox must not leak a Slack
    /// incoming-webhook secret (its path token). redact_url_for_error keeps
    /// scheme+host, masks the path/query.
    #[test]
    fn redact_url_for_error_hides_slack_path_token() {
        assert_eq!(
            redact_url_for_error("https://hooks.slack.com/services/T01/B02/SECRETtoken"),
            "https://hooks.slack.com/<redacted>"
        );
        assert_eq!(
            redact_url_for_error("https://example.com/hook?token=abc"),
            "https://example.com/<redacted>"
        );
        // No path/query → nothing secret to hide; host-only is fine.
        assert_eq!(
            redact_url_for_error("https://example.com/"),
            "https://example.com"
        );
        assert_eq!(redact_url_for_error("not a url"), "<invalid-url>");
        // Port preserved for diagnosability.
        assert_eq!(
            redact_url_for_error("http://host:8080/a/b"),
            "http://host:8080/<redacted>"
        );
    }

    /// M1 (SSRF): the PRODUCTION client (`default()`, guard ON) must reject a
    /// private/loopback/metadata webhook target before any network call, for
    /// both the raw and Slack-incoming-webhook formats.
    #[tokio::test]
    async fn production_client_rejects_ssrf_webhook_targets() {
        let prod = HttpWebhookDeliveryClient::default();
        // Insert one event + claim it so we have a real EventOutboxRecord; the
        // SSRF guard fires before the record content matters.
        let db = ConfigDb::in_memory("test-pass").unwrap();
        db.event_outbox_insert(&event("k")).unwrap();
        let rec = db
            .event_outbox_claim_due("w", current_unix_seconds() + 1, 60, 1)
            .unwrap()
            .pop()
            .unwrap();

        for url in [
            "http://169.254.169.254/latest/meta-data/", // cloud metadata
            "http://127.0.0.1:9000/hook",               // loopback
            "http://10.0.0.5/hook",                     // private RFC1918
            "http://localhost/hook",                    // loopback by name
        ] {
            // Raw webhook format.
            let mut c = cfg();
            c.webhook_url = Some(url.to_string());
            c.webhook_urls = Vec::new();
            let err = prod
                .deliver(&c, &rec)
                .await
                .expect_err("SSRF target must be rejected (raw)");
            assert!(
                err.contains("rejected"),
                "expected SSRF rejection for {url}, got: {err}"
            );

            // Slack incoming-webhook format.
            let mut s = cfg();
            s.format = EventDeliveryFormat::Slack;
            s.webhook_url = Some(url.to_string());
            s.webhook_urls = Vec::new();
            s.slack_bot_token = None;
            let err = prod
                .deliver(&s, &rec)
                .await
                .expect_err("SSRF target must be rejected (slack)");
            assert!(
                err.contains("rejected"),
                "expected SSRF rejection for slack {url}, got: {err}"
            );
        }
    }

    #[tokio::test]
    async fn dispatch_marks_success_delivered() {
        let db = Arc::new(Mutex::new(ConfigDb::in_memory("test-pass").unwrap()));
        let id = {
            let db = db.lock().await;
            db.event_outbox_insert(&event("ok")).unwrap()
        };
        let client = FakeClient {
            failures_before_success: 0,
            calls: AtomicUsize::new(0),
        };

        dispatch_once(&db, &client, &cfg(), "test-worker", 200).await;

        let rows = db.lock().await.event_outbox_recent(10).unwrap();
        let row = rows.iter().find(|r| r.id == id).unwrap();
        assert_eq!(row.status, STATUS_DELIVERED);
        assert_eq!(row.attempts, 1);
        assert_eq!(client.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dispatch_retries_then_permanently_fails() {
        let db = Arc::new(Mutex::new(ConfigDb::in_memory("test-pass").unwrap()));
        let id = {
            let db = db.lock().await;
            db.event_outbox_insert(&event("fail")).unwrap()
        };
        let client = FakeClient {
            failures_before_success: 99,
            calls: AtomicUsize::new(0),
        };
        let config = cfg();

        dispatch_once(&db, &client, &config, "test-worker", 200).await;
        let row = db
            .lock()
            .await
            .event_outbox_recent(10)
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert_eq!(row.status, STATUS_PENDING);
        assert_eq!(row.next_attempt_at, Some(205));

        dispatch_once(&db, &client, &config, "test-worker", 205).await;
        let row = db
            .lock()
            .await
            .event_outbox_recent(10)
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert_eq!(row.status, STATUS_FAILED);
        assert_eq!(row.next_attempt_at, None);
        assert_eq!(row.last_error.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn http_webhook_client_posts_event_payload() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let app = Router::new()
            .route(
                "/hook",
                post({
                    let tx = tx.clone();
                    move |headers: HeaderMap, Json(payload): Json<Value>| {
                        let tx = tx.clone();
                        async move {
                            tx.send(json!({
                                "token": headers.get("x-dgp-token").and_then(|v| v.to_str().ok()),
                                "payload": payload,
                            }))
                            .unwrap();
                            StatusCode::NO_CONTENT
                        }
                    }
                }),
            )
            .route(
                "/hook2",
                post(move |headers: HeaderMap, Json(payload): Json<Value>| {
                    let tx = tx.clone();
                    async move {
                        tx.send(json!({
                            "token": headers.get("x-dgp-token").and_then(|v| v.to_str().ok()),
                            "payload": payload,
                        }))
                        .unwrap();
                        StatusCode::NO_CONTENT
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let db = Arc::new(Mutex::new(ConfigDb::in_memory("test-pass").unwrap()));
        let id = {
            let db = db.lock().await;
            db.event_outbox_insert(&event("webhook")).unwrap()
        };
        let mut config = cfg();
        config.webhook_url = Some(format!("{base_url}/hook"));
        config.webhook_urls = vec![format!("{base_url}/hook2")];
        config
            .webhook_headers
            .insert("x-dgp-token".to_string(), "secret".to_string());
        let client = HttpWebhookDeliveryClient::for_tests();

        dispatch_once(&db, &client, &config, "test-worker", 200).await;

        let first = timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .expect("webhook request");
        let second = timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .expect("second webhook request");
        for payload in [first, second] {
            assert_eq!(payload["token"].as_str(), Some("secret"));
            let payload = &payload["payload"];
            assert_eq!(payload["schema"].as_str(), Some("deltaglider.event.v1"));
            assert_eq!(payload["event"]["id"].as_i64(), Some(id));
            assert_eq!(payload["event"]["kind"].as_str(), Some("ObjectCreated"));
            assert_eq!(payload["event"]["key"].as_str(), Some("webhook"));
        }

        let row = db
            .lock()
            .await
            .event_outbox_recent(10)
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert_eq!(row.status, STATUS_DELIVERED);
        assert_eq!(row.attempts, 1);

        server.abort();
    }

    #[tokio::test]
    async fn slack_webhook_delivery_formats_block_kit_and_filters() {
        // Mock Slack Incoming Webhook: capture the posted body, return 200.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let app = Router::new().route(
            "/services/T/B/X",
            post(move |Json(payload): Json<Value>| {
                let tx = tx.clone();
                async move {
                    tx.send(payload).unwrap();
                    StatusCode::OK
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let db = Arc::new(Mutex::new(ConfigDb::in_memory("test-pass").unwrap()));
        // One notifying event + one that the kind filter drops.
        let created = {
            let db = db.lock().await;
            let id = db
                .event_outbox_insert(&NewEvent::new(
                    EventKind::ObjectCreated,
                    "builds",
                    "ror/app.zip",
                    EventSource::S3Api,
                    1_700_000_000,
                    json!({ "content_length": 2048, "storage_type": "delta" }),
                ))
                .unwrap();
            // A delete — NOT in default notify_kinds → must be skipped (delivered, no POST).
            db.event_outbox_insert(&NewEvent::new(
                EventKind::ObjectDeleted,
                "builds",
                "ror/old.zip",
                EventSource::S3Api,
                1_700_000_001,
                json!({}),
            ))
            .unwrap();
            id
        };

        let mut config = cfg();
        config.format = EventDeliveryFormat::Slack;
        config.webhook_url = Some(format!("{base}/services/T/B/X"));
        config.webhook_urls = Vec::new();

        let client = HttpWebhookDeliveryClient::for_tests();
        dispatch_once(&db, &client, &config, "test-worker", 200).await;

        // Exactly ONE Slack POST (the ObjectCreated); the delete was filtered.
        let body = timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .expect("one slack message");
        assert!(
            rx.try_recv().is_err(),
            "filtered delete must NOT post to slack"
        );

        // Block Kit shape + text fallback.
        assert!(body["text"].as_str().unwrap().contains("New object"));
        assert!(body["text"]
            .as_str()
            .unwrap()
            .contains("builds/ror/app.zip"));
        let blocks = body["blocks"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "header");
        assert!(blocks[1]["text"]["text"]
            .as_str()
            .unwrap()
            .contains("`ror/app.zip`"));

        // Both rows end delivered (the created posted; the deleted was consumed).
        let rows = db.lock().await.event_outbox_recent(10).unwrap();
        for r in rows {
            assert_eq!(r.status, STATUS_DELIVERED, "row {} not delivered", r.id);
        }
        let _ = created;
        server.abort();
    }

    #[test]
    fn slack_web_api_ok_false_is_failure() {
        // The Slack Web API returns HTTP 200 even on error; the real status is
        // the JSON `ok` field. `slack_api_result` is the pure decision used by
        // the bot-token delivery path.
        assert!(slack_api_result(&json!({ "ok": true })).is_ok());
        let err =
            slack_api_result(&json!({ "ok": false, "error": "channel_not_found" })).unwrap_err();
        assert!(err.contains("channel_not_found"), "got: {err}");
        // Missing / malformed ok → failure with "unknown".
        let err2 = slack_api_result(&json!({})).unwrap_err();
        assert!(err2.contains("unknown"), "got: {err2}");
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let mut config = cfg();
        config.retry_base = "5s".to_string();
        config.retry_max = "12s".to_string();
        config.max_attempts = 10;
        assert_eq!(next_attempt_after(&config, 1, 100), Some(105));
        assert_eq!(next_attempt_after(&config, 2, 100), Some(110));
        assert_eq!(next_attempt_after(&config, 3, 100), Some(112));
    }
}
