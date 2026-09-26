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
