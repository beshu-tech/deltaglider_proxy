// SPDX-License-Identifier: BUSL-1.1

//! Cross-instance coordination primitives for the job plane.
//!
//! The job schedulers elect a single leader per rule via a TTL lease with
//! heartbeat renewal + steal-on-expiry. [`CoordinationLease`] is the seam that
//! lets the SAME scheduler code run against either backing store, selected once
//! at startup:
//!   - [`LocalLease`] — wraps the existing SQLite CAS (`config_db/job_store.rs`).
//!     Zero cross-node visibility, zero S3 traffic. The single-instance default
//!     (no coordination bucket). Subsystems still on this lease under
//!     multi-instance (lifecycle / maintenance / parity) can double-run — a
//!     peer never sees another node's SQLite lease.
//!   - [`S3Lease`] — the lease as a CAS'd object in the coordination bucket,
//!     visible to every node → real leader failover on dead-leader TTL lapse.
//!     Gated on the boot-validated coordination bucket supporting conditional
//!     writes. Only REPLICATION is wired through it; lifecycle, maintenance
//!     and parity stay node-local.
//!
//! The trait deliberately mirrors `job_store`'s two-predicate tiling: acquire/steal
//! on `expires_at < now` (strict), renew while `expires_at >= now` (non-strict), so
//! the exact expiry instant is never simultaneously renewable and stealable.

pub mod capability;
pub mod cas;
pub mod cas_probe;
pub mod health;
pub mod lease;
pub mod reference_lock;
pub mod s3_lease;
pub mod server_clock;

pub use capability::{BackendCapabilityCache, CapabilityVerdict, VerifiedVia};
pub use health::{BackendHealthCache, HealthVerdict};
pub use lease::{CoordinationLease, DynCoordinationLease, LeaseError, LeaseSubsystem, LocalLease};
pub use reference_lock::{DynReferenceLock, ReferenceLock, S3ReferenceLock};
pub use s3_lease::S3Lease;

/// A durable-per-node identity for lease ownership provenance + self-reclaim.
///
/// Unlike the ephemeral per-task `owner` uuid (regenerated every process/task),
/// this SURVIVES a restart, so a rebooted node recognises its own prior lease
/// object and reclaims it immediately rather than orphaning it for a full TTL
/// (edge case E7). Resolution order:
///  1. `DGP_NODE_ID` env (operator-pinned — best for stable fleets).
///  2. `HOSTNAME` env (container id / host name — stable across restarts of the
///     same container/pod).
///  3. A uuid generated once and persisted to `<dir>/node-id` next to the config
///     DB, re-read on subsequent boots.
///
/// Purely a provenance/self-reclaim label — NEVER a coordination decision (the
/// lease CAS is the only arbiter). `dir` is the config-DB directory.
pub fn durable_node_id(dir: &std::path::Path) -> String {
    durable_node_id_with(&crate::config::process_env, dir)
}

/// [`durable_node_id`] over an injected environment.
pub fn durable_node_id_with(env: crate::config::EnvLookup, dir: &std::path::Path) -> String {
    for var in ["DGP_NODE_ID", "HOSTNAME"] {
        if let Some(id) = crate::config::lookup_parse::<String>(env, var) {
            if !id.trim().is_empty() {
                return id;
            }
        }
    }
    let path = dir.join("node-id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let generated = format!("node-{}", uuid::Uuid::new_v4());
    // Best-effort persist; if the write fails we still return a valid (if
    // non-durable-this-boot) id rather than block startup.
    let _ = std::fs::write(&path, &generated);
    generated
}

/// This process's boot id, and the one this node's previous process
/// recorded. The lease writes `current` into its body and reclaims a live
/// lease only when it carries `previous` (a restart of this node), never a
/// lease of a live twin that shares the node id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootIds {
    pub current: String,
    pub previous: Option<String>,
}

/// The boot ids of this process. The first call reads `<dir>/boot-id` (the
/// previous process's id) and writes a fresh one; later calls return the
/// same pair, so every lease of the process shares one id. `dir` is the
/// config-DB directory.
pub fn process_boot_ids(dir: &std::path::Path) -> BootIds {
    static IDS: std::sync::OnceLock<BootIds> = std::sync::OnceLock::new();
    IDS.get_or_init(|| rotate_boot_id(dir)).clone()
}

/// Read the recorded boot id and replace it with a fresh one. A failed
/// write only costs the next boot its self-reclaim (its leases free at TTL).
fn rotate_boot_id(dir: &std::path::Path) -> BootIds {
    let path = dir.join("boot-id");
    let previous = std::fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let current = uuid::Uuid::new_v4().to_string();
    let _ = std::fs::write(&path, &current);
    BootIds { current, previous }
}

#[cfg(test)]
mod node_identity_tests {
    use super::*;

    #[test]
    fn node_id_resolution_order() {
        let dir = tempfile::tempdir().unwrap();
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        let both = env(&[("DGP_NODE_ID", "pinned"), ("HOSTNAME", "host")]);
        assert_eq!(durable_node_id_with(&both, dir.path()), "pinned");
        let blank = env(&[("DGP_NODE_ID", "  "), ("HOSTNAME", "host")]);
        assert_eq!(durable_node_id_with(&blank, dir.path()), "host");
        let none = env(&[]);
        let generated = durable_node_id_with(&none, dir.path());
        assert!(generated.starts_with("node-"));
        assert_eq!(
            durable_node_id_with(&none, dir.path()),
            generated,
            "persisted"
        );
    }

    #[test]
    fn each_boot_sees_the_one_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let first = rotate_boot_id(dir.path());
        assert_eq!(first.previous, None);
        let second = rotate_boot_id(dir.path());
        assert_eq!(second.previous.as_deref(), Some(first.current.as_str()));
        // A twin with its own data dir never sees this node's boot id.
        let twin_dir = tempfile::tempdir().unwrap();
        assert_eq!(rotate_boot_id(twin_dir.path()).previous, None);
    }
}

#[cfg(test)]
pub(crate) mod test_s3 {
    //! Test-only S3 client over a canned connector: a GET answers with a fixed
    //! body, `Date` and `Last-Modified`; every other request answers 200. The
    //! lock and lease tests use it to drive the real read path, including the
    //! response headers that a mock of the trait cannot show.

    use aws_sdk_s3::config::{BehaviorVersion, Region};
    use aws_smithy_runtime_api::client::http::{
        HttpClient, HttpConnector, HttpConnectorFuture, HttpConnectorSettings, SharedHttpConnector,
    };
    use aws_smithy_runtime_api::client::orchestrator::{HttpRequest, HttpResponse};
    use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
    use aws_smithy_types::body::SdkBody;
    use std::sync::{Arc, Mutex};

    /// HTTP date (IMF-fixdate) for a unix time.
    pub fn http_date(unix: i64) -> String {
        chrono::DateTime::from_timestamp(unix, 0)
            .unwrap()
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string()
    }

    #[derive(Debug, Clone)]
    pub struct Canned {
        pub body: Vec<u8>,
        pub date: i64,
        pub last_modified: i64,
        /// Methods of the requests seen, in order. A create-only PUT
        /// (`If-None-Match: *`) is `PUT-CREATE`: the canned object exists,
        /// so it answers 412.
        pub seen: Arc<Mutex<Vec<String>>>,
    }

    impl Canned {
        /// PUTs that replaced the object (steal, renew): create-only PUTs
        /// excluded, because they always fail here.
        pub fn puts(&self) -> usize {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|m| *m == "PUT")
                .count()
        }
    }

    impl HttpConnector for Canned {
        fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
            let method = request.method().to_string();
            let create_only = request.headers().get("if-none-match").is_some();
            let seen = if method == "PUT" && create_only {
                "PUT-CREATE".to_string()
            } else {
                method.clone()
            };
            self.seen.lock().unwrap().push(seen);
            let mut resp = if method == "PUT" && create_only {
                HttpResponse::new(
                    412.try_into().unwrap(),
                    SdkBody::from(
                        "<Error><Code>PreconditionFailed</Code>\
                         <Message>At least one of the pre-conditions you specified did not hold</Message></Error>",
                    ),
                )
            } else if method == "GET" {
                let mut r =
                    HttpResponse::new(200.try_into().unwrap(), SdkBody::from(self.body.clone()));
                r.headers_mut()
                    .insert("last-modified", http_date(self.last_modified));
                r.headers_mut().insert("content-type", "application/json");
                r
            } else {
                HttpResponse::new(200.try_into().unwrap(), SdkBody::empty())
            };
            resp.headers_mut().insert("date", http_date(self.date));
            resp.headers_mut().insert("etag", "\"e1\"");
            HttpConnectorFuture::ready(Ok(resp))
        }
    }

    impl HttpClient for Canned {
        fn http_connector(
            &self,
            _: &HttpConnectorSettings,
            _: &RuntimeComponents,
        ) -> SharedHttpConnector {
            SharedHttpConnector::new(self.clone())
        }
    }

    /// An in-memory bucket with real conditional-write semantics whose
    /// next PUTs can LOSE their response: the write lands, the client sees
    /// a connection reset, and the SDK sends the same PUT again, which
    /// answers 412 against our own write. Holds any number of keys.
    /// key -> (body, etag)
    pub type Objects = std::collections::HashMap<String, (Vec<u8>, String)>;

    #[derive(Debug, Clone, Default)]
    pub struct Lossy {
        pub objects: Arc<Mutex<Objects>>,
        writes: Arc<Mutex<u32>>,
        /// How many of the next successful PUTs lose their response.
        pub lose_puts: Arc<Mutex<u32>>,
        /// Methods of the requests seen, in order (`PUT 412` for a lost race).
        pub seen: Arc<Mutex<Vec<String>>>,
    }

    impl Lossy {
        /// The body of `key`, parsed as JSON.
        pub fn body(&self, key: &str) -> Option<serde_json::Value> {
            let objects = self.objects.lock().unwrap();
            objects
                .get(key)
                .map(|(b, _)| serde_json::from_slice(b).unwrap())
        }

        pub fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }

        fn status(code: u16, error: Option<&str>) -> HttpResponse {
            let body = error.map_or_else(SdkBody::empty, |c| {
                SdkBody::from(format!(
                    "<Error><Code>{c}</Code><Message>x</Message></Error>"
                ))
            });
            HttpResponse::new(code.try_into().unwrap(), body)
        }
    }

    impl HttpConnector for Lossy {
        fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
            let key = request
                .uri()
                .split('?')
                .next()
                .unwrap_or_default()
                .rsplit("/coord/")
                .next()
                .unwrap_or_default()
                .to_string();
            let mut objects = self.objects.lock().unwrap();
            let current = objects.get(&key).map(|(_, e)| e.clone());
            let if_match = request.headers().get("if-match").map(str::to_string);
            let if_none = request.headers().get("if-none-match").is_some();
            let failed = (if_none && current.is_some())
                || if_match
                    .as_ref()
                    .is_some_and(|m| Some(m) != current.as_ref());
            let method = request.method().to_string();
            let resp = match method.as_str() {
                "PUT" if failed => {
                    self.seen.lock().unwrap().push("PUT 412".into());
                    Self::status(412, Some("PreconditionFailed"))
                }
                "PUT" => {
                    self.seen.lock().unwrap().push("PUT".into());
                    let body = request.body().bytes().unwrap_or_default().to_vec();
                    let mut writes = self.writes.lock().unwrap();
                    *writes += 1;
                    let etag = format!("\"e{writes}\"");
                    objects.insert(key, (body, etag.clone()));
                    let mut lose = self.lose_puts.lock().unwrap();
                    if *lose > 0 {
                        *lose -= 1;
                        return HttpConnectorFuture::ready(Err(
                            aws_smithy_runtime_api::client::result::ConnectorError::io(Box::new(
                                std::io::Error::new(
                                    std::io::ErrorKind::ConnectionReset,
                                    "the response was lost",
                                ),
                            )),
                        ));
                    }
                    let mut r = Self::status(200, None);
                    r.headers_mut().insert("etag", etag);
                    r
                }
                "GET" => {
                    self.seen.lock().unwrap().push("GET".into());
                    match objects.get(&key) {
                        Some((body, etag)) => {
                            let mut r = HttpResponse::new(
                                200.try_into().unwrap(),
                                SdkBody::from(body.clone()),
                            );
                            r.headers_mut().insert("etag", etag.clone());
                            r
                        }
                        None => Self::status(404, Some("NoSuchKey")),
                    }
                }
                "DELETE" if failed => {
                    self.seen.lock().unwrap().push("DELETE 412".into());
                    Self::status(412, Some("PreconditionFailed"))
                }
                "DELETE" => {
                    self.seen.lock().unwrap().push("DELETE".into());
                    objects.remove(&key);
                    Self::status(204, None)
                }
                _ => Self::status(501, Some("NotImplemented")),
            };
            HttpConnectorFuture::ready(Ok(resp))
        }
    }

    impl HttpClient for Lossy {
        fn http_connector(
            &self,
            _: &HttpConnectorSettings,
            _: &RuntimeComponents,
        ) -> SharedHttpConnector {
            SharedHttpConnector::new(self.clone())
        }
    }

    /// A client over [`Lossy`], bucket `coord`, that retries a lost
    /// response at once (as the SDK does, without the backoff).
    pub fn lossy_client(lossy: &Lossy) -> aws_sdk_s3::Client {
        let conf = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .credentials_provider(aws_credential_types::Credentials::new(
                "k", "s", None, None, "test",
            ))
            .endpoint_url("http://127.0.0.1:1")
            .force_path_style(true)
            .http_client(lossy.clone())
            .retry_config(
                aws_sdk_s3::config::retry::RetryConfig::standard()
                    .with_max_attempts(3)
                    .with_initial_backoff(std::time::Duration::from_millis(1)),
            )
            .build();
        aws_sdk_s3::Client::from_conf(conf)
    }

    pub fn client(canned: &Canned) -> aws_sdk_s3::Client {
        let conf = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .credentials_provider(aws_credential_types::Credentials::new(
                "k", "s", None, None, "test",
            ))
            .endpoint_url("http://127.0.0.1:1")
            .force_path_style(true)
            .http_client(canned.clone())
            .build();
        aws_sdk_s3::Client::from_conf(conf)
    }
}
