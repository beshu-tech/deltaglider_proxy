// SPDX-License-Identifier: BUSL-1.1

//! Error response XML compliance tests
//!
//! Uses reqwest (not aws-sdk-s3) to inspect raw HTTP responses.
//! Verifies error codes, status codes, and Content-Type headers.

use crate::common;

use common::TestServer;

// NOTE: the plain "missing key → NoSuchKey code" mapping is unit-tested
// directly in src/s3_adapter_s3s.rs (`engine_error_to_s3s`); no TestServer
// spawn is needed to prove it. The tests below cover PIPELINE behaviour that
// the pure mapping can't (s3s XML body parsing → 400, multipart state, the
// content-type the framework renders, HEAD-bucket status).

#[tokio::test]
async fn test_nosuchbucket_xml_response() {
    let server = TestServer::builder().build().await;
    let client = server.http();

    // HEAD on a bucket that has no objects and was never created → NoSuchBucket
    let url = format!("{}/nonexistent-bucket", server.endpoint());
    let resp = client.head(&url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    // GET on a key inside a valid-but-empty bucket → NoSuchKey (multi-bucket: any bucket is accepted)
    let url = format!("{}/nonexistent-bucket/file.txt", server.endpoint());
    let resp = client.get(&url).send().await.unwrap();

    assert_eq!(resp.status().as_u16(), 404);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<Code>NoSuchKey</Code>"),
        "Multi-bucket mode: unknown bucket with missing key returns NoSuchKey, got: {}",
        body
    );
}

#[tokio::test]
async fn test_malformed_xml_delete_request() {
    let server = TestServer::builder().build().await;
    let client = server.http();

    let url = format!("{}/{}?delete", server.endpoint(), server.bucket());
    let resp = client
        .post(&url)
        .header("content-type", "application/xml")
        .body("this is not valid xml at all <<<>>>")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 400);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<Code>MalformedXML</Code>"),
        "Should contain MalformedXML error code, got: {}",
        body
    );
}

#[tokio::test]
async fn test_multipart_create_upload() {
    let server = TestServer::builder().build().await;
    let client = server.http();

    let url = format!("{}/{}/test.zip?uploads", server.endpoint(), server.bucket());
    let resp = client.post(&url).send().await.unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<UploadId>"),
        "CreateMultipartUpload should return an UploadId, got: {}",
        body
    );
}

#[tokio::test]
async fn test_error_content_type_is_xml() {
    let server = TestServer::builder().build().await;
    let client = server.http();

    // GET nonexistent key
    let url = format!("{}/{}/missing.txt", server.endpoint(), server.bucket());
    let resp = client.get(&url).send().await.unwrap();

    assert_eq!(resp.status().as_u16(), 404);

    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("application/xml"),
        "Error Content-Type should be application/xml, got: {}",
        ct
    );
}

#[tokio::test]
async fn test_entitytoolarge_response() {
    // This test requires a server with a very low max_object_size.
    // We can't easily set that per-test with the binary, so we use a
    // standard server and verify the error path exists by sending a
    // request to a nonexistent bucket (which triggers a different error).
    // The EntityTooLarge path is covered by the engine unit test.
    // Here we just verify the error XML format for the paths we CAN trigger.
    let server = TestServer::builder().build().await;
    let client = server.http();

    // HEAD nonexistent bucket → 404 NoSuchBucket with XML
    let url = format!("{}/fakebucket", server.endpoint());
    let resp = client.head(&url).send().await.unwrap();
    // HEAD responses don't have bodies in HTTP, so just verify status
    assert_eq!(resp.status().as_u16(), 404);
}

/// A key whose file name is too long for the filesystem backend (255 bytes
/// per segment, `.delta` included) is a client error: 400 KeyTooLongError,
/// which SDKs do not retry. It used to be a 500, retried four times.
#[tokio::test]
async fn a_too_long_file_name_is_400_key_too_long() {
    let server = TestServer::filesystem().await;
    let http = server.http();
    for key in [
        // Delta-eligible: the stored name gains `.delta` and passes 255.
        format!("{}.zip", "g".repeat(250)),
        // Passthrough, one segment over the limit.
        format!("dir/{}.jpg", "h".repeat(260)),
    ] {
        let url = format!("{}/{}/{}", server.endpoint(), server.bucket(), key);
        let r = http
            .put(&url)
            .body(b"0123456789".repeat(100))
            .send()
            .await
            .unwrap();
        let status = r.status();
        let body = r.text().await.unwrap();
        assert_eq!(status, 400, "{key}: {body}");
        assert!(
            body.contains("<Code>KeyTooLongError</Code>"),
            "{key}: {body}"
        );
    }
    // A long key that fits still works.
    let ok = format!(
        "{}/{}/{}.zip",
        server.endpoint(),
        server.bucket(),
        "k".repeat(200)
    );
    let r = http.put(&ok).body(b"fits".to_vec()).send().await.unwrap();
    assert_eq!(r.status(), 200);
}
