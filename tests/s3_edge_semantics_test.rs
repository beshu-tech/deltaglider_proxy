// SPDX-License-Identifier: BUSL-1.1

//! Wire seams of S3 edge semantics (review 4, s3surface): conditional
//! requests with the object's own `Last-Modified`, ranges on a HEAD and on an
//! empty object, `If-Range`, the 304 headers, and the filesystem backend's
//! file-versus-directory key space. The pure decisions behind them are unit
//! tests in `s3_adapter_s3s::tests`; these check what reaches the client.

use crate::common;

use common::{put_object, signed_setup};

fn header(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// A client that sends back the exact `Last-Modified` it received gets the
/// S3 answers: 304 for If-Modified-Since, 200 for If-Unmodified-Since, and a
/// conditional copy succeeds (s3surface-1).
#[tokio::test]
async fn own_last_modified_revalidates() {
    let (server, http) = signed_setup().await;
    let (endpoint, bucket) = (server.endpoint(), server.bucket().to_string());
    put_object(
        &http,
        &endpoint,
        &bucket,
        "lm.txt",
        b"data".to_vec(),
        "text/plain",
    )
    .await;
    let url = format!("{endpoint}/{bucket}/lm.txt");
    let head = http.head(&url).send().await.unwrap();
    let lm = header(&head, "last-modified").expect("Last-Modified");

    let resp = http
        .get(&url)
        .header("if-modified-since", &lm)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 304, "If-Modified-Since: {lm}");
    let resp = http
        .get(&url)
        .header("if-unmodified-since", &lm)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "If-Unmodified-Since: {lm}");
    let resp = http
        .head(&url)
        .header("if-unmodified-since", &lm)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "HEAD If-Unmodified-Since: {lm}"
    );

    let resp = http
        .put(format!("{endpoint}/{bucket}/lm-copy.txt"))
        .header("x-amz-copy-source", format!("{bucket}/lm.txt"))
        .header("x-amz-copy-source-if-unmodified-since", &lm)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "copy-source-if-unmodified-since: {lm}"
    );
}

/// A 304 carries `ETag` and `Last-Modified`, and no XML body headers
/// (s3surface-7).
#[tokio::test]
async fn not_modified_carries_validators() {
    let (server, http) = signed_setup().await;
    let (endpoint, bucket) = (server.endpoint(), server.bucket().to_string());
    put_object(
        &http,
        &endpoint,
        &bucket,
        "nm.txt",
        b"data".to_vec(),
        "text/plain",
    )
    .await;
    let url = format!("{endpoint}/{bucket}/nm.txt");
    let head = http.head(&url).send().await.unwrap();
    let etag = header(&head, "etag").unwrap();
    let lm = header(&head, "last-modified").unwrap();

    for (name, value) in [("if-none-match", &etag), ("if-modified-since", &lm)] {
        let resp = http.get(&url).header(name, value).send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 304, "{name}");
        assert_eq!(
            header(&resp, "etag").as_deref(),
            Some(etag.as_str()),
            "{name}"
        );
        assert_eq!(
            header(&resp, "last-modified").as_deref(),
            Some(lm.as_str()),
            "{name}"
        );
        assert_eq!(header(&resp, "content-type"), None, "{name}");
        assert!(resp.bytes().await.unwrap().is_empty());
    }
}

/// HEAD with `Range` answers 206 like S3; GET keeps its 206 (s3surface-6).
#[tokio::test]
async fn head_with_range_is_partial() {
    let (server, http) = signed_setup().await;
    let (endpoint, bucket) = (server.endpoint(), server.bucket().to_string());
    put_object(
        &http,
        &endpoint,
        &bucket,
        "r.txt",
        b"abcd".to_vec(),
        "text/plain",
    )
    .await;
    let url = format!("{endpoint}/{bucket}/r.txt");

    let resp = http
        .head(&url)
        .header("range", "bytes=1-")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 206);
    assert_eq!(
        header(&resp, "content-range").as_deref(),
        Some("bytes 1-3/4")
    );
    assert_eq!(header(&resp, "content-length").as_deref(), Some("3"));
    let resp = http
        .get(&url)
        .header("range", "bytes=1-")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 206);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"bcd");
}

/// A suffix range on an empty object is 416, never a 206 whose
/// `Content-Length` promises a byte it does not send (s3surface-3).
#[tokio::test]
async fn range_on_empty_object_is_unsatisfiable() {
    let (server, http) = signed_setup().await;
    let (endpoint, bucket) = (server.endpoint(), server.bucket().to_string());
    put_object(
        &http,
        &endpoint,
        &bucket,
        "empty.txt",
        Vec::new(),
        "text/plain",
    )
    .await;
    let url = format!("{endpoint}/{bucket}/empty.txt");
    let resp = http
        .get(&url)
        .header("range", "bytes=-5")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 416);
    let resp = http
        .head(&url)
        .header("range", "bytes=-5")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 416);
}

/// A resumed download whose `If-Range` names another version gets the whole
/// object with 200; a matching one keeps the 206 (s3surface-4).
#[tokio::test]
async fn stale_if_range_serves_the_whole_object() {
    let (server, http) = signed_setup().await;
    let (endpoint, bucket) = (server.endpoint(), server.bucket().to_string());
    put_object(
        &http,
        &endpoint,
        &bucket,
        "ir.txt",
        b"0123456789".to_vec(),
        "text/plain",
    )
    .await;
    let url = format!("{endpoint}/{bucket}/ir.txt");
    let etag = header(&http.head(&url).send().await.unwrap(), "etag").unwrap();

    let resp = http
        .get(&url)
        .header("range", "bytes=5-")
        .header("if-range", "\"deadbeef\"")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"0123456789");

    let resp = http
        .get(&url)
        .header("range", "bytes=5-")
        .header("if-range", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 206);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"56789");
}
