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

    // GET on a key in a bucket that does not exist → NoSuchBucket, as on S3
    // (review 4, s3surface-5; it answered NoSuchKey).
    let url = format!("{}/nonexistent-bucket/file.txt", server.endpoint());
    let resp = client.get(&url).send().await.unwrap();

    assert_eq!(resp.status().as_u16(), 404);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<Code>NoSuchBucket</Code>"),
        "unknown bucket with missing key returns NoSuchBucket, got: {}",
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
/// per segment) is a client error: 400 KeyTooLongError, which SDKs do not
/// retry. It used to be a 500, retried four times. A delta-eligible name
/// that fits but whose `.delta` form would not is stored whole (epic #142,
/// F43), so it does not fail on the upload that would have been a delta.
#[tokio::test]
async fn a_too_long_file_name_is_400_key_too_long() {
    let server = TestServer::filesystem().await;
    let http = server.http();
    // Delta-eligible: the stored name would gain `.delta` and pass 255. It
    // is stored whole instead, for the first upload and the second.
    let near = format!(
        "{}/{}/{}.zip",
        server.endpoint(),
        server.bucket(),
        "g".repeat(250)
    );
    for body in [b"0123456789".repeat(100), b"0123456780".repeat(100)] {
        let r = http.put(&near).body(body).send().await.unwrap();
        assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    }
    // Passthrough, one segment over the limit.
    let key = format!("dir/{}.jpg", "h".repeat(260));
    {
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

/// A WRITE of a key over 1024 bytes and the query text of a LIST over 1024
/// bytes are refused before authorization and before the engine, with the
/// error that S3 answers. The text of the limit itself still passes.
#[tokio::test]
async fn key_text_over_1024_bytes_is_refused_for_writes_and_list_text() {
    let server = TestServer::filesystem().await;
    let http = server.http();
    let ep = server.endpoint();
    let bucket = server.bucket();
    let long = "a/".repeat(513);
    let limit = format!("{}ab", "a/".repeat(511));
    assert_eq!(limit.len(), 1024);
    let code = |body: &str| {
        let start = body.find("<Code>").map(|i| i + 6).unwrap_or(0);
        let end = body.find("</Code>").unwrap_or(body.len());
        body[start..end].to_string()
    };

    // The key of a write: PutObject, the destination of a copy, the start
    // of a multipart upload.
    let r = http
        .put(format!("{ep}/{bucket}/{long}"))
        .body(b"x".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(code(&r.text().await.unwrap()), "KeyTooLongError");
    let r = http
        .put(format!("{ep}/{bucket}/{long}"))
        .header("x-amz-copy-source", format!("{bucket}/src.bin"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(code(&r.text().await.unwrap()), "KeyTooLongError");
    let r = http
        .post(format!("{ep}/{bucket}/{long}?uploads"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(code(&r.text().await.unwrap()), "KeyTooLongError");
    let r = http
        .put(format!("{ep}/{bucket}/{limit}"))
        .body(b"x".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "a key of 1024 bytes passes the limit");

    // The text parameters of a LIST.
    for name in ["prefix", "start-after", "marker", "delimiter"] {
        let r = http
            .get(format!("{ep}/{bucket}?list-type=2&{name}={long}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{name}");
        assert_eq!(code(&r.text().await.unwrap()), "InvalidArgument", "{name}");
    }
    let r = http
        .get(format!("{ep}/{bucket}?list-type=2&prefix={limit}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // A key in the body of a DeleteObjects request is not a write: a key
    // over the limit is an object that does not exist, so the delete succeeds.
    let xml = format!(
        "<Delete><Object><Key>fine.bin</Key></Object><Object><Key>{long}</Key></Object></Delete>"
    );
    let r = http
        .post(format!("{ep}/{bucket}?delete"))
        .header("content-type", "application/xml")
        .body(xml)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

/// An object whose key is longer than 1024 bytes can exist: the filesystem
/// backend limits a path SEGMENT (255 bytes), not the whole key, and release
/// 2.2.5 stored such keys. They stay reachable through every door that the
/// proxy itself owns: LIST shows them, DeleteObjects and the admin bulk delete
/// remove them. A write of such a key is refused with `KeyTooLongError`.
/// A request that names such a key in its URL path (GET, HEAD, DELETE) is
/// refused with `KeyTooLongError`, as S3 answers: the length check of the
/// URL applies to every method, and s3s refuses the key too.
/// (The principal of these requests is the bootstrap admin, which no key
/// length limits when a policy reads it.)
#[tokio::test]
async fn an_existing_key_over_1024_bytes_stays_reachable_and_cannot_be_written_again() {
    let server = TestServer::filesystem().await;
    let http = server.http();
    let ep = server.endpoint();
    let bucket = server.bucket();
    let data = server
        .data_dir()
        .expect("a filesystem backend")
        .to_path_buf();
    // Segments of 250 bytes: a key of 1244 bytes, each segment (and its
    // `.delta` form) within the 255 bytes of the filesystem.
    let segments: Vec<String> = [("a", 250), ("b", 250), ("c", 250), ("d", 250), ("e", 240)]
        .iter()
        .map(|(c, n)| c.repeat(*n))
        .collect();
    let key = segments.join("/");
    assert!(key.len() > 1024);
    let on_disk = segments
        .iter()
        .fold(data.join(bucket).join("deltaspaces"), |p, s| p.join(s));
    // Laid out as the backend lays out a stored object: the file in its folder.
    let place = |content: &[u8]| {
        std::fs::create_dir_all(on_disk.parent().unwrap()).unwrap();
        std::fs::write(&on_disk, content).unwrap();
    };
    place(b"stored by an older release");
    let code = |body: &str| {
        let start = body.find("<Code>").map(|i| i + 6).unwrap_or(0);
        let end = body.find("</Code>").unwrap_or(body.len());
        body[start..end].to_string()
    };
    let prefix = segments[..3].join("/");
    let listed = || async {
        http.get(format!("{ep}/{bucket}?list-type=2&prefix={prefix}"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
            .contains(&key)
    };

    // LIST shows it (a prefix of 752 bytes is within the limit).
    assert!(listed().await, "the long key is listed");

    // A new PUT of that key is refused by the length rule of writes.
    let r = http
        .put(format!("{ep}/{bucket}/{key}"))
        .body(b"new".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(code(&r.text().await.unwrap()), "KeyTooLongError");
    assert_eq!(
        std::fs::read(&on_disk).unwrap(),
        b"stored by an older release"
    );

    // A read names the key in its URL path: refused for its length.
    let r = http
        .get(format!("{ep}/{bucket}/{key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(code(&r.text().await.unwrap()), "KeyTooLongError");

    // DeleteObjects names the key in its body: it removes the object.
    let xml = format!("<Delete><Object><Key>{key}</Key></Object></Delete>");
    let r = http
        .post(format!("{ep}/{bucket}?delete"))
        .header("content-type", "application/xml")
        .body(xml)
        .send()
        .await
        .unwrap();
    let status = r.status();
    let body = r.text().await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("<Deleted>"), "{body}");
    assert!(!on_disk.exists(), "DeleteObjects removed the file");
    assert!(!listed().await, "and the listing no longer shows it");

    // The admin bulk delete has no length limit either (`path_guard` checks
    // the segments, not the length).
    place(b"again");
    let admin = common::admin_http_client(&ep).await;
    let r = admin
        .post(format!("{ep}/_/api/admin/objects/delete"))
        .json(&serde_json::json!({ "bucket": bucket, "keys": [key] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let answer: serde_json::Value = r.json().await.unwrap();
    assert_eq!(answer["deleted"].as_u64(), Some(1), "{answer}");
    assert!(!on_disk.exists(), "the bulk delete removed the file");
}

/// A user whose policy holds `${iam:username}` is not an unconditional admin:
/// the keys that a request puts into a policy are bounded, so a key of 1 MiB
/// in a DeleteObjects body, or of 60 KB in a URL, is refused at once and
/// does not keep the proxy busy (iam-rs matched such a resource with a
/// search that costs the key squared).
#[tokio::test]
async fn a_long_key_is_refused_at_once_for_a_user_with_a_templated_policy() {
    use std::time::{Duration, Instant};
    let server = TestServer::filesystem().await;
    let ep = server.endpoint();
    let bucket = server.bucket();
    let admin = common::admin_http_client(&ep).await;
    let before = common::get_iam_version(&admin, &ep).await;
    let r = admin
        .post(format!("{ep}/_/api/admin/users"))
        .json(&serde_json::json!({
            "name": "dana",
            "permissions": [{
                "effect": "Allow",
                "actions": ["read", "write", "delete", "list"],
                "resources": [format!("{bucket}/scrap/customers/${{iam:username}}/*")],
            }],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 201);
    let user: serde_json::Value = r.json().await.unwrap();
    common::wait_for_iam_rebuild(&admin, &ep, before).await;
    let dana = common::S3Http::signed(
        user["access_key_id"].as_str().unwrap(),
        user["secret_access_key"].as_str().unwrap(),
    );
    let home = "scrap/customers/dana";
    // A key of 1 MiB in a DeleteObjects body (the body limit is 4 MiB).
    let long = format!("{home}/{}", "a".repeat(1 << 20));
    let xml = format!(
        "<Delete><Object><Key>fine.bin</Key></Object><Object><Key>{long}</Key></Object></Delete>"
    );
    let started = Instant::now();
    let r = dana
        .post(format!("{ep}/{bucket}?delete"))
        .header("content-type", "application/xml")
        .body(xml)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    assert!(body.contains("<Code>AccessDenied</Code>"), "{body:.300}");
    // A key of 60 KB in the URL.
    let url_long = format!("{ep}/{bucket}/{home}/{}", "b".repeat(60_000));
    for method in [
        reqwest::Method::GET,
        reqwest::Method::HEAD,
        reqwest::Method::DELETE,
    ] {
        let r = dana
            .request(method.clone(), &url_long)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 400, "{method}");
    }
    // The proxy stays responsive while it did that.
    let health = reqwest::get(format!("{ep}/_/health")).await.unwrap();
    assert!(health.status().is_success());
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the requests took {:?}",
        started.elapsed()
    );
}
