#![cfg(feature = "s3s-adapter")]

//! Parity tests for DeltaGlider-specific contracts while migrating to `s3s`.
//!
//! The broad `s3_compat_test` corpus verifies the `s3s` path directly. These
//! tests compare the legacy Axum adapter and the experimental `s3s` adapter
//! side-by-side for behavior that is easy to lose during a protocol-layer swap.

mod common;

use common::{generate_binary, put_object, TestServer};

async fn parity_servers() -> (TestServer, TestServer) {
    let legacy = TestServer::builder().bucket("parity-bucket").build().await;
    let s3s = TestServer::builder()
        .bucket("parity-bucket")
        .s3s_adapter()
        .build()
        .await;
    (legacy, s3s)
}

fn header_string(resp: &reqwest::Response, name: &str) -> String {
    resp.headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string()
}

#[tokio::test]
async fn s3s_open_mode_accepts_anonymous_dummy_credentials() {
    let server = TestServer::builder()
        .bucket("open-mode-bucket")
        .s3s_adapter()
        .build()
        .await;
    let client = server.s3_client_with_creds("anonymous", "anonymous").await;

    let buckets = client
        .list_buckets()
        .send()
        .await
        .expect("open mode should accept anonymous/anonymous dummy SDK credentials");

    assert!(
        buckets
            .buckets()
            .iter()
            .any(|bucket| bucket.name() == Some(server.bucket())),
        "test bucket should be visible in open mode"
    );
}

#[tokio::test]
async fn range_and_conditionals_match_legacy_adapter() {
    let (legacy, s3s) = parity_servers().await;
    let http = reqwest::Client::new();
    let data = generate_binary(1024, 77);

    for server in [&legacy, &s3s] {
        put_object(
            &http,
            &server.endpoint(),
            server.bucket(),
            "range.bin",
            data.clone(),
            "application/octet-stream",
        )
        .await;
    }

    let mut bodies = Vec::new();
    let mut etags = Vec::new();
    for server in [&legacy, &s3s] {
        let resp = http
            .get(format!(
                "{}/{}/range.bin",
                server.endpoint(),
                server.bucket()
            ))
            .header("range", "bytes=10-31")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 206);
        assert_eq!(header_string(&resp, "content-range"), "bytes 10-31/1024");
        assert_eq!(header_string(&resp, "content-length"), "22");
        etags.push(header_string(&resp, "etag"));
        bodies.push(resp.bytes().await.unwrap().to_vec());
    }
    assert_eq!(bodies[0], bodies[1]);

    for (server, etag) in [(&legacy, &etags[0]), (&s3s, &etags[1])] {
        let not_modified = http
            .get(format!(
                "{}/{}/range.bin",
                server.endpoint(),
                server.bucket()
            ))
            .header("if-none-match", etag)
            .send()
            .await
            .unwrap();
        assert_eq!(not_modified.status(), 304);
        assert!(not_modified.headers().contains_key("x-amz-request-id"));

        let precondition_failed = http
            .get(format!(
                "{}/{}/range.bin",
                server.endpoint(),
                server.bucket()
            ))
            .header("if-match", "\"definitely-not-the-etag\"")
            .send()
            .await
            .unwrap();
        assert_eq!(precondition_failed.status(), 412);
        assert!(precondition_failed
            .headers()
            .contains_key("x-amz-request-id"));
    }
}

#[tokio::test]
async fn copy_replace_metadata_matches_legacy_adapter() {
    let (legacy, s3s) = parity_servers().await;
    let http = reqwest::Client::new();

    for server in [&legacy, &s3s] {
        http.put(format!(
            "{}/{}/copy-src.txt",
            server.endpoint(),
            server.bucket()
        ))
        .header("content-type", "text/plain")
        .header("x-amz-meta-old-key", "old-value")
        .body("source data")
        .send()
        .await
        .unwrap();

        let copy = http
            .put(format!(
                "{}/{}/copy-dst.txt",
                server.endpoint(),
                server.bucket()
            ))
            .header(
                "x-amz-copy-source",
                format!("{}/copy-src.txt", server.bucket()),
            )
            .header("x-amz-metadata-directive", "REPLACE")
            .header("content-type", "application/json")
            .header("x-amz-meta-new-key", "new-value")
            .send()
            .await
            .unwrap();
        assert!(copy.status().is_success(), "copy failed: {}", copy.status());

        let get = http
            .get(format!(
                "{}/{}/copy-dst.txt",
                server.endpoint(),
                server.bucket()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(get.status(), 200);
        assert_eq!(header_string(&get, "content-type"), "application/json");
        assert_eq!(header_string(&get, "x-amz-meta-new-key"), "new-value");
        assert!(
            !get.headers().contains_key("x-amz-meta-old-key"),
            "REPLACE must drop source user metadata"
        );
    }
}

#[tokio::test]
async fn list_metadata_extension_matches_legacy_adapter() {
    let (legacy, s3s) = parity_servers().await;
    let http = reqwest::Client::new();

    for server in [&legacy, &s3s] {
        http.put(format!(
            "{}/{}/metadir/file.txt",
            server.endpoint(),
            server.bucket()
        ))
        .header("content-type", "text/plain")
        .header("x-amz-meta-custom-key", "custom-value")
        .body("hello metadata")
        .send()
        .await
        .unwrap();

        let with_metadata = http
            .get(format!(
                "{}/{}?list-type=2&metadata=true&prefix=metadir/",
                server.endpoint(),
                server.bucket()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(with_metadata.status(), 200);
        let body = with_metadata.text().await.unwrap();
        assert!(body.contains("<UserMetadata>"), "{body}");
        assert!(body.contains("x-amz-meta-user-custom-key"), "{body}");
        assert!(body.contains("custom-value"), "{body}");
        assert!(body.contains("x-amz-meta-dg-note"), "{body}");

        let without_metadata = http
            .get(format!(
                "{}/{}?list-type=2&prefix=metadir/",
                server.endpoint(),
                server.bucket()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(without_metadata.status(), 200);
        let body = without_metadata.text().await.unwrap();
        assert!(!body.contains("<UserMetadata>"), "{body}");
    }
}
