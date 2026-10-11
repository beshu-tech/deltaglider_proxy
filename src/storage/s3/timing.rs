// SPDX-License-Identifier: BUSL-1.1

//! The time of every request that an S3 client of the proxy sends.
//!
//! [`RequestTimer`] is an SDK interceptor, and [`RequestTimer::client`] is
//! THE place where the proxy builds an S3 client (source test
//! `every_s3_client_is_built_by_the_request_timer`): the clients of the
//! storage backends and the coordination client (config sync, leases,
//! reference locks, health and capability probes). For each operation it
//! measures the total time (every SDK attempt and the backoff between them,
//! until the response headers arrive), the attempts sent, the time before
//! the first attempt was sent (serialization, the adaptive rate limiter's
//! wait, identity and signing), and how the operation ended. It records
//! them in [`BACKEND_REQUEST_DURATION`] and [`BACKEND_REQUEST_ATTEMPTS`],
//! and it logs one WARN line for an operation that is slower than
//! `DGP_BACKEND_SLOW_REQUEST_SECS` (an upload gets one more second for each
//! MiB of its body, as its deadline does).
//!
//! The measurement ([`RequestClock`]) lives in the operation's config bag,
//! and the bag drops it when the operation ends in any way. An operation
//! timeout, or a caller that drops the request, skips the SDK's last hooks;
//! the drop records the operation all the same.

use aws_sdk_s3::config::interceptors::{
    BeforeSerializationInterceptorContextRef, BeforeTransmitInterceptorContextRef,
    FinalizerInterceptorContextRef,
};
use aws_sdk_s3::config::timeout::TimeoutConfig;
use aws_sdk_s3::config::{ConfigBag, Intercept, RuntimeComponents};
use aws_smithy_runtime_api::box_error::BoxError;
use aws_smithy_runtime_api::client::endpoint::EndpointResolverParams;
use aws_smithy_runtime_api::client::interceptors::context::{Error, Output};
use aws_smithy_runtime_api::client::orchestrator::{Metadata, OrchestratorError};
use aws_smithy_types::config_bag::{Storable, StoreReplace};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::{Duration, Instant};

/// Upper bounds of the duration buckets, 10 ms to 60 s.
const DURATION_BUCKETS: [f64; 12] = [
    0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

/// The duration of each S3 operation that the proxy sends, from the start
/// of the SDK call until the response headers arrive (every attempt and
/// the backoff between them). Registered by `Metrics::new()`.
pub static BACKEND_REQUEST_DURATION: LazyLock<prometheus::HistogramVec> = LazyLock::new(|| {
    prometheus::HistogramVec::new(
        prometheus::HistogramOpts::new(
            "deltaglider_backend_request_duration_seconds",
            "Duration of the S3 requests that the proxy sends to its backends, SDK retries included",
        )
        .buckets(DURATION_BUCKETS.to_vec()),
        &["backend", "operation", "outcome"],
    )
    .expect("valid metric")
});

/// The attempts that the S3 operations sent: the first send and each SDK
/// retry. Minus the operation count of [`BACKEND_REQUEST_DURATION`], this
/// is the number of retries. Registered by `Metrics::new()`.
pub static BACKEND_REQUEST_ATTEMPTS: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::IntCounterVec::new(
        prometheus::Opts::new(
            "deltaglider_backend_request_attempts_total",
            "Attempts (first sends and SDK retries) of the S3 requests that the proxy sends to its backends",
        ),
        &["backend", "operation"],
    )
    .expect("valid metric")
});

/// The `backend` label of the coordination client.
pub(crate) const COORDINATION: &str = "coordination";

/// The `backend` label of the health probe clients (one per backend
/// definition; the probe of a data backend is not a coordination request).
pub(crate) const HEALTH_PROBE: &str = "health";

/// The `backend` label of the conditional-write capability probe clients.
pub(crate) const CAPABILITY_PROBE: &str = "capability";

/// Requests timed under `backend`, `operation` and `outcome` so far (tests
/// of the callers that pick a label).
#[cfg(test)]
pub(crate) fn backend_request_samples(backend: &str, operation: &str, outcome: &str) -> u64 {
    BACKEND_REQUEST_DURATION
        .with_label_values(&[backend, operation, outcome])
        .get_sample_count()
}

/// The `backend` label of a client that has no backend name (the CLI, a
/// connection test).
const UNNAMED: &str = "unnamed";

/// `DGP_BACKEND_SLOW_REQUEST_SECS` (default 5; 0 turns the log off): an S3
/// operation that takes this long or longer logs one WARN line (see
/// [`is_slow`]). Read when a client is built.
fn slow_request_threshold() -> Option<Duration> {
    match crate::config::env_parse_with_default("DGP_BACKEND_SLOW_REQUEST_SECS", 5u64) {
        0 => None,
        secs => Some(Duration::from_secs(secs)),
    }
}

/// The `backend` label of a client. An engine backend learns its name
/// after its clients are built (`S3Backend::with_health_name`), so the
/// label is set once, later; clones share it.
#[derive(Clone, Debug, Default)]
pub(crate) struct BackendLabel(Arc<OnceLock<String>>);

impl BackendLabel {
    /// A label with the value `name`.
    pub(crate) fn named(name: &str) -> Self {
        let label = Self::default();
        label.set(name);
        label
    }

    /// Give the label its value. A label keeps its first value.
    pub(crate) fn set(&self, name: &str) {
        let _ = self.0.set(name.to_string());
    }

    fn get(&self) -> &str {
        self.0.get().map_or(UNNAMED, String::as_str)
    }
}

/// The SDK interceptor that times every operation of a client.
#[derive(Clone, Debug)]
pub(crate) struct RequestTimer {
    label: BackendLabel,
    /// Operations this slow or slower log a WARN line; `None`: no log.
    slow: Option<Duration>,
}

impl RequestTimer {
    pub(crate) fn new(label: BackendLabel, slow: Option<Duration>) -> Self {
        Self { label, slow }
    }

    /// A timer with the slow-request threshold `DGP_BACKEND_SLOW_REQUEST_SECS`.
    pub(crate) fn from_env(label: BackendLabel) -> Self {
        Self::new(label, slow_request_threshold())
    }

    /// THE place where the proxy builds an S3 client: `builder` with this
    /// timer installed. A client rebuilt from a client's own config
    /// (`config().to_builder()`) keeps the timer.
    pub(crate) fn client(self, builder: aws_sdk_s3::config::Builder) -> aws_sdk_s3::Client {
        aws_sdk_s3::Client::from_conf(builder.interceptor(self).build())
    }
}

impl Intercept for RequestTimer {
    fn name(&self) -> &'static str {
        "DgpRequestTimer"
    }

    fn read_before_execution(
        &self,
        _context: &BeforeSerializationInterceptorContextRef<'_>,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        cfg.interceptor_state()
            .store_put(RequestClock::start(self.clone()));
        Ok(())
    }

    // The operation's own config (its name, its endpoint parameters, the
    // merged timeouts) is in the bag from here on, not yet when
    // `read_before_execution` runs for a client interceptor.
    fn read_before_serialization(
        &self,
        _context: &BeforeSerializationInterceptorContextRef<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let operation = cfg.load::<Metadata>().cloned();
        let bucket = cfg
            .load::<EndpointResolverParams>()
            .and_then(|p| p.get::<aws_sdk_s3::config::endpoint::Params>())
            .and_then(|p| p.bucket())
            .map(str::to_string);
        let deadline = cfg
            .load::<TimeoutConfig>()
            .and_then(|t| t.operation_timeout());
        if let Some(clock) = cfg.get_mut_from_interceptor_state::<RequestClock>() {
            clock.operation = operation;
            clock.bucket = bucket;
            clock.deadline = deadline;
        }
        Ok(())
    }

    // The serialized request: its path is the key and its query holds the
    // listing parameters. The endpoint (and the bucket of a path-style
    // request) and the signature are not in it yet.
    fn read_after_serialization(
        &self,
        context: &BeforeTransmitInterceptorContextRef<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let request = context.request();
        let uri = request.uri().to_string();
        let body_bytes = request.body().content_length().unwrap_or(0);
        if let Some(clock) = cfg.get_mut_from_interceptor_state::<RequestClock>() {
            clock.uri = Some(uri);
            clock.body_bytes = body_bytes;
        }
        Ok(())
    }

    fn read_before_transmit(
        &self,
        _context: &BeforeTransmitInterceptorContextRef<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        if let Some(clock) = cfg.get_mut_from_interceptor_state::<RequestClock>() {
            clock.first_send.get_or_insert_with(Instant::now);
            clock.statuses.push(None);
            clock.in_attempt = true;
        }
        Ok(())
    }

    // Not called for an attempt that its attempt timeout ended: that
    // attempt keeps `None` (no response).
    fn read_after_attempt(
        &self,
        context: &FinalizerInterceptorContextRef<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let status = context.response().map(|r| r.status().as_u16());
        if let Some(clock) = cfg.get_mut_from_interceptor_state::<RequestClock>() {
            if std::mem::take(&mut clock.in_attempt) {
                if let Some(last) = clock.statuses.last_mut() {
                    *last = status;
                }
            }
        }
        Ok(())
    }

    fn read_after_execution(
        &self,
        context: &FinalizerInterceptorContextRef<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let status = context.response().map(|r| r.status().as_u16());
        let end = context
            .output_or_error()
            .map(|result| end_of(result, status));
        if let Some(clock) = cfg.get_mut_from_interceptor_state::<RequestClock>() {
            match end {
                Some(end) => clock.finished = Some((Instant::now(), end)),
                // No result: the operation stopped before it was sent on
                // purpose (a presigned request). Nothing to record.
                None => clock.record = false,
            }
        }
        Ok(())
    }
}

/// How an operation ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum End {
    /// The operation succeeded.
    Ok,
    /// The backend answered with this error status.
    Status(u16),
    /// No answer in time: the operation timeout, an attempt timeout or the
    /// read or connect timeout.
    Timeout,
    /// The connection failed (DNS, refused, reset, TLS).
    Connection,
    /// The response could not be read or parsed.
    Response,
    /// Any other failure (an interceptor, the request construction).
    Other,
    /// The caller dropped the operation before it ended.
    Cancelled,
}

impl End {
    /// The `outcome` label: `ok` when the backend answered with a success
    /// or with an answer that the proxy reads as information (404 Not
    /// Found, 304 Not Modified, 412 Precondition Failed); `cancelled` when
    /// the caller dropped the request; `error` for everything else.
    pub(super) fn outcome(self) -> &'static str {
        match self {
            End::Ok | End::Status(304 | 404 | 412) => "ok",
            End::Cancelled => "cancelled",
            End::Status(_) | End::Timeout | End::Connection | End::Response | End::Other => "error",
        }
    }
}

impl std::fmt::Display for End {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            End::Ok => f.write_str("ok"),
            End::Status(s) => write!(f, "HTTP {s}"),
            End::Timeout => f.write_str("timeout"),
            End::Connection => f.write_str("connection failure"),
            End::Response => f.write_str("response failure"),
            End::Other => f.write_str("failure"),
            End::Cancelled => f.write_str("cancelled"),
        }
    }
}

/// How the SDK's result `result` ended an operation whose last response
/// had the status `status`.
fn end_of(result: Result<&Output, &OrchestratorError<Error>>, status: Option<u16>) -> End {
    let Err(err) = result else {
        return End::Ok;
    };
    if err.is_timeout_error() {
        End::Timeout
    } else if let Some(conn) = err.as_connector_error() {
        if conn.is_timeout() {
            End::Timeout
        } else {
            End::Connection
        }
    } else if err.is_response_error() {
        End::Response
    } else if err.is_operation_error() {
        status.map_or(End::Other, End::Status)
    } else {
        End::Other
    }
}

/// How an operation that the SDK did not finish ended: it took `elapsed`,
/// and its deadline is `deadline`. The operation timeout drops the
/// operation at its deadline; an earlier drop is the caller's.
pub(super) fn unfinished_end(elapsed: Duration, deadline: Option<Duration>) -> End {
    if deadline.is_some_and(|d| elapsed >= d) {
        End::Timeout
    } else {
        End::Cancelled
    }
}

/// Whether an operation that took `total` and sent a body of
/// `body_bytes` is slow for the threshold `threshold` (`None`: nothing is
/// slow). An upload's time grows with its body, so it gets the allowance
/// of its deadline: one more second for each MiB
/// ([`super::client::upload_deadline`]). Without it, every large upload
/// logged a line.
pub(super) fn is_slow(total: Duration, threshold: Option<Duration>, body_bytes: u64) -> bool {
    super::client::upload_deadline(threshold, body_bytes).is_some_and(|t| total >= t)
}

/// The object key and the listing prefix of a serialized request URI,
/// percent-decoded. Only these two values are read: the URI holds no
/// credentials, and the log shows nothing else of it.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Target {
    pub(super) key: Option<String>,
    pub(super) prefix: Option<String>,
}

#[allow(
    clippy::disallowed_methods,
    reason = "an OUTBOUND SDK request, decoded for a log line only; the rule is for client requests"
)]
pub(super) fn request_target(uri: &str) -> Target {
    let (path, query) = uri.split_once('?').unwrap_or((uri, ""));
    let key = path.strip_prefix('/').unwrap_or(path);
    let key = (!key.is_empty())
        .then(|| urlencoding::decode(key).map_or_else(|_| key.to_string(), |k| k.into_owned()));
    let prefix = serde_urlencoded::from_str::<Vec<(String, String)>>(query)
        .ok()
        .and_then(|pairs| pairs.into_iter().find(|(k, _)| k == "prefix"))
        .map(|(_, v)| v);
    Target { key, prefix }
}

/// `statuses` as text: `503,503,200`, `-` for an attempt without a response.
fn attempt_statuses(statuses: &[Option<u16>]) -> String {
    statuses
        .iter()
        .map(|s| s.map_or_else(|| "-".to_string(), |s| s.to_string()))
        .collect::<Vec<_>>()
        .join(",")
}

/// Seconds, rounded to the millisecond.
fn secs(d: Duration) -> f64 {
    (d.as_secs_f64() * 1000.0).round() / 1000.0
}

/// One operation, from the first hook until the config bag drops it.
#[derive(Debug)]
struct RequestClock {
    timer: RequestTimer,
    start: Instant,
    operation: Option<Metadata>,
    bucket: Option<String>,
    /// The serialized request URI (see [`request_target`]).
    uri: Option<String>,
    /// The length of the request body (0 when unknown).
    body_bytes: u64,
    /// The operation timeout.
    deadline: Option<Duration>,
    first_send: Option<Instant>,
    /// The response status of each attempt sent; `None`: no response.
    statuses: Vec<Option<u16>>,
    /// An attempt was sent and its `read_after_attempt` did not run yet.
    in_attempt: bool,
    /// When and how the SDK finished the operation.
    finished: Option<(Instant, End)>,
    /// `false`: the operation is not recorded.
    record: bool,
}

impl Storable for RequestClock {
    type Storer = StoreReplace<Self>;
}

impl RequestClock {
    fn start(timer: RequestTimer) -> Self {
        Self {
            timer,
            start: Instant::now(),
            operation: None,
            bucket: None,
            uri: None,
            body_bytes: 0,
            deadline: None,
            first_send: None,
            statuses: Vec::new(),
            in_attempt: false,
            finished: None,
            record: true,
        }
    }
}

impl Drop for RequestClock {
    fn drop(&mut self) {
        if !self.record {
            return;
        }
        let (at, end) = self.finished.unwrap_or_else(|| {
            let now = Instant::now();
            (now, unfinished_end(now - self.start, self.deadline))
        });
        let report = Report {
            backend: self.timer.label.get(),
            operation: self.operation.as_ref().map_or("unknown", |m| m.name()),
            bucket: self.bucket.as_deref().unwrap_or(""),
            uri: self.uri.as_deref().unwrap_or(""),
            body_bytes: self.body_bytes,
            total: at.saturating_duration_since(self.start),
            before_first_send: self.first_send.map(|t| t - self.start),
            statuses: &self.statuses,
            end,
        };
        report.record(self.timer.slow);
    }
}

/// What one operation records.
struct Report<'a> {
    backend: &'a str,
    operation: &'a str,
    bucket: &'a str,
    uri: &'a str,
    body_bytes: u64,
    total: Duration,
    before_first_send: Option<Duration>,
    statuses: &'a [Option<u16>],
    end: End,
}

impl Report<'_> {
    fn record(&self, slow: Option<Duration>) {
        BACKEND_REQUEST_DURATION
            .with_label_values(&[self.backend, self.operation, self.end.outcome()])
            .observe(self.total.as_secs_f64());
        if !self.statuses.is_empty() {
            BACKEND_REQUEST_ATTEMPTS
                .with_label_values(&[self.backend, self.operation])
                .inc_by(self.statuses.len() as u64);
        }
        if is_slow(self.total, slow, self.body_bytes) {
            self.warn();
        }
    }

    fn warn(&self) {
        let target = request_target(self.uri);
        let before_first_attempt = self.before_first_send.map_or_else(
            || "not sent".to_string(),
            |d| format!("{:.3}", d.as_secs_f64()),
        );
        let status = self
            .statuses
            .last()
            .copied()
            .flatten()
            .map_or_else(|| "none".to_string(), |s| s.to_string());
        tracing::warn!(
            backend = self.backend,
            operation = self.operation,
            bucket = self.bucket,
            key = target.key.as_deref().unwrap_or(""),
            prefix = target.prefix.as_deref().unwrap_or(""),
            body_bytes = self.body_bytes,
            total_secs = secs(self.total),
            attempts = self.statuses.len(),
            before_first_attempt_secs = %before_first_attempt,
            status = %status,
            attempt_statuses = %attempt_statuses(self.statuses),
            result = %self.end,
            "slow backend request"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackendConfig;
    use crate::storage::fake_s3;
    use crate::storage::{NativeEncryptionConfig, S3Backend};

    fn s3_config(endpoint: &str) -> BackendConfig {
        BackendConfig::S3 {
            endpoint: Some(endpoint.to_string()),
            region: "us-east-1".into(),
            force_path_style: true,
            access_key_id: Some("a".into()),
            secret_access_key: Some("b".into()),
            allow_local: true,
            session_token: None,
        }
    }

    /// A backend named `name` on a fresh FakeS3, built like the engine
    /// builds one. Each test uses its own name: the metrics are global.
    async fn named_backend(name: &str) -> (S3Backend, std::sync::Arc<fake_s3::FakeS3>) {
        let (endpoint, fake) = fake_s3::start().await;
        let config = s3_config(&endpoint);
        let s3 = S3Backend::new(&config, NativeEncryptionConfig::None)
            .await
            .unwrap()
            .with_health_name(name, &config);
        (s3, fake)
    }

    fn samples(backend: &str, operation: &str, outcome: &str) -> u64 {
        BACKEND_REQUEST_DURATION
            .with_label_values(&[backend, operation, outcome])
            .get_sample_count()
    }

    fn seconds(backend: &str, operation: &str, outcome: &str) -> f64 {
        BACKEND_REQUEST_DURATION
            .with_label_values(&[backend, operation, outcome])
            .get_sample_sum()
    }

    fn attempts(backend: &str, operation: &str) -> u64 {
        BACKEND_REQUEST_ATTEMPTS
            .with_label_values(&[backend, operation])
            .get()
    }

    #[test]
    fn request_target_reads_the_key_and_the_listing_prefix() {
        assert_eq!(
            request_target("/dir/a%20b%2Bc.zip?x-id=GetObject"),
            Target {
                key: Some("dir/a b+c.zip".into()),
                prefix: None
            }
        );
        assert_eq!(
            request_target("/?list-type=2&delimiter=%2F&prefix=v1%2Fbuild%20x%2F"),
            Target {
                key: None,
                prefix: Some("v1/build x/".into())
            }
        );
        assert_eq!(request_target("/?delete"), Target::default());
        assert_eq!(request_target(""), Target::default());
    }

    #[test]
    fn an_unfinished_request_is_a_timeout_only_at_its_deadline() {
        let s = Duration::from_secs;
        assert_eq!(unfinished_end(s(30), Some(s(30))), End::Timeout);
        assert_eq!(unfinished_end(s(31), Some(s(30))), End::Timeout);
        assert_eq!(unfinished_end(s(29), Some(s(30))), End::Cancelled);
        assert_eq!(unfinished_end(s(300), None), End::Cancelled);
    }

    #[test]
    fn the_outcome_counts_expected_answers_as_ok() {
        assert_eq!(End::Ok.outcome(), "ok");
        for s in [304, 404, 412] {
            assert_eq!(End::Status(s).outcome(), "ok", "{s}");
        }
        for s in [400, 403, 409, 429, 500, 503] {
            assert_eq!(End::Status(s).outcome(), "error", "{s}");
        }
        for end in [End::Timeout, End::Connection, End::Response, End::Other] {
            assert_eq!(end.outcome(), "error", "{end:?}");
        }
        assert_eq!(End::Cancelled.outcome(), "cancelled");
    }

    #[test]
    fn slow_means_at_or_above_the_threshold_plus_a_second_per_mib_of_body() {
        let ms = Duration::from_millis;
        assert!(is_slow(ms(5000), Some(ms(5000)), 0));
        assert!(!is_slow(ms(4999), Some(ms(5000)), 0));
        assert!(!is_slow(ms(600_000), None, 0), "0 turns the log off");
        let mib = 1024 * 1024;
        assert!(!is_slow(ms(14_999), Some(ms(5000)), 10 * mib));
        assert!(is_slow(ms(15_000), Some(ms(5000)), 10 * mib));
        assert!(
            !is_slow(ms(5_999), Some(ms(5000)), 1),
            "a partial MiB counts"
        );
    }

    /// Each operation is one sample under its backend's name, its SDK
    /// operation name and its outcome, and its duration includes the
    /// backend's delay. A 404 is an answer, not an error.
    #[tokio::test]
    async fn each_request_is_one_sample_with_its_backend_operation_and_outcome() {
        let (s3, fake) = named_backend("timing-labels").await;
        fake.set_delay_ms("GET", 50);
        s3.client
            .put_object()
            .bucket("b")
            .key("dir/a.zip")
            .body(aws_sdk_s3::primitives::ByteStream::from_static(b"x"))
            .send()
            .await
            .unwrap();
        s3.client
            .get_object()
            .bucket("b")
            .key("dir/a.zip")
            .send()
            .await
            .unwrap();
        s3.client
            .get_object()
            .bucket("b")
            .key("dir/missing.zip")
            .send()
            .await
            .unwrap_err();
        s3.client
            .list_objects_v2()
            .bucket("b")
            .prefix("dir/")
            .delimiter("/")
            .send()
            .await
            .unwrap();
        assert_eq!(samples("timing-labels", "PutObject", "ok"), 1);
        assert_eq!(samples("timing-labels", "GetObject", "ok"), 2);
        assert_eq!(samples("timing-labels", "GetObject", "error"), 0);
        assert_eq!(samples("timing-labels", "ListObjectsV2", "ok"), 1);
        assert!(
            seconds("timing-labels", "GetObject", "ok") >= 0.1,
            "two GETs of 50 ms each"
        );
        assert_eq!(attempts("timing-labels", "GetObject"), 2);
    }

    /// A 503 SlowDown that the SDK retries is one request of two attempts.
    #[tokio::test]
    async fn a_retried_503_is_one_request_of_two_attempts() {
        let (s3, fake) = named_backend("timing-retry").await;
        fake.fail("GET", "b/k", 503, "SlowDown", 1);
        s3.client
            .get_object()
            .bucket("b")
            .key("k")
            .send()
            .await
            .unwrap_err(); // 404 after the retry: the key does not exist
        assert_eq!(
            fake.requests()
                .iter()
                .filter(|r| r.starts_with("GET /b/k"))
                .count(),
            2,
            "the SDK retried the 503"
        );
        assert_eq!(samples("timing-retry", "GetObject", "ok"), 1);
        assert_eq!(attempts("timing-retry", "GetObject"), 2);
    }

    /// The operation timeout and a caller that gives up both drop the
    /// operation before the SDK's last hooks run. Both are still recorded:
    /// the first as an error, the second as cancelled.
    #[tokio::test]
    async fn a_request_that_times_out_or_is_dropped_is_still_recorded() {
        let (endpoint, fake) = fake_s3::start().await;
        fake.set_delay_ms("GET", 2_000);
        let config = s3_config(&endpoint);
        let partition = super::super::client::backend_retry_partition(&config);
        let timer = RequestTimer::new(BackendLabel::named("timing-unfinished"), None);
        let client =
            S3Backend::build_client_in(&config, Some(Duration::from_millis(200)), partition, timer)
                .await
                .unwrap();
        client
            .get_object()
            .bucket("b")
            .key("k")
            .send()
            .await
            .unwrap_err();
        assert_eq!(samples("timing-unfinished", "GetObject", "error"), 1);

        fake.set_delay_ms("LIST", 2_000);
        let dropped = tokio::time::timeout(
            Duration::from_millis(50),
            client.list_objects_v2().bucket("b").send(),
        );
        assert!(dropped.await.is_err(), "the caller gave up");
        assert_eq!(
            samples("timing-unfinished", "ListObjectsV2", "cancelled"),
            1
        );
    }

    /// A `fmt` subscriber that writes into a buffer.
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// An operation over the threshold logs one WARN line that says where
    /// the time went; a fast one logs nothing.
    #[tokio::test(flavor = "current_thread")]
    async fn a_slow_request_logs_one_line_that_explains_it() {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let (endpoint, fake) = fake_s3::start().await;
        let config = s3_config(&endpoint);
        let partition = super::super::client::backend_retry_partition(&config);
        let timer = RequestTimer::new(
            BackendLabel::named("timing-slow"),
            Some(Duration::from_millis(100)),
        );
        let client = S3Backend::build_client_in(&config, None, partition, timer)
            .await
            .unwrap();
        fake.set_delay_ms("LIST", 150);
        fake.set_delay_ms("PUT", 150);
        // An upload of 2 MiB gets 2 s more: 150 ms is not slow for it.
        client
            .put_object()
            .bucket("releases")
            .key("v1/app.zip")
            .body(aws_sdk_s3::primitives::ByteStream::from(vec![0u8; 2 << 20]))
            .send()
            .await
            .unwrap();
        client
            .list_objects_v2()
            .bucket("releases")
            .prefix("v1/build 7/")
            .delimiter("/")
            .send()
            .await
            .unwrap();
        client
            .get_object()
            .bucket("releases")
            .key("v1/fast.zip")
            .send()
            .await
            .unwrap_err();

        let text = String::from_utf8(captured.0.lock().clone()).unwrap();
        let lines: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("slow backend request"))
            .collect();
        assert_eq!(lines.len(), 1, "one line, for the slow LIST only:\n{text}");
        let line = lines[0];
        for field in [
            "WARN",
            "backend=\"timing-slow\"",
            "operation=\"ListObjectsV2\"",
            "bucket=\"releases\"",
            "key=\"\"",
            "prefix=\"v1/build 7/\"",
            "body_bytes=0",
            "attempts=1",
            "status=200",
            "attempt_statuses=200",
            "result=ok",
        ] {
            assert!(line.contains(field), "{field} missing in: {line}");
        }
        assert!(line.contains("total_secs=0.1"), "{line}");
        assert!(line.contains("before_first_attempt_secs=0.0"), "{line}");
        assert!(!line.contains("x-id"), "no other part of the URI: {line}");
    }

    /// The coordination client carries the timer, under the backend label
    /// `coordination`.
    #[tokio::test]
    async fn the_coordination_client_is_timed() {
        let (endpoint, _fake) = fake_s3::start().await;
        let client =
            crate::config_db_sync::ConfigDbSync::build_client_with(&s3_config(&endpoint), None)
                .await
                .unwrap();
        assert!(client
            .config()
            .interceptors()
            .any(|i| i.name() == "DgpRequestTimer"));
        let before = samples(COORDINATION, "GetObject", "ok");
        client
            .get_object()
            .bucket("timing-coordination")
            .key("k")
            .send()
            .await
            .unwrap_err();
        assert!(samples(COORDINATION, "GetObject", "ok") > before);
    }

    /// Fix the class: every S3 client of the server is built by
    /// [`RequestTimer::client`], so no backend request goes untimed. A
    /// client rebuilt from a built client's config (`.to_builder()`, the
    /// health probe client) keeps the timer.
    #[test]
    fn every_s3_client_is_built_by_the_request_timer() {
        let needles = ["Client::from_conf(", "s3::Client::new("];
        let mut offenders = Vec::new();
        for (file, text) in crate::source_scan::prod_sources("src") {
            if file == "src/storage/s3/timing.rs" {
                continue;
            }
            let lines = crate::source_scan::prod_lines(&text);
            for (i, (n, line)) in lines.iter().enumerate() {
                if line.trim_start().starts_with("//") || !needles.iter().any(|x| line.contains(x))
                {
                    continue;
                }
                let rebuilt = lines[i.saturating_sub(15)..i]
                    .iter()
                    .any(|(_, l)| l.contains(".to_builder()"));
                if !rebuilt {
                    offenders.push(format!("{file}:{n}: {}", line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "build S3 clients with RequestTimer::client (storage/s3/timing.rs):\n{}",
            offenders.join("\n")
        );
    }
}
