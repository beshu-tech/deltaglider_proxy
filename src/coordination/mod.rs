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
//!     writes. REPLICATION is wired through this today; the other subsystems
//!     are documented follow-ups.
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
pub use lease::{CoordinationLease, LeaseSubsystem, LocalLease};
pub use reference_lock::{ReferenceLock, S3ReferenceLock};
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
    if let Ok(id) = std::env::var("DGP_NODE_ID") {
        if !id.trim().is_empty() {
            return id;
        }
    }
    if let Ok(host) = std::env::var("HOSTNAME") {
        if !host.trim().is_empty() {
            return host;
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

#[cfg(test)]
mod test_s3 {
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
        /// Methods of the requests seen, in order.
        pub seen: Arc<Mutex<Vec<String>>>,
    }

    impl Canned {
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
            self.seen.lock().unwrap().push(method.clone());
            let mut resp = if method == "GET" {
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
