// SPDX-License-Identifier: BUSL-1.1

//! `common::S3Http` signs hand-built raw requests so they pass SigV4 on an
//! auth-on server, keys with characters that need encoding included.

use crate::common::{self, TestServer};
use reqwest::StatusCode;

#[tokio::test]
async fn signed_raw_requests_pass_auth_and_unsigned_ones_do_not() {
    let (server, http) = common::signed_setup().await;
    let ep = server.endpoint();
    let b = server.bucket();
    for key in [
        "plain.txt",
        "dir/with%20space.txt",
        "uni-%C3%A9.bin",
        "plus%2Bsign.txt",
    ] {
        let url = format!("{ep}/{b}/{key}");
        let put = http
            .put(&url)
            .body(key.as_bytes().to_vec())
            .send()
            .await
            .unwrap();
        assert!(put.status().is_success(), "PUT {key}: {}", put.status());
        let got = http.get(&url).send().await.unwrap();
        assert_eq!(got.status(), StatusCode::OK, "GET {key}");
        assert_eq!(got.bytes().await.unwrap(), key.as_bytes());
    }
    let listed = common::list_objects_raw(&http, &ep, b, "prefix=dir%2F&delimiter=%2F").await;
    assert!(listed.contains("with space.txt"), "{listed}");
    // The same request unsigned is refused: auth really is on.
    let anon = reqwest::Client::new()
        .get(format!("{ep}/{b}/plain.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), StatusCode::FORBIDDEN);
    // Wrong secret: refused.
    let wrong = common::S3Http::signed(common::TEST_ACCESS_KEY, "not-the-secret")
        .get(format!("{ep}/{b}/plain.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn open_access_server_gets_an_unsigned_client() {
    let server = TestServer::builder().open_access().build().await;
    let http = server.http();
    let url = format!("{}/{}/k", server.endpoint(), server.bucket());
    assert!(http
        .put(&url)
        .body("x")
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
}

/// A proxy with one bucket on a dead backend: a request to it answers 503.
async fn server_with_a_dead_backend_bucket() -> TestServer {
    // The healthy disk keeps the boot alive: the boot probe refuses to start
    // a proxy whose only backend is dead.
    let good_dir = tempfile::tempdir().expect("tempdir").keep();
    TestServer::builder()
        .extra_yaml_storage_section(&format!(
            r#"
backends:
  - name: deadb2
    type: s3
    endpoint: "http://127.0.0.1:1"
    region: us-east-1
    access_key_id: x
    secret_access_key: y
    allow_local: true
  - name: gooddisk
    type: filesystem
    path: {}
buckets:
  doomed:
    backend: deadb2
"#,
            good_dir.display()
        ))
        .env("DGP_BOOT_BACKEND_PROBE", "enforce")
        .build()
        .await
}

async fn list_the_doomed_bucket(server: &TestServer) -> u16 {
    use aws_sdk_s3::error::ProvideErrorMetadata;
    let err = server
        .s3_client()
        .await
        .list_objects_v2()
        .bucket("doomed")
        .send()
        .await
        .expect_err("a dead backend cannot list");
    assert_eq!(err.code(), Some("ServiceUnavailable"), "{err:?}");
    503
}

/// F27: a 5xx the test did not declare fails it when the server drops. The
/// request itself "passed" (the test expects the error), so only the counter
/// can tell.
#[tokio::test]
#[should_panic(expected = "request(s) with a 5xx")]
async fn an_undeclared_5xx_fails_the_test_when_the_server_drops() {
    let server = server_with_a_dead_backend_bucket().await;
    list_the_doomed_bucket(&server).await;
}

#[tokio::test]
async fn a_declared_5xx_does_not_fail_the_test() {
    common::expect_server_errors("the test lists a bucket on a dead backend (503)");
    let server = server_with_a_dead_backend_bucket().await;
    list_the_doomed_bucket(&server).await;
}

/// A server that answered no 5xx (a 404 is a client error) drops cleanly.
#[tokio::test]
async fn a_server_with_only_client_errors_drops_cleanly() {
    let server = TestServer::builder().build().await;
    let missing = server
        .s3_client()
        .await
        .get_object()
        .bucket(server.bucket())
        .key("absent")
        .send()
        .await;
    assert!(missing.is_err());
}
