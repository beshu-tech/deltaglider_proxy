// SPDX-License-Identifier: BUSL-1.1

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
    /// Deliver the whole event (Slack format: one formatted message).
    async fn deliver(
        &self,
        config: &EventDeliveryConfig,
        event: &EventOutboxRecord,
    ) -> Result<(), String>;

    /// Deliver the event to ONE target (a webhook URL or a Slack channel). The
    /// dispatcher calls it per target and records each outcome, so a retry
    /// skips the targets that already succeeded.
    async fn deliver_target(
        &self,
        config: &EventDeliveryConfig,
        event: &EventOutboxRecord,
        target: &DeliveryTarget,
    ) -> Result<(), String> {
        let _ = target;
        self.deliver(config, event).await
    }
}

/// One place an event is delivered to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryTarget {
    /// A raw or Slack Incoming Webhook URL.
    Webhook(String),
    /// A Slack channel (bot-token mode, `chat.postMessage`).
    SlackChannel(String),
}

impl DeliveryTarget {
    /// The `event_deliveries.endpoint_id` of this target.
    pub fn id(&self) -> String {
        match self {
            DeliveryTarget::Webhook(url) => endpoint_id(url),
            DeliveryTarget::SlackChannel(c) => endpoint_id(&format!("slack-channel:{c}")),
        }
    }
}

/// Pure: the targets `event` goes to under `config`. `Ok(empty)` means consume
/// without posting (a Slack filter or route did not match); `Err` is a config
/// fault that the row reports as its error.
pub fn delivery_targets(
    config: &EventDeliveryConfig,
    event: &EventOutboxRecord,
) -> Result<Vec<DeliveryTarget>, String> {
    let webhooks = || -> Vec<DeliveryTarget> {
        config
            .webhook_endpoints()
            .into_iter()
            .map(|u| DeliveryTarget::Webhook(u.to_string()))
            .collect()
    };
    match config.format {
        EventDeliveryFormat::Raw => {
            let t = webhooks();
            if t.is_empty() {
                return Err("event delivery enabled without webhook endpoint".to_string());
            }
            Ok(t)
        }
        EventDeliveryFormat::Slack => {
            let (include, exclude) = crate::slack_format::compile_slack_globs(config)?;
            if !crate::slack_format::should_notify(event, config, &include, &exclude) {
                return Ok(Vec::new());
            }
            if config.uses_slack_bot_token() {
                let channels = crate::slack_format::resolve_channels(event, config);
                if channels.is_empty() && config.slack_routes.is_empty() {
                    return Err("slack bot-token mode requires slack_channel".to_string());
                }
                return Ok(channels
                    .into_iter()
                    .map(DeliveryTarget::SlackChannel)
                    .collect());
            }
            let t = webhooks();
            if t.is_empty() {
                return Err("slack delivery enabled without a webhook URL or bot token".to_string());
            }
            Ok(t)
        }
    }
}

/// Stable, non-secret id of a webhook endpoint URL: the URL can carry a token
/// (Slack-style paths), so only a hash of it is stored.
pub fn endpoint_id(endpoint: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(&Sha256::digest(endpoint.trim().as_bytes())[..8])
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

/// Redact EVERY URL inside a free-text error (pure). Error sources we do not
/// format ourselves embed the full URL — reqwest's Display is
/// `error sending request for url (https://hooks.slack.com/services/…)` —
/// so the persistence boundary scrubs the whole message, not just the parts
/// we built. Each `scheme://…` run (up to whitespace or a closing delimiter)
/// goes through `redact_url_for_error`.
fn redact_urls_in_text(text: &str) -> String {
    let is_url_end = |c: char| c.is_whitespace() || matches!(c, ')' | '(' | '"' | '\'' | '<' | '>');
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(sep) = rest.find("://") {
        let scheme_start = rest[..sep]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
            .map(|i| i + 1)
            .unwrap_or(0);
        if scheme_start == sep {
            // "://" without a scheme: not a URL; copy through it.
            out.push_str(&rest[..sep + 3]);
            rest = &rest[sep + 3..];
            continue;
        }
        let end = rest[sep + 3..]
            .find(is_url_end)
            .map(|i| sep + 3 + i)
            .unwrap_or(rest.len());
        // A trailing ':' / ',' / ';' / '.' is punctuation, not URL.
        let url_end = scheme_start
            + rest[scheme_start..end]
                .trim_end_matches([':', ',', ';', '.'])
                .len();
        out.push_str(&rest[..scheme_start]);
        out.push_str(&redact_url_for_error(&rest[scheme_start..url_end]));
        rest = &rest[url_end..];
    }
    out.push_str(rest);
    out
}

/// The one shape every delivery error takes before it is persisted.
fn persistable_error(error: &str) -> String {
    truncate_error(&redact_urls_in_text(error))
}

#[async_trait]
impl EventDeliveryClient for HttpWebhookDeliveryClient {
    /// Direct delivery to every target (the dispatcher goes target by target).
    async fn deliver(
        &self,
        config: &EventDeliveryConfig,
        event: &EventOutboxRecord,
    ) -> Result<(), String> {
        let mut errors = Vec::new();
        for target in delivery_targets(config, event)? {
            if let Err(e) = self.deliver_target(config, event, &target).await {
                errors.push(e);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    async fn deliver_target(
        &self,
        config: &EventDeliveryConfig,
        event: &EventOutboxRecord,
        target: &DeliveryTarget,
    ) -> Result<(), String> {
        let timeout = parse_duration_or(
            &config.request_timeout,
            DEFAULT_TIMEOUT,
            MIN_TIMEOUT,
            "event_delivery.request_timeout",
        );
        match (config.format, target) {
            (EventDeliveryFormat::Raw, DeliveryTarget::Webhook(url)) => {
                self.post_raw(config, event, url, timeout).await
            }
            (EventDeliveryFormat::Slack, DeliveryTarget::Webhook(url)) => {
                self.post_slack_webhook(config, event, url, timeout).await
            }
            (_, DeliveryTarget::SlackChannel(channel)) => {
                self.post_slack_channel(config, event, channel, timeout)
                    .await
            }
        }
    }
}

impl HttpWebhookDeliveryClient {
    /// POST the `{schema,event}` envelope to one endpoint with the configured
    /// static headers.
    async fn post_raw(
        &self,
        config: &EventDeliveryConfig,
        event: &EventOutboxRecord,
        endpoint: &str,
        timeout: Duration,
    ) -> Result<(), String> {
        let payload = EventWebhookPayload {
            schema: "deltaglider.event.v1",
            event,
        };
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
            .map_err(|e| format!("{}: {}", redact_url_for_error(endpoint), e.without_url()))?;
        if !response.status().is_success() {
            return Err(format!(
                "{}: webhook returned HTTP {}",
                redact_url_for_error(endpoint),
                response.status()
            ));
        }
        Ok(())
    }

    /// Slack Web API `chat.postMessage` to one channel. Slack answers HTTP 200
    /// even on error; the real status is the JSON `{ "ok": bool, "error": ... }`.
    async fn post_slack_channel(
        &self,
        config: &EventDeliveryConfig,
        event: &EventOutboxRecord,
        channel: &str,
        timeout: Duration,
    ) -> Result<(), String> {
        let token = config
            .slack_bot_token
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_string();
        let mut msg = crate::slack_format::slack_message(event, config);
        if let Value::Object(ref mut map) = msg {
            map.insert("channel".to_string(), Value::String(channel.to_string()));
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
        result.map_err(|e| {
            warn!("slack chat.postMessage to {channel} failed: {e}");
            format!("{channel}: {e}")
        })
    }

    /// POST `{text, blocks, username?, icon_emoji?}` to one Slack Incoming
    /// Webhook URL. 2xx = delivered.
    async fn post_slack_webhook(
        &self,
        config: &EventDeliveryConfig,
        event: &EventOutboxRecord,
        endpoint: &str,
        timeout: Duration,
    ) -> Result<(), String> {
        let mut body = crate::slack_format::slack_message(event, config);
        if let Value::Object(ref mut map) = body {
            if let Some(u) = config.slack_username.as_deref().filter(|s| !s.is_empty()) {
                map.insert("username".to_string(), Value::String(u.to_string()));
            }
            if let Some(i) = config.slack_icon_emoji.as_deref().filter(|s| !s.is_empty()) {
                map.insert("icon_emoji".to_string(), Value::String(i.to_string()));
            }
        }
        self.check_ssrf(endpoint, "slack webhook URL")?;
        let url = Url::parse(endpoint).map_err(|e| format!("invalid slack webhook URL: {e}"))?;
        let response = self
            .client
            .post(url)
            .timeout(timeout)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("{}: {}", redact_url_for_error(endpoint), e.without_url()))?;
        if !response.status().is_success() {
            return Err(format!(
                "{}: slack webhook returned HTTP {}",
                redact_url_for_error(endpoint),
                response.status()
            ));
        }
        Ok(())
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
            let tick = dispatcher_tick(&config.read().await.event_delivery);
            tokio::time::sleep(tick).await;
            // Read the config AFTER the sleep: a snapshot from before it could
            // prune events written after delivery was turned on.
            let cfg = { config.read().await.event_delivery.clone() };
            if !cfg.is_active() {
                // Delivery off: no dispatch, but still bound the outbox (X-ray
                // H14, issue #92). See `prune_while_inactive`.
                let replication_enabled = { config.read().await.replication.enabled };
                prune_while_inactive(&db, &cfg, replication_enabled, current_unix_seconds()).await;
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

/// Upper bound on prune batches per tick while delivery is off, so one tick
/// never holds the config-DB lock for long.
const INACTIVE_PRUNE_MAX_BATCHES: u32 = 50;

/// The highest outbox id nobody still needs while delivery is off, or `None`
/// to delete nothing. Pure; unit-tested.
///
/// While delivery is off the only other reader is event-driven replication.
/// * Replication enabled: rows at or below its ACTIVE cursor are consumed. With
///   no active cursor (an idle consumer ages out after an hour) keep
///   everything: the next event must not be pruned before replication reads it.
/// * Replication disabled: rows at or below a still-active cursor are kept for
///   a quick re-enable to replay; with no active cursor, NO reader exists, so
///   every row goes. Without this the outbox of a default install (no delivery,
///   no replication) grew by one row per write, forever. A replication consumer
///   enabled later seeds its cursor at the newest id and never reads older rows.
pub(crate) fn inactive_prune_floor(
    replication_enabled: bool,
    active_cursor: Option<i64>,
    max_id: Option<i64>,
) -> Option<i64> {
    if replication_enabled {
        active_cursor
    } else {
        active_cursor.or(max_id)
    }
}

/// Prune the outbox while delivery is off (see [`inactive_prune_floor`]).
/// Events are therefore not kept for a later enable of delivery: the UI says so.
pub async fn prune_while_inactive(
    db: &Arc<Mutex<ConfigDb>>,
    config: &EventDeliveryConfig,
    replication_enabled: bool,
    now: i64,
) {
    let batch = config.prune_batch.max(1);
    for _ in 0..INACTIVE_PRUNE_MAX_BATCHES {
        let db = db.lock().await;
        let active = db
            .event_outbox_min_active_listener_cursor(now, LISTENER_CURSOR_STALE_SECS)
            .unwrap_or(None);
        let max_id = db.event_outbox_max_id().unwrap_or(None);
        let Some(floor) = inactive_prune_floor(replication_enabled, active, max_id) else {
            return;
        };
        match db.event_outbox_prune_below_floor(floor, batch) {
            Ok(deleted) if (deleted as u32) < batch => return,
            Ok(_) => {}
            Err(err) => {
                warn!("Event outbox floor prune failed: {}", err);
                return;
            }
        }
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
        let outcome = deliver_to_targets(db, client, config, &event).await;
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
                    db.event_outbox_mark_failed(event.id, &persistable_error(&err), next_attempt_at)
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
    }
    // NOTE: no any-status floor prune here. When delivery is ACTIVE every
    // claimed row ends `delivered` or `failed` (a filtered-out event is marked
    // delivered — see `deliver_slack`'s "consume without posting"), so nothing
    // stays consumed-but-pending on this path; the delivered/failed prunes above
    // bound the outbox. An any-status prune here would delete rows still
    // `pending`/retrying below the replication floor (target down) BEFORE webhook
    // delivery resolves them — breaking the at-least-once contract. The
    // consumed-but-never-delivered accumulation the floor prune guards against
    // only arises when delivery is DISABLED, and the disabled branch of
    // `spawn_dispatcher_with_client` handles that via `prune_while_inactive`.
}

/// Fan-out: deliver to every target that has not yet received this event,
/// record each outcome, and fail the row (for a retry) when any target failed.
/// One failing target never stops the others, and a retry never re-posts to a
/// target that already succeeded (Slack `chat.postMessage` has no idempotency
/// key, so that would be a duplicate message).
async fn deliver_to_targets(
    db: &Arc<Mutex<ConfigDb>>,
    client: &dyn EventDeliveryClient,
    config: &EventDeliveryConfig,
    event: &EventOutboxRecord,
) -> Result<(), String> {
    let targets = delivery_targets(config, event)?;
    if targets.is_empty() {
        return Ok(()); // filtered out or routed nowhere: consumed
    }
    let mut done = db
        .lock()
        .await
        .event_delivery_done_endpoints(event.id)
        .map_err(|e| format!("event delivery state unreadable: {e}"))?;
    let mut errors = Vec::new();
    for target in targets {
        let id = target.id();
        if done.contains(&id) {
            continue;
        }
        let outcome = client.deliver_target(config, event, &target).await;
        let error = outcome.as_ref().err().map(|e| persistable_error(e));
        if let Err(e) = db.lock().await.event_delivery_record(
            event.id,
            &id,
            error.as_deref(),
            current_unix_seconds(),
        ) {
            // Unrecorded success: the retry may post to this target again
            // (at-least-once), never skip it.
            warn!("Event delivery record failed for {}: {}", event.id, e);
        }
        match error {
            Some(e) => errors.push(e),
            None => {
                done.insert(id);
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
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

    #[test]
    fn inactive_prune_floor_truth_table() {
        // Replication on: only what its active cursor consumed; idle → keep all.
        assert_eq!(inactive_prune_floor(true, Some(5), Some(9)), Some(5));
        assert_eq!(inactive_prune_floor(true, None, Some(9)), None);
        // Replication off: keep rows a still-active cursor would replay …
        assert_eq!(inactive_prune_floor(false, Some(5), Some(9)), Some(5));
        // … and with no reader at all, everything goes.
        assert_eq!(inactive_prune_floor(false, None, Some(9)), Some(9));
        assert_eq!(inactive_prune_floor(false, None, None), None);
    }

    /// Issue #92: with delivery AND replication off, the outbox kept every
    /// write forever. It must now drain; with replication on (idle consumer,
    /// no active cursor) nothing may be pruned.
    #[tokio::test]
    async fn prune_while_inactive_drains_outbox_without_readers() {
        let mut off = cfg();
        off.enabled = false;
        off.prune_batch = 2; // forces several batches in one tick
        let db = Arc::new(Mutex::new(ConfigDb::in_memory("test-pass").unwrap()));
        {
            let db = db.lock().await;
            for i in 0..5 {
                db.event_outbox_insert(&event(&format!("k{i}"))).unwrap();
            }
        }
        let now = current_unix_seconds();
        prune_while_inactive(&db, &off, true, now).await;
        assert_eq!(db.lock().await.event_outbox_count(None).unwrap(), 5);

        prune_while_inactive(&db, &off, false, now).await;
        assert_eq!(db.lock().await.event_outbox_count(None).unwrap(), 0);

        // A live replication cursor still protects the rows after it.
        {
            let db = db.lock().await;
            let first = db.event_outbox_insert(&event("a")).unwrap();
            db.event_outbox_insert(&event("b")).unwrap();
            db.listener_cursor_advance("replication", first, now)
                .unwrap();
        }
        prune_while_inactive(&db, &off, true, now).await;
        assert_eq!(db.lock().await.event_outbox_count(None).unwrap(), 1);
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

    /// Regression (at-least-once): a row still awaiting webhook delivery must
    /// survive a dispatch pass even when the replication listener has already
    /// consumed past it. Previously an any-status floor prune in the active
    /// path deleted such rows below the replication cursor, dropping the event
    /// before delivery ever succeeded.
    #[tokio::test]
    async fn dispatch_keeps_undelivered_row_below_replication_floor() {
        let db = Arc::new(Mutex::new(ConfigDb::in_memory("test-pass").unwrap()));
        let id = {
            let db = db.lock().await;
            let id = db.event_outbox_insert(&event("down")).unwrap();
            // Replication (a faster listener) has consumed through this row.
            db.listener_cursor_advance("replication", id, 200).unwrap();
            id
        };
        // Delivery target is down: every attempt fails, so after one pass the
        // row is PENDING/retrying (max_attempts=2 → attempt 1 reschedules).
        let client = FakeClient {
            failures_before_success: 99,
            calls: AtomicUsize::new(0),
        };

        dispatch_once(&db, &client, &cfg(), "test-worker", 200).await;

        let rows = db.lock().await.event_outbox_recent(10).unwrap();
        let row = rows
            .iter()
            .find(|r| r.id == id)
            .expect("undelivered row must NOT be pruned below the replication floor");
        assert_eq!(row.status, STATUS_PENDING);
        assert!(row.next_attempt_at.is_some(), "row is still owed a retry");
    }

    /// Records every per-endpoint post; the endpoint named `fail_url` fails
    /// its first `fail_times` posts.
    struct EndpointClient {
        fail_url: &'static str,
        fail_times: usize,
        failed: AtomicUsize,
        posts: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl EventDeliveryClient for EndpointClient {
        async fn deliver(
            &self,
            _config: &EventDeliveryConfig,
            _event: &EventOutboxRecord,
        ) -> Result<(), String> {
            unreachable!("delivery must go target by target")
        }

        async fn deliver_target(
            &self,
            _config: &EventDeliveryConfig,
            _event: &EventOutboxRecord,
            target: &DeliveryTarget,
        ) -> Result<(), String> {
            let endpoint = match target {
                DeliveryTarget::Webhook(u) => u.as_str(),
                DeliveryTarget::SlackChannel(c) => c.as_str(),
            };
            self.posts.lock().unwrap().push(endpoint.to_string());
            if endpoint == self.fail_url
                && self.failed.fetch_add(1, Ordering::SeqCst) < self.fail_times
            {
                Err("HTTP 503".to_string())
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn a_retry_posts_only_to_endpoints_that_have_not_succeeded() {
        let db = Arc::new(Mutex::new(ConfigDb::in_memory("test-pass").unwrap()));
        let id = db.lock().await.event_outbox_insert(&event("fan")).unwrap();
        let client = EndpointClient {
            fail_url: "http://b.invalid/hook",
            fail_times: 1,
            failed: AtomicUsize::new(0),
            posts: Default::default(),
        };
        let config = EventDeliveryConfig {
            webhook_url: Some("http://a.invalid/hook".into()),
            webhook_urls: vec![
                "http://b.invalid/hook".into(),
                "http://c.invalid/hook".into(),
            ],
            ..cfg()
        };

        dispatch_once(&db, &client, &config, "w", 200).await;
        // B failing does not stop C.
        assert_eq!(
            *client.posts.lock().unwrap(),
            vec![
                "http://a.invalid/hook",
                "http://b.invalid/hook",
                "http://c.invalid/hook"
            ]
        );
        let deliveries = db.lock().await.event_deliveries_for(id).unwrap();
        let status = |url: &str| {
            deliveries
                .iter()
                .find(|d| d.endpoint_id == endpoint_id(url))
                .map(|d| (d.status.clone(), d.attempts))
        };
        assert_eq!(
            status("http://a.invalid/hook"),
            Some(("delivered".into(), 1))
        );
        assert_eq!(status("http://b.invalid/hook"), Some(("failed".into(), 1)));
        let row = db.lock().await.event_outbox_load(id).unwrap().unwrap();
        assert_eq!(row.status, STATUS_PENDING);
        assert_eq!(row.last_error.as_deref(), Some("HTTP 503"));

        // The retry posts to B only.
        client.posts.lock().unwrap().clear();
        dispatch_once(&db, &client, &config, "w", 205).await;
        assert_eq!(*client.posts.lock().unwrap(), vec!["http://b.invalid/hook"]);
        let row = db.lock().await.event_outbox_load(id).unwrap().unwrap();
        assert_eq!(row.status, STATUS_DELIVERED);
        let deliveries = db.lock().await.event_deliveries_for(id).unwrap();
        assert!(deliveries.iter().all(|d| d.status == "delivered"));
        assert_eq!(
            deliveries
                .iter()
                .find(|d| d.endpoint_id == endpoint_id("http://b.invalid/hook"))
                .unwrap()
                .attempts,
            2
        );
    }

    fn slack_event() -> NewEvent {
        NewEvent::new(
            EventKind::ObjectCreated,
            "builds",
            "ror/app.zip",
            EventSource::S3Api,
            100,
            json!({ "content_length": 1 }),
        )
    }

    /// Two dispatches: B fails once. Returns (first posts, retry posts).
    async fn fan_out_twice(
        config: EventDeliveryConfig,
        fail: &'static str,
    ) -> (Vec<String>, Vec<String>) {
        let db = Arc::new(Mutex::new(ConfigDb::in_memory("test-pass").unwrap()));
        let id = db.lock().await.event_outbox_insert(&slack_event()).unwrap();
        let client = EndpointClient {
            fail_url: fail,
            fail_times: 1,
            failed: AtomicUsize::new(0),
            posts: Default::default(),
        };
        dispatch_once(&db, &client, &config, "w", 200).await;
        let first = std::mem::take(&mut *client.posts.lock().unwrap());
        let row = db.lock().await.event_outbox_load(id).unwrap().unwrap();
        assert_eq!(
            row.status, STATUS_PENDING,
            "a failed target retries the row"
        );
        dispatch_once(&db, &client, &config, "w", 205).await;
        let retry = client.posts.lock().unwrap().clone();
        let row = db.lock().await.event_outbox_load(id).unwrap().unwrap();
        assert_eq!(row.status, STATUS_DELIVERED);
        (first, retry)
    }

    #[tokio::test]
    async fn slack_webhook_retry_posts_only_to_the_failed_url() {
        let config = EventDeliveryConfig {
            format: EventDeliveryFormat::Slack,
            webhook_url: Some("https://hooks.slack.test/a".into()),
            webhook_urls: vec!["https://hooks.slack.test/b".into()],
            slack_bot_token: None,
            ..cfg()
        };
        let (first, retry) = fan_out_twice(config, "https://hooks.slack.test/b").await;
        assert_eq!(
            first,
            vec!["https://hooks.slack.test/a", "https://hooks.slack.test/b"]
        );
        assert_eq!(retry, vec!["https://hooks.slack.test/b"]);
    }

    #[tokio::test]
    async fn slack_channel_retry_posts_only_to_the_failed_channel() {
        let route = |c: &str| crate::config_sections::SlackRoute {
            name: None,
            bucket: None,
            prefix_globs: vec![],
            channel: c.into(),
        };
        let config = EventDeliveryConfig {
            format: EventDeliveryFormat::Slack,
            webhook_url: None,
            slack_bot_token: Some("xoxb-test".into()),
            slack_routes: vec![route("C1"), route("C2")],
            ..cfg()
        };
        let (first, retry) = fan_out_twice(config, "C2").await;
        assert_eq!(first, vec!["C1", "C2"]);
        assert_eq!(retry, vec!["C2"], "C1 must not get the message twice");
    }

    #[test]
    fn delivery_targets_truth_table() {
        let rec = |kind: EventKind| EventOutboxRecord {
            id: 1,
            kind: kind.as_str().to_string(),
            bucket: "builds".into(),
            key: "ror/app.zip".into(),
            source: "s3".into(),
            occurred_at: 0,
            payload: json!({}),
            status: STATUS_PENDING.into(),
            attempts: 1,
            next_attempt_at: None,
            claimed_by: None,
            claimed_at: None,
            delivered_at: None,
            last_error: None,
            created_at: 0,
        };
        let slack = EventDeliveryConfig {
            format: EventDeliveryFormat::Slack,
            ..cfg()
        };
        // A filtered-out kind is consumed without posting.
        assert_eq!(
            delivery_targets(&slack, &rec(EventKind::ObjectDeleted)),
            Ok(vec![])
        );
        assert_eq!(
            delivery_targets(&slack, &rec(EventKind::ObjectCreated)),
            Ok(vec![DeliveryTarget::Webhook(
                "http://example.invalid/hook".into()
            )])
        );
        // Bot token without a channel is a config error; routed-but-unmatched is not.
        let bot = EventDeliveryConfig {
            slack_bot_token: Some("xoxb".into()),
            ..slack.clone()
        };
        assert!(delivery_targets(&bot, &rec(EventKind::ObjectCreated)).is_err());
        let raw = EventDeliveryConfig {
            webhook_url: None,
            ..cfg()
        };
        assert!(delivery_targets(&raw, &rec(EventKind::ObjectCreated)).is_err());
        // Channel and URL ids never collide.
        assert_ne!(
            DeliveryTarget::SlackChannel("x".into()).id(),
            DeliveryTarget::Webhook("x".into()).id()
        );
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

    /// S17: a Slack incoming-webhook URL is a bearer secret. A transport error
    /// (reqwest Display embeds the full URL) must not carry it into the
    /// persisted `last_error`, for raw and Slack formats alike.
    #[tokio::test]
    async fn transport_error_does_not_persist_the_webhook_secret() {
        for format in [EventDeliveryFormat::Raw, EventDeliveryFormat::Slack] {
            let db = Arc::new(Mutex::new(ConfigDb::in_memory("test-pass").unwrap()));
            db.lock()
                .await
                .event_outbox_insert(&event("ror/app.zip"))
                .unwrap();
            let mut config = cfg();
            config.format = format;
            // Port 1: connection refused → a reqwest transport error.
            config.webhook_url = Some("http://127.0.0.1:1/services/T0/B0/SECRETTOKEN".into());
            config.webhook_urls = Vec::new();
            dispatch_once(
                &db,
                &HttpWebhookDeliveryClient::for_tests(),
                &config,
                "w",
                200,
            )
            .await;
            let rows = db.lock().await.event_outbox_recent(10).unwrap();
            let err = rows[0].last_error.clone().expect("delivery must fail");
            assert!(
                !err.contains("SECRETTOKEN"),
                "{format:?}: webhook secret persisted in last_error: {err}"
            );
        }
    }

    #[test]
    fn redact_urls_in_text_scrubs_every_url() {
        let msg = "error sending request for url (https://hooks.slack.com/services/T/B/X?q=1): \
                   refused; also http://h:8080/p";
        let out = redact_urls_in_text(msg);
        assert!(!out.contains("/services/"), "{out}");
        assert!(!out.contains("/p"), "{out}");
        assert!(out.contains("https://hooks.slack.com/<redacted>"), "{out}");
        assert!(out.contains("http://h:8080/<redacted>"), "{out}");
        assert_eq!(redact_urls_in_text("no url here"), "no url here");
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
