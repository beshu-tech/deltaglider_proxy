//! Background delivery for the durable event outbox.
//!
//! The dispatcher is intentionally conservative: it is disabled unless
//! `advanced.event_delivery.enabled=true` and `webhook_url` is set. Request
//! handlers never call this module; they only append to `event_outbox`.

use crate::background::parse_duration_or;
use crate::config::SharedConfig;
use crate::config_db::ConfigDb;
use crate::config_sections::EventDeliveryConfig;
use crate::event_outbox::{
    current_unix_seconds, EventOutboxRecord, STATUS_DELIVERED, STATUS_FAILED, STATUS_IN_PROGRESS,
    STATUS_PENDING,
};
use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::Url;
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

const DEFAULT_TICK: Duration = Duration::from_secs(10);
const MIN_TICK: Duration = Duration::from_secs(1);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const MIN_TIMEOUT: Duration = Duration::from_millis(500);
const DEFAULT_RETRY_BASE: Duration = Duration::from_secs(5);
const DEFAULT_RETRY_MAX: Duration = Duration::from_secs(300);
const DEFAULT_STALE_CLAIM_AFTER: Duration = Duration::from_secs(60);
const DEFAULT_DELIVERED_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

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

#[derive(Clone, Default)]
pub struct HttpWebhookDeliveryClient {
    client: reqwest::Client,
}

#[async_trait]
impl EventDeliveryClient for HttpWebhookDeliveryClient {
    async fn deliver(
        &self,
        config: &EventDeliveryConfig,
        event: &EventOutboxRecord,
    ) -> Result<(), String> {
        let endpoints = config.webhook_endpoints();
        if endpoints.is_empty() {
            return Err("event delivery enabled without webhook endpoint".to_string());
        }
        let payload = EventWebhookPayload {
            schema: "deltaglider.event.v1",
            event,
        };
        let timeout = parse_duration_or(
            &config.request_timeout,
            DEFAULT_TIMEOUT,
            MIN_TIMEOUT,
            "event_delivery.request_timeout",
        );
        for endpoint in endpoints {
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
                .map_err(|e| format!("{endpoint}: {e}"))?;
            if !response.status().is_success() {
                return Err(format!(
                    "{endpoint}: webhook returned HTTP {}",
                    response.status()
                ));
            }
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
            let cfg = { config.read().await.event_delivery.clone() };
            tokio::time::sleep(dispatcher_tick(&cfg)).await;
            if !cfg.is_active() {
                debug!("Event outbox dispatcher skipped: disabled");
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

    let retention = delivered_retention_secs(config);
    if retention > 0 {
        let before = now.saturating_sub(retention);
        let db = db.lock().await;
        if let Err(err) = db.event_outbox_prune_delivered_before(before, config.prune_batch) {
            warn!("Event outbox delivered prune failed: {}", err);
        }
    }
    if config.prune_batch > 0 {
        let db = db.lock().await;
        if let Err(err) = db
            .event_outbox_prune_delivered_over_count(config.delivered_max_rows, config.prune_batch)
        {
            warn!("Event outbox delivered count-prune failed: {}", err);
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
        error.to_string()
    } else {
        format!("{}...", &error[..MAX_ERROR_LEN])
    }
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
            webhook_urls: Vec::new(),
            webhook_headers: Default::default(),
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
        let client = HttpWebhookDeliveryClient::default();

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
