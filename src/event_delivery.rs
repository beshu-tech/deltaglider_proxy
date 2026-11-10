// SPDX-License-Identifier: BUSL-1.1

//! Background delivery for the durable event outbox.
//!
//! The dispatcher is intentionally conservative: it is disabled unless
//! `advanced.event_delivery.enabled=true` and a delivery target is set (a
//! webhook URL, or — in `format = slack` bot-token mode — a Slack bot token).
//! Request handlers never call this module; they only append to `event_outbox`.

use crate::background::parse_duration_or;
use crate::config::SharedConfig;
use crate::config_db::JobsDb;
use crate::config_sections::{EventDeliveryConfig, EventDeliveryFormat};
use crate::event_outbox::{
    current_unix_seconds, EndpointOutcome, EventOutboxRecord, Settlement, STATUS_DELIVERED,
    STATUS_FAILED, STATUS_IN_PROGRESS, STATUS_PENDING,
};
use crate::security::{validate_outbound_url, UrlKind};
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::Url;
use serde::Serialize;
use serde_json::Value;
use std::future::Future;
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

#[dynosaur::dynosaur(pub DynEventDeliveryClient = dyn(box) EventDeliveryClient)]
pub trait EventDeliveryClient: Send + Sync {
    /// Deliver the whole event (Slack format: one formatted message).
    fn deliver(
        &self,
        config: &EventDeliveryConfig,
        event: &EventOutboxRecord,
    ) -> impl Future<Output = Result<(), String>> + Send;

    /// Deliver the event to ONE target (a webhook URL or a Slack channel). The
    /// dispatcher calls it per target and records each outcome, so a retry
    /// skips the targets that already succeeded.
    fn deliver_target(
        &self,
        config: &EventDeliveryConfig,
        event: &EventOutboxRecord,
        target: &DeliveryTarget,
    ) -> impl Future<Output = Result<(), String>> + Send {
        async move {
            let _ = target;
            self.deliver(config, event).await
        }
    }
}

/// One place an event is delivered to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryTarget {
    /// A raw or Slack Incoming Webhook URL, at position `slot` of the
    /// configured endpoints.
    Webhook { url: String, slot: usize },
    /// A Slack channel (bot-token mode, `chat.postMessage`).
    SlackChannel(String),
}

impl DeliveryTarget {
    /// The `event_deliveries.endpoint_id` of this target. A webhook is the
    /// configured endpoint at its position and origin, not its full URL: a
    /// rotated token in the path or query is the same endpoint, and keeps
    /// its delivered state (a retry does not post to it again). A removal
    /// that moves another host into the position is a new endpoint.
    pub fn id(&self) -> String {
        match self {
            DeliveryTarget::Webhook { url, slot } => {
                endpoint_id(&format!("webhook:{slot}:{}", url_origin(url)))
            }
            DeliveryTarget::SlackChannel(c) => endpoint_id(&format!("slack-channel:{c}")),
        }
    }

    /// The id rows written before the position-keyed id carry (the URL
    /// hash): still honoured, so an upgrade does not re-post.
    fn legacy_id(&self) -> Option<String> {
        match self {
            DeliveryTarget::Webhook { url, .. } => Some(endpoint_id(url)),
            DeliveryTarget::SlackChannel(_) => None,
        }
    }
}

/// `scheme://host[:port]` of a URL (no path, query or credentials), or the
/// trimmed input when it does not parse.
fn url_origin(raw: &str) -> String {
    match Url::parse(raw.trim()) {
        Ok(u) => u.origin().ascii_serialization(),
        Err(_) => raw.trim().to_string(),
    }
}

/// The webhook endpoints as targets. The same URL listed twice is one
/// endpoint (posted once).
fn webhook_targets(config: &EventDeliveryConfig) -> Vec<DeliveryTarget> {
    let mut seen = std::collections::HashSet::new();
    config
        .webhook_endpoints()
        .into_iter()
        .enumerate()
        .filter(|(_, u)| seen.insert(endpoint_id(u)))
        .map(|(slot, u)| DeliveryTarget::Webhook {
            url: u.to_string(),
            slot,
        })
        .collect()
}

/// Pure: the targets `event` goes to under `config`. `Ok(empty)` means consume
/// without posting (a Slack filter or route did not match); `Err` is a config
/// fault that the row reports as its error.
pub fn delivery_targets(
    config: &EventDeliveryConfig,
    event: &EventOutboxRecord,
) -> Result<Vec<DeliveryTarget>, String> {
    let webhooks = || webhook_targets(config);
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

/// Pure: a readable, non-secret label for every target the current config can
/// deliver to, keyed by its `event_deliveries.endpoint_id`. URLs are redacted
/// (a path can carry a token); the position tells two URLs on one host apart.
/// An id missing from the map is an endpoint removed from the config.
pub fn endpoint_labels(config: &EventDeliveryConfig) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for target in webhook_targets(config) {
        if let DeliveryTarget::Webhook { url, slot } = &target {
            let label = format!("webhook {}: {}", slot + 1, redact_url_for_error(url));
            if let Some(legacy) = target.legacy_id() {
                out.insert(legacy, label.clone());
            }
            out.insert(target.id(), label);
        }
    }
    let channels = config
        .slack_channel
        .iter()
        .map(String::as_str)
        .chain(config.slack_routes.iter().map(|r| r.channel.as_str()))
        .map(str::trim)
        .filter(|c| !c.is_empty());
    for c in channels {
        out.insert(
            DeliveryTarget::SlackChannel(c.to_string()).id(),
            format!("slack channel {c}"),
        );
    }
    out
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
    /// The same client for `event_delivery.allow_local: true`: its resolver
    /// allows private addresses (metadata stays refused).
    local_client: reqwest::Client,
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
        // Close the DNS-rebinding gap: a webhook hostname that resolves to a
        // metadata/private address fails closed at connect time (the
        // literal-IP validate_outbound_url check can't see DNS).
        let build = |kind| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .dns_resolver(std::sync::Arc::new(
                    crate::security::SsrfGuardedResolver::new(kind),
                ))
                .build()
                .unwrap_or_default()
        };
        Self {
            client: build(UrlKind::Webhook),
            local_client: build(UrlKind::BackendDev),
            skip_ssrf_check: false,
        }
    }
}

impl HttpWebhookDeliveryClient {
    /// Validate an operator-supplied delivery URL unless SSRF checks are
    /// disabled (tests only). Centralises the guard so both the raw-webhook and
    /// slack-incoming-webhook paths apply it identically.
    /// A refusal is permanent: the URL is the same on every retry.
    fn check_ssrf(
        &self,
        config: &EventDeliveryConfig,
        endpoint: &str,
        what: &str,
    ) -> Result<(), String> {
        if self.skip_ssrf_check {
            return Ok(());
        }
        validate_outbound_url(endpoint, config.webhook_url_kind()).map_err(|e| {
            permanent(format!(
                "{what} rejected: {e}. Set event_delivery.allow_local: true to allow \
                 http:// and private addresses"
            ))
        })
    }

    /// The HTTP client for `config`'s URL policy.
    fn http(&self, config: &EventDeliveryConfig) -> &reqwest::Client {
        if config.allow_local {
            &self.local_client
        } else {
            &self.client
        }
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
        if end == sep + 3 {
            // "https://" with nothing after it is advice text ("use
            // https://"), not a URL that can hold a secret: copy it through.
            out.push_str(&rest[..end]);
            rest = &rest[end..];
            continue;
        }
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

/// Prefix of a delivery error that no retry can fix (an SSRF refusal, an
/// invalid URL or header, a config fault). A row whose every failed target
/// failed this way is dead-lettered at once instead of retried.
const PERMANENT: &str = "[permanent] ";

fn permanent(msg: String) -> String {
    format!("{PERMANENT}{msg}")
}

/// Pure: does every part of a (`"; "`-joined) delivery error say permanent?
pub(crate) fn is_permanent_error(error: &str) -> bool {
    !error.is_empty() && error.split("; ").all(|e| e.starts_with(PERMANENT))
}

/// The newest delivery outcome on this node (success, or the error), for the
/// admin "Failing" status. In memory: a restart shows "Active" until the next
/// delivery says otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastOutcome {
    pub ok: bool,
    pub error: Option<String>,
    pub at: i64,
}

static LAST_OUTCOME: parking_lot::Mutex<Option<LastOutcome>> = parking_lot::Mutex::new(None);

pub fn last_outcome() -> Option<LastOutcome> {
    LAST_OUTCOME.lock().clone()
}

fn record_outcome(error: Option<&str>, at: i64) {
    *LAST_OUTCOME.lock() = Some(LastOutcome {
        ok: error.is_none(),
        error: error.map(str::to_string),
        at,
    });
}

/// What the admin GUI shows for event delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeliveryState {
    Disabled,
    /// Enabled, but no webhook URL or bot token: nothing is sent.
    NoEndpoint,
    Active,
    /// Active, but the newest delivery failed.
    Failing,
}

/// Pure: the delivery state for `config` and the newest outcome.
pub fn delivery_state(config: &EventDeliveryConfig, last: Option<&LastOutcome>) -> DeliveryState {
    if !config.enabled {
        DeliveryState::Disabled
    } else if !config.is_active() {
        DeliveryState::NoEndpoint
    } else if last.is_some_and(|o| !o.ok) {
        DeliveryState::Failing
    } else {
        DeliveryState::Active
    }
}

/// The one shape every delivery error takes before it is persisted.
fn persistable_error(error: &str) -> String {
    truncate_error(&redact_urls_in_text(error))
}

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
            (EventDeliveryFormat::Raw, DeliveryTarget::Webhook { url, .. }) => {
                self.post_raw(config, event, url, timeout).await
            }
            (EventDeliveryFormat::Slack, DeliveryTarget::Webhook { url, .. }) => {
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
        self.check_ssrf(config, endpoint, "webhook endpoint")?;
        let url = Url::parse(endpoint)
            .map_err(|e| permanent(format!("invalid webhook endpoint: {e}")))?;
        let mut request = self
            .http(config)
            .post(url)
            .timeout(timeout)
            .header("user-agent", "deltaglider-proxy-event-outbox");
        for (name, value) in &config.webhook_headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| permanent(format!("invalid webhook header name {name:?}: {e}")))?;
            let value = HeaderValue::from_str(value)
                .map_err(|e| permanent(format!("invalid webhook header value for {name}: {e}")))?;
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
        self.check_ssrf(config, endpoint, "slack webhook URL")?;
        let url = Url::parse(endpoint)
            .map_err(|e| permanent(format!("invalid slack webhook URL: {e}")))?;
        let response = self
            .http(config)
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
    db: Arc<Mutex<JobsDb>>,
) -> tokio::task::JoinHandle<()> {
    spawn_dispatcher_with_client(
        config,
        db,
        DynEventDeliveryClient::new_arc(HttpWebhookDeliveryClient::default()),
    )
}

pub fn spawn_dispatcher_with_client(
    config: SharedConfig,
    db: Arc<Mutex<JobsDb>>,
    client: Arc<DynEventDeliveryClient<'static>>,
) -> tokio::task::JoinHandle<()> {
    let claimant = format!("event-delivery:{}", uuid::Uuid::new_v4());
    crate::background::spawn_supervised("event-dispatcher", move || {
        let (config, db, client, claimant) =
            (config.clone(), db.clone(), client.clone(), claimant.clone());
        async move {
            info!("Event outbox dispatcher started: claimant={}", claimant);
            // Boot: does anything read the outbox? Until this runs, every
            // append is kept (the gate starts open).
            crate::event_outbox::init_readers(&db, &*config.read().await).await;
            loop {
                let tick = dispatcher_tick(&config.read().await.event_delivery);
                tokio::time::sleep(tick).await;
                // Read the config AFTER the sleep: a snapshot from before it
                // could prune events written after delivery was turned on.
                let cfg = { config.read().await.event_delivery.clone() };
                if !cfg.is_active() {
                    // Delivery off: no dispatch, but still bound the outbox
                    // (X-ray H14, issue #92). See `prune_while_inactive`.
                    let replication_enabled = { config.read().await.replication.enabled };
                    prune_while_inactive(&db, &cfg, replication_enabled, current_unix_seconds())
                        .await;
                    continue;
                }
                drain_outbox(
                    &db,
                    client.as_ref(),
                    &cfg,
                    &claimant,
                    drain_budget(&cfg),
                    current_unix_seconds,
                )
                .await;
            }
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

/// Prune the outbox while delivery is off (see `inactive_prune_floor`).
/// Events are therefore not kept for a later enable of delivery: the UI says so.
pub async fn prune_while_inactive(
    db: &Arc<Mutex<JobsDb>>,
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

/// What one claimed-and-settled batch did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct BatchReport {
    /// Rows the claim returned.
    pub claimed: usize,
    /// Rows that ended `failed` (a retry or dead) in this batch.
    pub failed: usize,
}

/// What a whole drain did (see [`drain_outbox`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrainReport {
    pub batches: usize,
    pub claimed: usize,
    pub failed: usize,
}

/// Raw webhook posts that run at once in one batch. A bound, so a slow
/// target cannot make the proxy open an unbounded number of connections.
const DELIVERY_CONCURRENCY: usize = 8;

/// Pure: how many posts of one batch run at once. A Slack target takes one at
/// a time: `chat.postMessage` and Incoming Webhooks answer about one message
/// per second per channel with 429, and posts in order read in order.
pub(crate) fn delivery_concurrency(config: &EventDeliveryConfig) -> usize {
    match config.format {
        EventDeliveryFormat::Slack => 1,
        _ => DELIVERY_CONCURRENCY,
    }
}

/// Pure: rows one claim takes.
pub(crate) fn claim_limit(config: &EventDeliveryConfig) -> u32 {
    config.batch_size.clamp(1, 500)
}

/// Time one drain may spend claiming new batches: one tick. The sleep
/// before the next drain adds a tick, so a drain never takes the whole
/// loop.
pub(crate) fn drain_budget(config: &EventDeliveryConfig) -> Duration {
    dispatcher_tick(config)
}

/// Pure: whether a drain claims another batch. Yes only when the last claim
/// was FULL (there is probably more), most of its rows were delivered (a
/// target that fails most rows is not asked again at once: its rows wait for
/// their retry time, and the posts of the others would only time out too),
/// and the time budget has room.
pub(crate) fn drain_continues(
    batch_limit: u32,
    last: &BatchReport,
    elapsed: Duration,
    budget: Duration,
) -> bool {
    last.claimed >= batch_limit as usize && last.failed * 2 < last.claimed && elapsed < budget
}

/// One dispatcher tick: claim and deliver batches until the outbox has no
/// due row, a batch comes back mostly failed, or `budget` is spent, then
/// prune. Before, a tick took ONE batch (50 rows per 10 s tick): at more than
/// 5 events per second the backlog grew without bound.
///
/// Delivery stays at-least-once: every batch is claimed, posted and settled
/// as before, a crash re-delivers after the stale claim. Order: rows are
/// claimed oldest event first, and the posts of one object (bucket + key)
/// run one after the other.
pub async fn drain_outbox(
    db: &Arc<Mutex<JobsDb>>,
    client: &(impl EventDeliveryClient + ?Sized),
    config: &EventDeliveryConfig,
    claimant: &str,
    budget: Duration,
    now: impl Fn() -> i64,
) -> DrainReport {
    let mut report = DrainReport::default();
    if !config.is_active() {
        return report;
    }
    let started = std::time::Instant::now();
    loop {
        let last = dispatch_batch(db, client, config, claimant, now()).await;
        report.batches += 1;
        report.claimed += last.claimed;
        report.failed += last.failed;
        if !drain_continues(claim_limit(config), &last, started.elapsed(), budget) {
            break;
        }
    }
    prune_outbox(db, config, now()).await;
    report
}

/// Claim, deliver and settle ONE batch, then prune (what a tick did before
/// it drained; the tests drive it batch by batch).
pub async fn dispatch_once(
    db: &Arc<Mutex<JobsDb>>,
    client: &(impl EventDeliveryClient + ?Sized),
    config: &EventDeliveryConfig,
    claimant: &str,
    now: i64,
) {
    if !config.is_active() {
        return;
    }
    dispatch_batch(db, client, config, claimant, now).await;
    prune_outbox(db, config, now).await;
}

async fn dispatch_batch(
    db: &Arc<Mutex<JobsDb>>,
    client: &(impl EventDeliveryClient + ?Sized),
    config: &EventDeliveryConfig,
    claimant: &str,
    now: i64,
) -> BatchReport {
    let claimed = {
        let db = db.lock().await;
        match db.event_outbox_claim_due(
            claimant,
            now,
            stale_claim_after_secs(config),
            claim_limit(config),
        ) {
            Ok(rows) => rows,
            Err(err) => {
                warn!("Event outbox claim failed: {}", err);
                return BatchReport::default();
            }
        }
    };

    // Deliver the whole batch, then settle it in one transaction. A crash
    // between the posts and the commit re-delivers the batch to the raw
    // webhooks after the stale claim (at-least-once; receivers deduplicate
    // on `event.id`). A Slack success is saved right after its post, so
    // Slack gets no second message.
    let (endpoints, rows) = deliver_batch(db, client, config, &claimed, now).await;
    let failed = rows
        .iter()
        .filter(|r| matches!(r, Settlement::Failed { .. }))
        .count();
    if let Err(err) =
        db.lock()
            .await
            .event_outbox_settle_batch(&endpoints, &rows, current_unix_seconds())
    {
        warn!(
            "Event outbox settle of {} row(s) failed; they are delivered again after the stale claim: {}",
            rows.len(),
            err
        );
    }
    BatchReport {
        claimed: claimed.len(),
        failed,
    }
}

/// Most prune statements one tick runs per kind. A drain delivers many rows
/// per tick, so one `prune_batch` per tick cannot keep up with it.
const PRUNE_MAX_ROUNDS: u32 = 50;

/// The retention prunes of a tick: delivered rows by age, dead rows by age,
/// delivered rows over the count. Each runs until a round deletes less than
/// `prune_batch` (nothing left to prune) or `PRUNE_MAX_ROUNDS`, and takes the
/// jobs-DB lock for one round at a time.
pub(crate) async fn prune_outbox(db: &Arc<Mutex<JobsDb>>, config: &EventDeliveryConfig, now: i64) {
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
    let batch = config.prune_batch;
    let retention = delivered_retention_secs(config);
    if retention > 0 {
        let before = now.saturating_sub(retention);
        prune_rounds(db, batch, "delivered prune", |db| {
            db.event_outbox_prune_delivered_before(before, batch, min_keep_id)
        })
        .await;
    }
    // Age out terminal failed rows independently of `delivered_retention` — a dead
    // target grows the DB even when delivered-retention is 0. Uses a fixed window
    // (or the delivered retention if larger) and the SAME listener-cursor floor so
    // it never drops an event replication hasn't consumed.
    let failed_window = retention.max(FAILED_ROW_MAX_AGE_SECS);
    let failed_before = now.saturating_sub(failed_window);
    prune_rounds(db, batch, "failed prune", |db| {
        db.event_outbox_prune_failed_before(failed_before, batch, min_keep_id)
    })
    .await;
    if batch > 0 {
        let max_rows = config.delivered_max_rows;
        prune_rounds(db, batch, "delivered count-prune", |db| {
            db.event_outbox_prune_delivered_over_count(max_rows, batch, min_keep_id)
        })
        .await;
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

/// Run one prune until a round deletes less than `batch`, at most
/// `PRUNE_MAX_ROUNDS` rounds. The lock is taken per round.
async fn prune_rounds(
    db: &Arc<Mutex<JobsDb>>,
    batch: u32,
    what: &str,
    mut round: impl FnMut(&JobsDb) -> Result<usize, crate::config_db::ConfigDbError>,
) {
    for _ in 0..PRUNE_MAX_ROUNDS {
        let deleted = round(&*db.lock().await);
        match deleted {
            Ok(n) if (n as u32) >= batch && batch > 0 => {}
            Ok(_) => return,
            Err(err) => {
                warn!("Event outbox {what} failed: {}", err);
                return;
            }
        }
    }
}

/// Pure: the claimed rows as groups of indexes into `claimed`, one group per
/// object (bucket + key), in claim order inside a group and by first
/// appearance across groups. The posts of one group run one after the
/// other; groups run side by side.
pub(crate) fn object_groups(claimed: &[EventOutboxRecord]) -> Vec<Vec<usize>> {
    let mut at: std::collections::HashMap<(&str, &str), usize> = std::collections::HashMap::new();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (i, e) in claimed.iter().enumerate() {
        let g = *at.entry((&e.bucket, &e.key)).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[g].push(i);
    }
    groups
}

/// Deliver every claimed row and return what to write: the endpoint
/// outcomes and each row's final state, in claim order. Writes nothing, so
/// the caller settles the batch in one transaction. At most
/// [`delivery_concurrency`] objects are posted at once.
async fn deliver_batch(
    db: &Arc<Mutex<JobsDb>>,
    client: &(impl EventDeliveryClient + ?Sized),
    config: &EventDeliveryConfig,
    claimed: &[EventOutboxRecord],
    now: i64,
) -> (Vec<EndpointOutcome>, Vec<Settlement>) {
    use futures::StreamExt;
    let ids: Vec<i64> = claimed.iter().map(|e| e.id).collect();
    let done = db
        .lock()
        .await
        .event_delivery_done_endpoints_many(&ids)
        .map_err(|e| format!("event delivery state unreadable: {e}"));
    let deliver_group = |group: Vec<usize>| {
        let done = &done;
        async move {
            let mut out = Vec::with_capacity(group.len());
            for i in group {
                let event = &claimed[i];
                let mut endpoints = Vec::new();
                let outcome = match done {
                    Ok(done) => {
                        let mut done = done.get(&event.id).cloned().unwrap_or_default();
                        deliver_to_targets(db, client, config, event, &mut done, &mut endpoints)
                            .await
                    }
                    Err(e) => Err(e.clone()),
                };
                out.push((i, endpoints, settlement(config, event, outcome, now)));
            }
            out
        }
    };
    let mut results: Vec<(usize, Vec<EndpointOutcome>, Settlement)> =
        futures::stream::iter(object_groups(claimed))
            .map(deliver_group)
            .buffer_unordered(delivery_concurrency(config))
            .flat_map(futures::stream::iter)
            .collect()
            .await;
    results.sort_by_key(|(i, _, _)| *i);
    let mut endpoints = Vec::new();
    let mut rows = Vec::with_capacity(results.len());
    for (_, e, row) in results {
        endpoints.extend(e);
        rows.push(row);
    }
    (endpoints, rows)
}

/// A row's final state from the outcome of its posts.
fn settlement(
    config: &EventDeliveryConfig,
    event: &EventOutboxRecord,
    outcome: Result<(), String>,
    now: i64,
) -> Settlement {
    match outcome {
        Ok(()) => {
            record_outcome(None, now);
            Settlement::Delivered { id: event.id }
        }
        Err(err) => {
            let stored = persistable_error(&err);
            record_outcome(Some(&stored), now);
            // A permanent error fails the row now: a retry cannot fix it.
            let next_attempt_at = if is_permanent_error(&err) {
                None
            } else {
                next_attempt_after(config, event.attempts, now)
            };
            Settlement::Failed {
                id: event.id,
                error: stored,
                next_attempt_at,
            }
        }
    }
}

/// Pure: `true` when a post under `config` cannot be deduplicated by the
/// receiver, so a success must be saved before the next post. A Slack post
/// (`chat.postMessage` or an Incoming Webhook) has no idempotency key: a
/// second post is a second message. A raw webhook carries `event.id`, and
/// its receiver deduplicates on it, so its outcome waits for the batch
/// commit.
fn saves_each_success(config: &EventDeliveryConfig) -> bool {
    config.format == EventDeliveryFormat::Slack
}

/// Fan-out: deliver to every target that has not yet received this event
/// (`done`), push each outcome onto `endpoints`, and fail the row (for a
/// retry) when any target failed. One failing target never stops the
/// others, and a retry never re-posts to a Slack target that already
/// succeeded: a Slack success is saved right after its post
/// ([`saves_each_success`]), so not even a crash before the batch settle
/// sends the message twice. A raw webhook can get the event again after
/// such a crash; its receiver deduplicates on `event.id`.
async fn deliver_to_targets(
    db: &Arc<Mutex<JobsDb>>,
    client: &(impl EventDeliveryClient + ?Sized),
    config: &EventDeliveryConfig,
    event: &EventOutboxRecord,
    done: &mut std::collections::HashSet<String>,
    endpoints: &mut Vec<EndpointOutcome>,
) -> Result<(), String> {
    // A config fault (no endpoint, a bad glob, no channel) fails every retry
    // the same way until the config changes.
    let targets = delivery_targets(config, event).map_err(permanent)?;
    if targets.is_empty() {
        return Ok(()); // filtered out or routed nowhere: consumed
    }
    let mut errors = Vec::new();
    for target in targets {
        let id = target.id();
        if done.contains(&id) || target.legacy_id().is_some_and(|l| done.contains(&l)) {
            continue;
        }
        let outcome = client.deliver_target(config, event, &target).await;
        let error = outcome.as_ref().err().map(|e| persistable_error(e));
        if error.is_none() && saves_each_success(config) {
            match db
                .lock()
                .await
                .event_delivery_record(event.id, &id, None, current_unix_seconds())
            {
                Ok(()) => {
                    done.insert(id);
                    continue;
                }
                // The batch settle tries again.
                Err(e) => warn!(
                    "Event delivery: saving the Slack post of outbox row {} failed: {}",
                    event.id, e
                ),
            }
        }
        endpoints.push(EndpointOutcome {
            outbox_id: event.id,
            endpoint_id: id.clone(),
            error: error.clone(),
        });
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
    // On a CHAR boundary: a naive `&error[..1000]` panics when byte 1000
    // lands mid-UTF-8-char (a backend error carrying multi-byte text), which
    // would kill the delivery dispatcher permanently.
    format!("{}...", crate::security::str_prefix(error, MAX_ERROR_LEN))
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
    use crate::config_db::JobsDb;

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
        let db = Arc::new(Mutex::new(JobsDb::in_memory("test-pass").unwrap()));
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
        let db = JobsDb::in_memory("test-pass").unwrap();
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
        let db = Arc::new(Mutex::new(JobsDb::in_memory("test-pass").unwrap()));
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
        let db = Arc::new(Mutex::new(JobsDb::in_memory("test-pass").unwrap()));
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
                DeliveryTarget::Webhook { url, .. } => url.as_str(),
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
        let db = Arc::new(Mutex::new(JobsDb::in_memory("test-pass").unwrap()));
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
        let slot = |url: &str| config.webhook_endpoints().iter().position(|u| *u == url);
        let wid = |url: &str| {
            DeliveryTarget::Webhook {
                url: url.to_string(),
                slot: slot(url).unwrap(),
            }
            .id()
        };
        let status = |url: &str| {
            deliveries
                .iter()
                .find(|d| d.endpoint_id == wid(url))
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
                .find(|d| d.endpoint_id == wid("http://b.invalid/hook"))
                .unwrap()
                .attempts,
            2
        );
    }

    /// An operator rotates the token in the URL of endpoint A (the same
    /// configured endpoint) while an event waits for a retry of B. A had
    /// the event already; the retry must post to B only. Keyed by the URL
    /// hash, A's new URL looked like a new endpoint and got it again.
    #[tokio::test]
    async fn a_rotated_endpoint_url_keeps_its_delivered_state() {
        let db = Arc::new(Mutex::new(JobsDb::in_memory("test-pass").unwrap()));
        db.lock().await.event_outbox_insert(&event("rot")).unwrap();
        let client = EndpointClient {
            fail_url: "http://b.invalid/hook",
            fail_times: 1,
            failed: AtomicUsize::new(0),
            posts: Default::default(),
        };
        let mut config = EventDeliveryConfig {
            webhook_url: Some("http://a.invalid/hook?token=old".into()),
            webhook_urls: vec!["http://b.invalid/hook".into()],
            ..cfg()
        };
        dispatch_once(&db, &client, &config, "w", 200).await;
        client.posts.lock().unwrap().clear();
        config.webhook_url = Some("http://a.invalid/hook?token=new".into());
        dispatch_once(&db, &client, &config, "w", 205).await;
        assert_eq!(*client.posts.lock().unwrap(), vec!["http://b.invalid/hook"]);
    }

    /// P1.9: one dispatch of 100 rows to two endpoints settles in one
    /// transaction. Before, each endpoint outcome and each row mark was its
    /// own commit (300 commits for this batch).
    #[tokio::test]
    async fn a_dispatch_batch_settles_in_one_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Mutex::new(
            JobsDb::open_or_create(&dir.path().join("c.db"), "pw").unwrap(),
        ));
        let events: Vec<NewEvent> = (0..100).map(|i| event(&format!("k{i}"))).collect();
        db.lock().await.event_outbox_insert_many(&events).unwrap();
        let client = EndpointClient {
            fail_url: "http://b.invalid/hook",
            fail_times: 50,
            failed: AtomicUsize::new(0),
            posts: Default::default(),
        };
        let config = EventDeliveryConfig {
            webhook_url: Some("http://a.invalid/hook".into()),
            webhook_urls: vec!["http://b.invalid/hook".into()],
            batch_size: 100,
            ..cfg()
        };
        let before = crate::event_outbox::pages_written(&*db.lock().await);
        dispatch_once(&db, &client, &config, "w", 200).await;
        let pages = crate::event_outbox::pages_written(&*db.lock().await) - before;
        assert_eq!(client.posts.lock().unwrap().len(), 200);
        let counts = db.lock().await.event_outbox_status_counts().unwrap();
        assert_eq!((counts.delivered, counts.pending), (50, 50));
        let deliveries = db
            .lock()
            .await
            .event_deliveries_for_many(&[1, 100])
            .unwrap();
        assert_eq!(deliveries[&1].len(), 2, "both endpoint outcomes recorded");
        assert!(
            deliveries[&100].iter().all(|d| d.status == "delivered"),
            "the late rows reached both endpoints"
        );
        assert!(
            pages <= 30,
            "claim + one settle commit, wrote {pages} pages"
        );
    }

    /// At-least-once across the batched settle: a crash after the posts and
    /// before the settle commit leaves the rows claimed with no endpoint
    /// state. Once the claim is stale, the next dispatch delivers them again
    /// to every endpoint, with the same event id.
    #[tokio::test]
    async fn a_crash_before_the_settle_delivers_the_batch_again() {
        let db = Arc::new(Mutex::new(JobsDb::in_memory("test-pass").unwrap()));
        let id = db
            .lock()
            .await
            .event_outbox_insert(&event("crash"))
            .unwrap();
        let client = EndpointClient {
            fail_url: "http://none.invalid/",
            fail_times: 0,
            failed: AtomicUsize::new(0),
            posts: Default::default(),
        };
        let config = EventDeliveryConfig {
            webhook_url: Some("http://a.invalid/hook".into()),
            webhook_urls: vec!["http://b.invalid/hook".into()],
            ..cfg()
        };
        // The crash: claim and post, then drop the settlement unwritten.
        let claimed = db
            .lock()
            .await
            .event_outbox_claim_due("w", 200, 60, 10)
            .unwrap();
        let _lost = deliver_batch(&db, &client, &config, &claimed, 200).await;
        assert_eq!(client.posts.lock().unwrap().len(), 2);
        let row = db.lock().await.event_outbox_load(id).unwrap().unwrap();
        assert_eq!(row.status, STATUS_IN_PROGRESS);
        // Before the claim is stale, nothing is delivered again.
        dispatch_once(&db, &client, &config, "w2", 230).await;
        assert_eq!(client.posts.lock().unwrap().len(), 2);
        // After it, both endpoints get the event again.
        client.posts.lock().unwrap().clear();
        dispatch_once(&db, &client, &config, "w2", 261).await;
        assert_eq!(
            *client.posts.lock().unwrap(),
            vec!["http://a.invalid/hook", "http://b.invalid/hook"]
        );
        let row = db.lock().await.event_outbox_load(id).unwrap().unwrap();
        assert_eq!((row.id, row.status.as_str()), (id, STATUS_DELIVERED));
    }

    /// Slack has no idempotency key (neither `chat.postMessage` nor an
    /// Incoming Webhook), so each successful Slack post is saved at once,
    /// not with the batch. A crash after the posts and before the settle
    /// commit then posts nothing again once the claim is stale. Before, the
    /// whole batch was posted to every channel a second time.
    #[tokio::test]
    async fn a_crash_before_the_settle_posts_no_slack_message_twice() {
        let route = |c: &str| crate::config_sections::SlackRoute {
            name: None,
            bucket: None,
            prefix_globs: vec![],
            channel: c.into(),
        };
        let bot = EventDeliveryConfig {
            format: EventDeliveryFormat::Slack,
            webhook_url: None,
            slack_bot_token: Some("xoxb-test".into()),
            slack_routes: vec![route("C1"), route("C2")],
            ..cfg()
        };
        let hooks = EventDeliveryConfig {
            format: EventDeliveryFormat::Slack,
            webhook_url: Some("https://hooks.slack.test/a".into()),
            webhook_urls: vec!["https://hooks.slack.test/b".into()],
            slack_bot_token: None,
            ..cfg()
        };
        for config in [bot, hooks] {
            let db = Arc::new(Mutex::new(JobsDb::in_memory("test-pass").unwrap()));
            let ids: Vec<i64> = (0..3)
                .map(|_| {
                    db.try_lock()
                        .unwrap()
                        .event_outbox_insert(&slack_event())
                        .unwrap()
                })
                .collect();
            let client = EndpointClient {
                fail_url: "none",
                fail_times: 0,
                failed: AtomicUsize::new(0),
                posts: Default::default(),
            };
            let claimed = db
                .lock()
                .await
                .event_outbox_claim_due("w", 200, 60, 10)
                .unwrap();
            let _lost = deliver_batch(&db, &client, &config, &claimed, 200).await;
            assert_eq!(client.posts.lock().unwrap().len(), 6);
            client.posts.lock().unwrap().clear();
            dispatch_once(&db, &client, &config, "w2", 261).await;
            assert!(
                client.posts.lock().unwrap().is_empty(),
                "a Slack message went out twice: {:?}",
                client.posts.lock().unwrap()
            );
            for id in ids {
                let row = db.lock().await.event_outbox_load(id).unwrap().unwrap();
                assert_eq!(row.status, STATUS_DELIVERED);
                let d = db.lock().await.event_deliveries_for(id).unwrap();
                assert_eq!(d.len(), 2);
                assert!(
                    d.iter().all(|d| d.status == "delivered" && d.attempts == 1),
                    "{d:?}"
                );
            }
        }
    }

    /// The same URL listed twice is one endpoint: one post.
    #[tokio::test]
    async fn a_url_listed_twice_gets_one_post() {
        let db = Arc::new(Mutex::new(JobsDb::in_memory("test-pass").unwrap()));
        db.lock().await.event_outbox_insert(&event("dup")).unwrap();
        let client = EndpointClient {
            fail_url: "http://none.invalid/",
            fail_times: 0,
            failed: AtomicUsize::new(0),
            posts: Default::default(),
        };
        let config = EventDeliveryConfig {
            webhook_url: Some("http://a.invalid/hook".into()),
            webhook_urls: vec!["http://a.invalid/hook".into()],
            ..cfg()
        };
        dispatch_once(&db, &client, &config, "w", 200).await;
        assert_eq!(*client.posts.lock().unwrap(), vec!["http://a.invalid/hook"]);
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
        let db = Arc::new(Mutex::new(JobsDb::in_memory("test-pass").unwrap()));
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
    fn endpoint_labels_name_every_target_without_secrets() {
        let config = EventDeliveryConfig {
            webhook_url: Some("https://hooks.slack.test/services/T/B/SECRET".into()),
            webhook_urls: vec!["https://hooks.slack.test/services/T/B/OTHER".into()],
            slack_channel: Some("#ops".into()),
            ..cfg()
        };
        let labels = endpoint_labels(&config);
        let a = &labels[&endpoint_id("https://hooks.slack.test/services/T/B/SECRET")];
        let b = &labels[&endpoint_id("https://hooks.slack.test/services/T/B/OTHER")];
        assert_eq!(a, "webhook 1: https://hooks.slack.test/<redacted>");
        assert_eq!(b, "webhook 2: https://hooks.slack.test/<redacted>");
        assert_eq!(
            labels[&DeliveryTarget::SlackChannel("#ops".into()).id()],
            "slack channel #ops"
        );
        assert!(labels.values().all(|l| !l.contains("SECRET")));
    }

    #[tokio::test]
    async fn deliveries_for_many_groups_rows_by_outbox_id() {
        let db = JobsDb::in_memory("test-pass").unwrap();
        let a = db.event_outbox_insert(&event("a")).unwrap();
        let b = db.event_outbox_insert(&event("b")).unwrap();
        db.event_delivery_record(a, "e1", None, 10).unwrap();
        db.event_delivery_record(a, "e2", Some("HTTP 503"), 10)
            .unwrap();
        db.event_delivery_record(b, "e1", None, 10).unwrap();
        let map = db.event_deliveries_for_many(&[a, b, 999]).unwrap();
        assert_eq!(map[&a].len(), 2);
        assert_eq!(map[&a][1].last_error.as_deref(), Some("HTTP 503"));
        assert_eq!(map[&b].len(), 1);
        assert!(!map.contains_key(&999));
        assert!(db.event_deliveries_for_many(&[]).unwrap().is_empty());
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
            Ok(vec![DeliveryTarget::Webhook {
                url: "http://example.invalid/hook".into(),
                slot: 0
            }])
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
            DeliveryTarget::Webhook {
                url: "x".into(),
                slot: 0
            }
            .id()
        );
    }

    /// Browser review #9: the redaction pass mangled our own advice text
    /// "(use https://)" into "(use <invalid-url>)".
    #[test]
    fn redaction_keeps_a_bare_scheme_in_error_text() {
        let msg = "webhook endpoint rejected: URL scheme 'http' is not allowed (use https://)";
        assert_eq!(persistable_error(msg), msg);
        // A real URL next to it is still redacted.
        assert_eq!(
            persistable_error("http://127.0.0.1:5056/deltaglider: refused (use https://)"),
            "http://127.0.0.1:5056/<redacted>: refused (use https://)"
        );
    }

    /// `allow_local: true` lets a webhook reach http:// and private
    /// addresses (as for backends); metadata stays refused.
    #[test]
    fn allow_local_opens_private_webhooks_but_never_metadata() {
        let client = HttpWebhookDeliveryClient::default();
        let mut c = cfg();
        assert!(client
            .check_ssrf(&c, "http://127.0.0.1:5056/hook", "webhook endpoint")
            .is_err());
        c.allow_local = true;
        client
            .check_ssrf(&c, "http://127.0.0.1:5056/hook", "webhook endpoint")
            .expect("allow_local opens loopback");
        client
            .check_ssrf(&c, "http://10.0.0.5/hook", "webhook endpoint")
            .expect("allow_local opens private space");
        assert!(client
            .check_ssrf(&c, "http://169.254.169.254/latest", "webhook endpoint")
            .is_err());
    }

    /// An SSRF refusal cannot succeed on a retry: the row fails at once
    /// instead of retrying max_attempts times.
    #[tokio::test]
    async fn ssrf_refusal_dead_letters_at_once() {
        let db = Arc::new(Mutex::new(JobsDb::in_memory("test-pass").unwrap()));
        let id = db.lock().await.event_outbox_insert(&event("ssrf")).unwrap();
        let mut config = cfg();
        config.max_attempts = 8;
        config.webhook_url = Some("http://127.0.0.1:5056/deltaglider".to_string());
        let client = HttpWebhookDeliveryClient::default();
        dispatch_once(&db, &client, &config, "test-worker", 200).await;
        let row = db
            .lock()
            .await
            .event_outbox_recent(10)
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert_eq!(row.status, STATUS_FAILED, "{row:?}");
        assert_eq!(row.next_attempt_at, None);
        assert_eq!(row.attempts, 1);
        assert!(
            row.last_error
                .as_deref()
                .unwrap_or("")
                .contains("allow_local"),
            "the error says how to opt in: {row:?}"
        );
    }

    #[test]
    fn delivery_state_truth_table() {
        let mut c = cfg();
        let failed = LastOutcome {
            ok: false,
            error: Some("x".into()),
            at: 1,
        };
        let ok = LastOutcome {
            ok: true,
            error: None,
            at: 1,
        };
        assert_eq!(delivery_state(&c, Some(&failed)), DeliveryState::Failing);
        assert_eq!(delivery_state(&c, Some(&ok)), DeliveryState::Active);
        assert_eq!(delivery_state(&c, None), DeliveryState::Active);
        c.webhook_url = None;
        assert_eq!(delivery_state(&c, Some(&failed)), DeliveryState::NoEndpoint);
        c.enabled = false;
        assert_eq!(delivery_state(&c, Some(&failed)), DeliveryState::Disabled);
    }

    #[tokio::test]
    async fn dispatch_retries_then_permanently_fails() {
        let db = Arc::new(Mutex::new(JobsDb::in_memory("test-pass").unwrap()));
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

        let db = Arc::new(Mutex::new(JobsDb::in_memory("test-pass").unwrap()));
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

        let db = Arc::new(Mutex::new(JobsDb::in_memory("test-pass").unwrap()));
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
            let db = Arc::new(Mutex::new(JobsDb::in_memory("test-pass").unwrap()));
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

    // === E6/F21: the drain ===

    /// A delivery client that records how its posts overlap and in what
    /// order they start.
    struct SinkClient {
        delay: Duration,
        fail: bool,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        /// Posts running now, per object key; the max of any of them.
        per_key: std::sync::Mutex<std::collections::HashMap<String, usize>>,
        max_per_key: AtomicUsize,
        /// `(key, event id)` in the order the posts started.
        started: std::sync::Mutex<Vec<(String, i64)>>,
    }

    impl SinkClient {
        fn new(delay: Duration, fail: bool) -> Self {
            Self {
                delay,
                fail,
                in_flight: AtomicUsize::new(0),
                max_in_flight: AtomicUsize::new(0),
                per_key: Default::default(),
                max_per_key: AtomicUsize::new(0),
                started: Default::default(),
            }
        }
    }

    impl EventDeliveryClient for SinkClient {
        async fn deliver(
            &self,
            _config: &EventDeliveryConfig,
            event: &EventOutboxRecord,
        ) -> Result<(), String> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            let same = {
                let mut m = self.per_key.lock().unwrap();
                let n = m.entry(event.key.clone()).or_default();
                *n += 1;
                *n
            };
            self.max_per_key.fetch_max(same, Ordering::SeqCst);
            self.started
                .lock()
                .unwrap()
                .push((event.key.clone(), event.id));
            tokio::time::sleep(self.delay).await;
            *self.per_key.lock().unwrap().get_mut(&event.key).unwrap() -= 1;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            if self.fail {
                Err("HTTP 503".to_string())
            } else {
                Ok(())
            }
        }
    }

    async fn seeded(keys: impl IntoIterator<Item = String>) -> Arc<Mutex<JobsDb>> {
        let db = Arc::new(Mutex::new(JobsDb::in_memory("test-pass").unwrap()));
        {
            let db = db.lock().await;
            for k in keys {
                db.event_outbox_insert(&event(&k)).unwrap();
            }
        }
        db
    }

    async fn count_status(db: &Arc<Mutex<JobsDb>>, status: &str) -> i64 {
        db.lock().await.event_outbox_count(Some(status)).unwrap()
    }

    /// F21: 10x `batch_size` rows drain in ONE tick. Before, a tick claimed
    /// one batch: 300 events took 60 s, and a producer above 5 events per
    /// second grew the backlog without bound.
    #[tokio::test]
    async fn ten_batches_drain_in_one_tick() {
        let config = cfg(); // batch_size 10
        let db = seeded((0..100).map(|i| format!("k{i}"))).await;
        let client = SinkClient::new(Duration::ZERO, false);
        let report = drain_outbox(
            &db,
            &client,
            &config,
            "w",
            Duration::from_secs(30),
            current_unix_seconds,
        )
        .await;
        assert_eq!(report.claimed, 100, "{report:?}");
        assert!(report.batches >= 10, "{report:?}");
        assert_eq!(count_status(&db, STATUS_DELIVERED).await, 100);
        assert_eq!(count_status(&db, "pending").await, 0);
    }

    /// The old behaviour, kept as `dispatch_once`: one batch.
    #[tokio::test]
    async fn dispatch_once_still_takes_one_batch() {
        let db = seeded((0..100).map(|i| format!("k{i}"))).await;
        let client = SinkClient::new(Duration::ZERO, false);
        dispatch_once(&db, &client, &cfg(), "w", current_unix_seconds()).await;
        assert_eq!(count_status(&db, STATUS_DELIVERED).await, 10);
    }

    #[tokio::test]
    async fn a_spent_budget_ends_the_drain_after_the_current_batch() {
        let db = seeded((0..100).map(|i| format!("k{i}"))).await;
        let client = SinkClient::new(Duration::ZERO, false);
        let report = drain_outbox(
            &db,
            &client,
            &cfg(),
            "w",
            Duration::ZERO,
            current_unix_seconds,
        )
        .await;
        assert_eq!(report.batches, 1, "{report:?}");
        assert_eq!(count_status(&db, STATUS_DELIVERED).await, 10);
    }

    /// A target that fails (nearly) every row is not asked again at once:
    /// the drain stops after the failed batch, and the rows wait for their
    /// retry time (the backoff of the row is unchanged).
    #[tokio::test]
    async fn a_failing_target_is_not_hammered() {
        let db = seeded((0..100).map(|i| format!("k{i}"))).await;
        let client = SinkClient::new(Duration::ZERO, true);
        let now = current_unix_seconds();
        let report = drain_outbox(
            &db,
            &client,
            &cfg(),
            "w",
            Duration::from_secs(30),
            move || now,
        )
        .await;
        assert_eq!(report.batches, 1, "{report:?}");
        assert_eq!(
            client.started.lock().unwrap().len(),
            10,
            "one batch of posts"
        );
        let rows = db.lock().await.event_outbox_recent(100).unwrap();
        let retrying: Vec<_> = rows
            .iter()
            .filter(|r| r.status == "pending" && r.attempts == 1)
            .collect();
        assert_eq!(retrying.len(), 10);
        assert!(
            retrying
                .iter()
                .all(|r| r.next_attempt_at.is_some_and(|t| t > now)),
            "a failed row waits for its retry time"
        );
        // The same tick again, at the same instant: the failed rows are not
        // due, so the next batch is the next 10 fresh rows, not a retry.
        let before = client.started.lock().unwrap().len();
        drain_outbox(&db, &client, &cfg(), "w", Duration::ZERO, move || now).await;
        let started = client.started.lock().unwrap();
        let ids: std::collections::HashSet<i64> = started.iter().map(|(_, id)| *id).collect();
        assert_eq!(started.len() - before, 10);
        assert_eq!(
            ids.len(),
            started.len(),
            "no row posted twice at one instant"
        );
    }

    /// Posts overlap (up to the bound), but the posts of ONE object run
    /// one after the other and in claim order.
    #[tokio::test]
    async fn posts_overlap_up_to_the_bound_and_one_object_stays_in_order() {
        let mut config = cfg();
        config.batch_size = 100;
        let mut keys: Vec<String> = (0..30).map(|i| format!("k{i}")).collect();
        keys.extend((0..5).map(|_| "same".to_string()));
        let db = seeded(keys).await;
        let client = SinkClient::new(Duration::from_millis(30), false);
        let started = std::time::Instant::now();
        drain_outbox(
            &db,
            &client,
            &config,
            "w",
            Duration::from_secs(30),
            current_unix_seconds,
        )
        .await;
        let max = client.max_in_flight.load(Ordering::SeqCst);
        assert!(max > 1, "posts ran one by one (max in flight {max})");
        assert!(max <= DELIVERY_CONCURRENCY, "bound broken: {max}");
        assert_eq!(client.max_per_key.load(Ordering::SeqCst), 1);
        let started_order = client.started.lock().unwrap().clone();
        let same: Vec<i64> = started_order
            .iter()
            .filter(|(k, _)| k == "same")
            .map(|(_, id)| *id)
            .collect();
        assert!(same.windows(2).all(|w| w[0] < w[1]), "order: {same:?}");
        assert_eq!(count_status(&db, STATUS_DELIVERED).await, 35);
        // 35 posts at 30 ms: sequential would take over a second.
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "{:?}",
            started.elapsed()
        );
    }

    /// Slack takes one post at a time (the target answers about one message
    /// per second per channel with 429; messages read in order).
    #[tokio::test]
    async fn slack_posts_one_at_a_time() {
        let mut config = cfg();
        config.format = EventDeliveryFormat::Slack;
        config.webhook_url = Some("http://example.invalid/services/T/B/X".to_string());
        assert_eq!(delivery_concurrency(&config), 1);
        config.format = EventDeliveryFormat::Raw;
        assert_eq!(delivery_concurrency(&config), DELIVERY_CONCURRENCY);
    }

    #[test]
    fn drain_continues_truth_table() {
        let full = BatchReport {
            claimed: 10,
            failed: 0,
        };
        let s = Duration::from_secs;
        assert!(drain_continues(10, &full, s(1), s(10)));
        // A short claim: the outbox is empty.
        assert!(!drain_continues(
            10,
            &BatchReport {
                claimed: 9,
                failed: 0
            },
            s(1),
            s(10)
        ));
        // Half the rows failed: stop.
        assert!(!drain_continues(
            10,
            &BatchReport {
                claimed: 10,
                failed: 5
            },
            s(1),
            s(10)
        ));
        assert!(drain_continues(
            10,
            &BatchReport {
                claimed: 10,
                failed: 4
            },
            s(1),
            s(10)
        ));
        // Time is up.
        assert!(!drain_continues(10, &full, s(10), s(10)));
        assert!(!drain_continues(10, &BatchReport::default(), s(0), s(10)));
    }

    #[test]
    fn object_groups_keep_claim_order_per_object() {
        let rec = |id: i64, key: &str| EventOutboxRecord {
            id,
            kind: "ObjectCreated".into(),
            bucket: "b".into(),
            key: key.into(),
            source: "s3".into(),
            occurred_at: id,
            payload: json!({}),
            status: "in_progress".into(),
            attempts: 1,
            next_attempt_at: None,
            claimed_by: None,
            claimed_at: None,
            delivered_at: None,
            last_error: None,
            created_at: 0,
        };
        let claimed = vec![
            rec(1, "a"),
            rec(2, "b"),
            rec(3, "a"),
            rec(4, "c"),
            rec(5, "a"),
        ];
        assert_eq!(
            object_groups(&claimed),
            vec![vec![0, 2, 4], vec![1], vec![3]]
        );
        assert!(object_groups(&[]).is_empty());
    }

    /// The retention prunes keep up with a drain: a drain that delivers
    /// 10x `prune_batch` rows leaves no more than `delivered_max_rows`.
    #[tokio::test]
    async fn the_prunes_keep_up_with_a_drain() {
        let mut config = cfg();
        config.prune_batch = 5;
        config.delivered_max_rows = 20;
        config.delivered_retention = "0s".to_string();
        let db = seeded((0..100).map(|i| format!("k{i}"))).await;
        let client = SinkClient::new(Duration::ZERO, false);
        drain_outbox(
            &db,
            &client,
            &config,
            "w",
            Duration::from_secs(30),
            current_unix_seconds,
        )
        .await;
        assert_eq!(count_status(&db, STATUS_DELIVERED).await, 20);
    }

    /// End to end with local sinks: a raw webhook that answers 503 for its
    /// first requests (the rows retry and arrive), and a Slack Incoming
    /// Webhook (one post at a time, one message per event, in order).
    #[tokio::test]
    async fn a_drain_delivers_to_local_sinks_through_5xx_and_slack() {
        // --- raw webhook: 503 for the first 3 requests
        let ids = Arc::new(std::sync::Mutex::new(Vec::<i64>::new()));
        let requests = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/hook",
            post({
                let (ids, requests) = (ids.clone(), requests.clone());
                move |Json(payload): Json<Value>| {
                    let (ids, requests) = (ids.clone(), requests.clone());
                    async move {
                        if requests.fetch_add(1, Ordering::SeqCst) < 3 {
                            return StatusCode::SERVICE_UNAVAILABLE;
                        }
                        ids.lock()
                            .unwrap()
                            .push(payload["event"]["id"].as_i64().unwrap());
                        StatusCode::NO_CONTENT
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let db = seeded((0..25).map(|i| format!("k{i}"))).await;
        let mut config = cfg();
        config.webhook_url = Some(format!("{base}/hook"));
        config.max_attempts = 5;
        let client = HttpWebhookDeliveryClient::for_tests();
        let t0 = current_unix_seconds();
        let report = drain_outbox(
            &db,
            &client,
            &config,
            "w",
            Duration::from_secs(30),
            move || t0,
        )
        .await;
        assert_eq!(report.claimed, 25, "{report:?}");
        assert_eq!(report.failed, 3, "{report:?}");
        assert_eq!(count_status(&db, STATUS_DELIVERED).await, 22);
        // The three failed rows come back after their backoff, and nothing
        // is posted twice.
        let t1 = t0 + 60;
        drain_outbox(
            &db,
            &client,
            &config,
            "w",
            Duration::from_secs(30),
            move || t1,
        )
        .await;
        assert_eq!(count_status(&db, STATUS_DELIVERED).await, 25);
        let mut got = ids.lock().unwrap().clone();
        got.sort_unstable();
        got.dedup();
        assert_eq!(got.len(), 25, "every event arrived once");
        assert_eq!(requests.load(Ordering::SeqCst), 28, "25 + 3 refused");
        server.abort();

        // --- Slack incoming webhook
        let texts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let app = Router::new().route(
            "/services/T/B/X",
            post({
                let texts = texts.clone();
                move |Json(payload): Json<Value>| {
                    let texts = texts.clone();
                    async move {
                        texts
                            .lock()
                            .unwrap()
                            .push(payload["text"].as_str().unwrap_or("").to_string());
                        StatusCode::OK
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let db = seeded((0..12).map(|i| format!("slack-{i:02}"))).await;
        let mut config = cfg();
        config.format = EventDeliveryFormat::Slack;
        config.webhook_url = Some(format!("{base}/services/T/B/X"));
        let report = drain_outbox(
            &db,
            &client,
            &config,
            "w",
            Duration::from_secs(30),
            current_unix_seconds,
        )
        .await;
        assert_eq!(report.claimed, 12, "{report:?}");
        assert_eq!(count_status(&db, STATUS_DELIVERED).await, 12);
        let texts = texts.lock().unwrap().clone();
        assert_eq!(texts.len(), 12);
        let keys: Vec<usize> = texts
            .iter()
            .map(|t| {
                let at = t.find("slack-").expect("key in text") + 6;
                t[at..at + 2].parse().unwrap()
            })
            .collect();
        assert!(
            keys.windows(2).all(|w| w[0] < w[1]),
            "Slack order: {keys:?}"
        );
        server.abort();
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
