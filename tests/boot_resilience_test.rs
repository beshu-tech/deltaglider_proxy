// SPDX-License-Identifier: BUSL-1.1

//! Boot resilience (epic #145, E7): a transient outage of something the proxy
//! only COORDINATES through must not stop the boot. No MinIO needed: the
//! "outage" is a local port that accepts connections and drops them.

use crate::common;

use common::TestServer;

/// A listener that accepts every connection and closes it at once: the shape
/// of an S3 endpoint that is up at the TCP level and dead above it.
fn dropping_endpoint() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            drop(stream);
        }
    });
    format!("http://{addr}")
}

/// F12: the coordination bucket cannot be probed (transport error). The
/// verdict is unknown, not non-CAS: the proxy must boot and answer
/// `/_/health`, with the boot probe on `warn` as the operator asked.
#[tokio::test]
async fn an_unreachable_coordination_bucket_does_not_stop_the_boot() {
    let endpoint = dropping_endpoint();
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .s3_endpoint(&endpoint)
        .config_sync_bucket("coordination")
        .env("DGP_BOOT_BACKEND_PROBE", "warn")
        .build()
        .await;
    let health = reqwest::get(format!("{}/_/health", server.endpoint()))
        .await
        .expect("health request");
    assert_eq!(health.status().as_u16(), 200);
}
